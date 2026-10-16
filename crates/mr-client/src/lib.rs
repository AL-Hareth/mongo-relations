//! Rust client for mongo-relations.
//!
//! ```no_run
//! use mr_client::{Client, field, rel, any};
//!
//! # async fn demo() -> mr_client::Result<()> {
//! let client = Client::connect("http://127.0.0.1:50051").await?.with_api_key("secret");
//!
//! // Posts by US authors that have a comment mentioning "rust", newest first,
//! // with the author's name and the three latest comments.
//! let posts = client
//!     .query("posts")
//!     .filter(field("author.country").eq("US") & any("comments", field("body").regex("rust")))
//!     .include(rel("author").select(["name"]))
//!     .include(rel("comments").sort_desc("created_at").limit(3))
//!     .sort_desc("created_at")
//!     .limit(20)
//!     .fetch_all()
//!     .await?;
//! # Ok(()) }
//! ```

use std::pin::Pin;

use bson::{Bson, Document};
use futures::{Stream, StreamExt, TryStreamExt};
use mr_proto::pb::admin_service_client::AdminServiceClient;
use mr_proto::pb::relations_service_client::RelationsServiceClient;
use mr_proto::{self as conv, pb};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};

pub use mr_core::query::{CmpOp, Expr, Include, JoinStrategy, Query, SortKey};
pub use mr_core::schema;
pub use mr_core::write::{WriteOp, WriteResult};
pub use mr_proto::pb::{ApplySchemaResponse as ApplyReport, ServerInfoResponse as ServerInfo};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// The server rejected the request. `kind` is the server's error class
    /// (`invalid_argument`, `failed_precondition`, ...).
    #[error("{code:?}: {message}")]
    Server {
        code: tonic::Code,
        kind: Option<String>,
        message: String,
    },
    #[error("{0}")]
    Decode(#[from] mr_proto::ConvertError),
    #[error("invalid API key (not a valid header value)")]
    InvalidApiKey,
}

impl From<tonic::Status> for Error {
    fn from(s: tonic::Status) -> Self {
        let kind = s
            .metadata()
            .get("mr-error-kind")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        Error::Server {
            code: s.code(),
            kind,
            message: s.message().to_string(),
        }
    }
}

impl Error {
    /// The server's error class, e.g. `failed_precondition` for FK violations.
    pub fn kind(&self) -> Option<&str> {
        match self {
            Error::Server { kind, .. } => kind.as_deref(),
            _ => None,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A connection to a mongo-relations server. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    relations: RelationsServiceClient<Channel>,
    admin: AdminServiceClient<Channel>,
    auth: Option<MetadataValue<tonic::metadata::Ascii>>,
}

impl Client {
    /// Connect to a gRPC endpoint such as `http://127.0.0.1:50051`.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let channel = Endpoint::from_shared(endpoint.into())?.connect().await?;
        Ok(Self::from_channel(channel))
    }

    pub fn from_channel(channel: Channel) -> Self {
        let max = 64 * 1024 * 1024;
        Self {
            relations: RelationsServiceClient::new(channel.clone()).max_decoding_message_size(max),
            admin: AdminServiceClient::new(channel).max_decoding_message_size(max),
            auth: None,
        }
    }

    /// Authenticate every request with this API key.
    pub fn with_api_key(mut self, key: &str) -> Self {
        self.auth = format!("Bearer {key}").parse().ok();
        self
    }

    fn request<T>(&self, msg: T) -> Result<tonic::Request<T>> {
        let mut req = tonic::Request::new(msg);
        if let Some(a) = &self.auth {
            req.metadata_mut().insert("authorization", a.clone());
        }
        Ok(req)
    }

    /// Start building a query on `collection`.
    pub fn query(&self, collection: impl Into<String>) -> QueryBuilder<'_> {
        QueryBuilder {
            client: self,
            query: Query::new(collection),
        }
    }

    /// Run a prepared query and stream documents.
    pub async fn run(&self, query: &Query) -> Result<DocumentStream> {
        let req = self.request(pb::QueryRequest {
            query: Some(conv::query_to_pb(query)),
        })?;
        let stream = self.relations.clone().query(req).await?.into_inner();
        let docs = stream
            .map_err(Error::from)
            .map_ok(|chunk| {
                futures::stream::iter(
                    chunk
                        .documents
                        .into_iter()
                        .map(|b| conv::decode_doc(&b).map_err(Error::from)),
                )
            })
            .try_flatten();
        Ok(Box::pin(docs))
    }

    pub async fn count(&self, collection: impl Into<String>, filter: Option<Expr>) -> Result<u64> {
        let req = self.request(pb::CountRequest {
            collection: collection.into(),
            filter: filter.as_ref().map(conv::expr_to_pb),
        })?;
        Ok(self.relations.clone().count(req).await?.into_inner().count)
    }

    pub async fn explain(&self, query: &Query) -> Result<Document> {
        let req = self.request(pb::QueryRequest {
            query: Some(conv::query_to_pb(query)),
        })?;
        let res = self.relations.clone().explain(req).await?.into_inner();
        Ok(conv::decode_doc(&res.explain)?)
    }

    /// Execute write operations atomically.
    pub async fn write(&self, ops: Vec<WriteOp>) -> Result<WriteResult> {
        let req = self.request(pb::WriteRequest {
            ops: ops.iter().map(conv::write_op_to_pb).collect(),
        })?;
        let res = self.relations.clone().write(req).await?.into_inner();
        Ok(conv::write_result_from_pb(res)?)
    }

    pub async fn insert(
        &self,
        collection: impl Into<String>,
        documents: Vec<Document>,
    ) -> Result<WriteResult> {
        self.write(vec![WriteOp::Insert {
            collection: collection.into(),
            documents,
        }])
        .await
    }

    /// Update every document matching `filter` (all documents if `None`).
    pub async fn update(
        &self,
        collection: impl Into<String>,
        filter: Option<Expr>,
        update: Document,
    ) -> Result<WriteResult> {
        self.write(vec![WriteOp::Update {
            collection: collection.into(),
            filter,
            update,
            multi: true,
        }])
        .await
    }

    /// Delete every document matching `filter` (all documents if `None`).
    pub async fn delete(
        &self,
        collection: impl Into<String>,
        filter: Option<Expr>,
    ) -> Result<WriteResult> {
        self.write(vec![WriteOp::Delete {
            collection: collection.into(),
            filter,
            multi: true,
        }])
        .await
    }

    /// The current schema and its version.
    pub async fn schema(&self) -> Result<(u64, schema::Schema)> {
        let res = self
            .admin
            .clone()
            .get_schema(self.request(pb::GetSchemaRequest {})?)
            .await?
            .into_inner();
        Ok((
            res.version,
            conv::schema_from_pb(res.schema.unwrap_or_default())?,
        ))
    }

    /// Apply a schema declaratively (requires an admin key).
    pub async fn apply_schema(
        &self,
        schema: &schema::Schema,
        dry_run: bool,
    ) -> Result<ApplyReport> {
        let req = self.request(pb::ApplySchemaRequest {
            schema: Some(conv::schema_to_pb(schema)),
            dry_run,
        })?;
        Ok(self.admin.clone().apply_schema(req).await?.into_inner())
    }

    pub async fn rebuild_embeds(&self) -> Result<u64> {
        let req = self.request(pb::RebuildEmbedsRequest {})?;
        Ok(self
            .admin
            .clone()
            .rebuild_embeds(req)
            .await?
            .into_inner()
            .updated)
    }

    pub async fn server_info(&self) -> Result<ServerInfo> {
        Ok(self
            .admin
            .clone()
            .server_info(self.request(pb::ServerInfoRequest {})?)
            .await?
            .into_inner())
    }
}

/// A stream of result documents.
pub type DocumentStream = Pin<Box<dyn Stream<Item = Result<Document>> + Send>>;

/// Builder for a [`Query`].
#[must_use]
pub struct QueryBuilder<'a> {
    client: &'a Client,
    query: Query,
}

impl QueryBuilder<'_> {
    /// Add a filter; several calls are combined with AND.
    pub fn filter(mut self, e: Expr) -> Self {
        self.query.filter = Some(match self.query.filter.take() {
            Some(prev) => prev & e,
            None => e,
        });
        self
    }

    pub fn select<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.query.select = Some(fields.into_iter().map(Into::into).collect());
        self
    }

    pub fn include(mut self, inc: impl Into<Include>) -> Self {
        self.query.include.push(inc.into());
        self
    }

    pub fn sort_asc(mut self, field: impl Into<String>) -> Self {
        self.query.sort.push(SortKey::asc(field));
        self
    }

    pub fn sort_desc(mut self, field: impl Into<String>) -> Self {
        self.query.sort.push(SortKey::desc(field));
        self
    }

    pub fn skip(mut self, n: u64) -> Self {
        self.query.skip = Some(n);
        self
    }

    pub fn limit(mut self, n: u64) -> Self {
        self.query.limit = Some(n);
        self
    }

    /// The query IR this builder produces.
    pub fn build(self) -> Query {
        self.query
    }

    pub async fn stream(self) -> Result<DocumentStream> {
        self.client.run(&self.query).await
    }

    pub async fn fetch_all(self) -> Result<Vec<Document>> {
        self.client.run(&self.query).await?.try_collect().await
    }

    pub async fn first(mut self) -> Result<Option<Document>> {
        self.query.limit = Some(1);
        let mut s = self.client.run(&self.query).await?;
        s.next().await.transpose()
    }

    pub async fn count(self) -> Result<u64> {
        self.client
            .count(self.query.collection, self.query.filter)
            .await
    }

    pub async fn explain(self) -> Result<Document> {
        self.client.explain(&self.query).await
    }
}

/// Start an include (join) of `relation`.
pub fn rel(relation: impl Into<String>) -> IncludeBuilder {
    IncludeBuilder(Include::new(relation))
}

/// Builder for an [`Include`].
#[must_use]
pub struct IncludeBuilder(Include);

impl IncludeBuilder {
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.0.alias = Some(alias.into());
        self
    }

    pub fn filter(mut self, e: Expr) -> Self {
        self.0.filter = Some(match self.0.filter.take() {
            Some(prev) => prev & e,
            None => e,
        });
        self
    }

    pub fn select<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.0.select = Some(fields.into_iter().map(Into::into).collect());
        self
    }

    pub fn sort_asc(mut self, field: impl Into<String>) -> Self {
        self.0.sort.push(SortKey::asc(field));
        self
    }

    pub fn sort_desc(mut self, field: impl Into<String>) -> Self {
        self.0.sort.push(SortKey::desc(field));
        self
    }

    /// Per-parent limit (to-many relations).
    pub fn limit(mut self, n: u64) -> Self {
        self.0.limit = Some(n);
        self
    }

    /// Inner-join semantics: drop parents without a match.
    pub fn required(mut self) -> Self {
        self.0.required = true;
        self
    }

    pub fn include(mut self, inc: impl Into<Include>) -> Self {
        self.0.include.push(inc.into());
        self
    }

    pub fn strategy(mut self, s: JoinStrategy) -> Self {
        self.0.strategy = Some(s);
        self
    }
}

impl From<IncludeBuilder> for Include {
    fn from(b: IncludeBuilder) -> Include {
        b.0
    }
}

/// A field path for building filters. Paths may cross to-one relations:
/// `field("author.country")`.
pub fn field(path: impl Into<String>) -> Field {
    Field(path.into())
}

pub struct Field(String);

impl Field {
    pub fn eq(self, v: impl Into<Bson>) -> Expr {
        Expr::eq(self.0, v)
    }
    pub fn ne(self, v: impl Into<Bson>) -> Expr {
        Expr::ne(self.0, v)
    }
    pub fn gt(self, v: impl Into<Bson>) -> Expr {
        Expr::gt(self.0, v)
    }
    pub fn gte(self, v: impl Into<Bson>) -> Expr {
        Expr::gte(self.0, v)
    }
    pub fn lt(self, v: impl Into<Bson>) -> Expr {
        Expr::lt(self.0, v)
    }
    pub fn lte(self, v: impl Into<Bson>) -> Expr {
        Expr::lte(self.0, v)
    }
    pub fn is_in<V: Into<Bson>>(self, values: impl IntoIterator<Item = V>) -> Expr {
        Expr::is_in(self.0, values)
    }
    pub fn not_in<V: Into<Bson>>(self, values: impl IntoIterator<Item = V>) -> Expr {
        Expr::Nin {
            path: self.0,
            values: values.into_iter().map(Into::into).collect(),
        }
    }
    pub fn exists(self) -> Expr {
        Expr::Exists {
            path: self.0,
            exists: true,
        }
    }
    pub fn missing(self) -> Expr {
        Expr::Exists {
            path: self.0,
            exists: false,
        }
    }
    pub fn is_null(self) -> Expr {
        Expr::IsNull { path: self.0 }
    }
    pub fn regex(self, pattern: impl Into<String>) -> Expr {
        Expr::Regex {
            path: self.0,
            pattern: pattern.into(),
            options: String::new(),
        }
    }
    pub fn regex_i(self, pattern: impl Into<String>) -> Expr {
        Expr::Regex {
            path: self.0,
            pattern: pattern.into(),
            options: "i".into(),
        }
    }
}

/// At least one related document matches (SQL `EXISTS`).
pub fn any(relation: impl Into<String>, filter: Expr) -> Expr {
    Expr::any(relation, Some(filter))
}

/// At least one related document exists.
pub fn has(relation: impl Into<String>) -> Expr {
    Expr::any(relation, None)
}

/// Every related document matches.
pub fn all(relation: impl Into<String>, filter: Expr) -> Expr {
    Expr::all(relation, filter)
}

/// A raw MongoDB filter on the current collection.
pub fn raw(filter: Document) -> Expr {
    Expr::Raw(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_produce_ir() {
        let inc: Include = rel("comments")
            .filter(field("ok").eq(true))
            .sort_desc("n")
            .limit(3)
            .required()
            .into();
        assert_eq!(inc.limit, Some(3));
        assert!(inc.required);
        let e = field("a").eq(1) & (field("b").gt(2) | !has("c"));
        assert!(matches!(e, Expr::And(ref v) if v.len() == 2));
    }
}
