//! gRPC front-end (tonic). Documents travel as raw BSON (D13).

use std::pin::Pin;

use futures::{Stream, StreamExt};
use mr_proto::pb::admin_service_server::AdminService;
use mr_proto::pb::relations_service_server::RelationsService;
use mr_proto::{self as conv, pb};
use tonic::{Request, Response, Status};

use crate::AppState;
use crate::auth::Role;
use crate::errors::ApiError;

#[derive(Clone)]
pub struct Grpc {
    pub state: AppState,
}

impl Grpc {
    fn authorize<T>(&self, req: &Request<T>, need: Role) -> Result<(), Status> {
        let header = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        self.state
            .auth
            .check(header, need)
            .map_err(ApiError::from)?;
        Ok(())
    }
}

/// Split a chunk of encoded documents into messages of at most `max` bytes
/// (a single larger document still goes out alone).
fn split_chunk(docs: Vec<Vec<u8>>, max: usize) -> Vec<pb::QueryResponse> {
    let mut out = vec![];
    let mut cur = vec![];
    let mut size = 0;
    for d in docs {
        if !cur.is_empty() && size + d.len() > max {
            out.push(pb::QueryResponse {
                documents: std::mem::take(&mut cur),
            });
            size = 0;
        }
        size += d.len();
        cur.push(d);
    }
    if !cur.is_empty() {
        out.push(pb::QueryResponse { documents: cur });
    }
    out
}

type QueryStream = Pin<Box<dyn Stream<Item = Result<pb::QueryResponse, Status>> + Send>>;

#[tonic::async_trait]
impl RelationsService for Grpc {
    type QueryStream = QueryStream;

    async fn query(
        &self,
        req: Request<pb::QueryRequest>,
    ) -> Result<Response<Self::QueryStream>, Status> {
        self.authorize(&req, Role::Read)?;
        let q = req
            .into_inner()
            .query
            .ok_or_else(|| Status::invalid_argument("missing query"))?;
        let q = conv::query_from_pb(q).map_err(ApiError::from)?;
        let stream = self.state.engine.query(&q).await.map_err(ApiError::from)?;
        let max = self.state.max_grpc_message_bytes;
        let out = stream.flat_map(move |chunk| {
            let items: Vec<Result<pb::QueryResponse, Status>> = match chunk {
                Ok(docs) => split_chunk(docs.iter().map(conv::encode_doc).collect(), max)
                    .into_iter()
                    .map(Ok)
                    .collect(),
                Err(e) => vec![Err(ApiError::from(e).into())],
            };
            futures::stream::iter(items)
        });
        Ok(Response::new(Box::pin(out)))
    }

    async fn count(
        &self,
        req: Request<pb::CountRequest>,
    ) -> Result<Response<pb::CountResponse>, Status> {
        self.authorize(&req, Role::Read)?;
        let r = req.into_inner();
        let filter = r
            .filter
            .map(conv::expr_from_pb)
            .transpose()
            .map_err(ApiError::from)?;
        let count = self
            .state
            .engine
            .count(&r.collection, filter.as_ref())
            .await
            .map_err(ApiError::from)?;
        Ok(Response::new(pb::CountResponse { count }))
    }

    async fn explain(
        &self,
        req: Request<pb::QueryRequest>,
    ) -> Result<Response<pb::ExplainResponse>, Status> {
        self.authorize(&req, Role::Read)?;
        let q = req
            .into_inner()
            .query
            .ok_or_else(|| Status::invalid_argument("missing query"))?;
        let q = conv::query_from_pb(q).map_err(ApiError::from)?;
        let explain = self
            .state
            .engine
            .explain(&q)
            .await
            .map_err(ApiError::from)?;
        Ok(Response::new(pb::ExplainResponse {
            explain: conv::encode_doc(&explain),
        }))
    }

    async fn write(
        &self,
        req: Request<pb::WriteRequest>,
    ) -> Result<Response<pb::WriteResponse>, Status> {
        self.authorize(&req, Role::ReadWrite)?;
        let ops = req
            .into_inner()
            .ops
            .into_iter()
            .map(conv::write_op_from_pb)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ApiError::from)?;
        let result = self.state.engine.write(ops).await.map_err(ApiError::from)?;
        Ok(Response::new(conv::write_result_to_pb(&result)))
    }
}

#[tonic::async_trait]
impl AdminService for Grpc {
    async fn get_schema(
        &self,
        req: Request<pb::GetSchemaRequest>,
    ) -> Result<Response<pb::GetSchemaResponse>, Status> {
        self.authorize(&req, Role::Read)?;
        let catalog = self.state.engine.catalog();
        Ok(Response::new(pb::GetSchemaResponse {
            version: catalog.version(),
            schema: Some(conv::schema_to_pb(catalog.schema())),
        }))
    }

    async fn apply_schema(
        &self,
        req: Request<pb::ApplySchemaRequest>,
    ) -> Result<Response<pb::ApplySchemaResponse>, Status> {
        self.authorize(&req, Role::Admin)?;
        let r = req.into_inner();
        let schema = r
            .schema
            .ok_or_else(|| Status::invalid_argument("missing schema"))?;
        let schema = conv::schema_from_pb(schema).map_err(ApiError::from)?;
        let rep = self
            .state
            .engine
            .apply_schema(schema, r.dry_run)
            .await
            .map_err(ApiError::from)?;
        Ok(Response::new(pb::ApplySchemaResponse {
            version: rep.version,
            changed: rep.changed,
            dry_run: rep.dry_run,
            created_collections: rep.created_collections,
            created_indexes: rep.created_indexes,
            dropped_indexes: rep.dropped_indexes,
            validators_updated: rep.validators_updated,
        }))
    }

    async fn schema_history(
        &self,
        req: Request<pb::SchemaHistoryRequest>,
    ) -> Result<Response<pb::SchemaHistoryResponse>, Status> {
        self.authorize(&req, Role::Read)?;
        let limit = match req.into_inner().limit {
            0 => 20,
            n => n.min(1000),
        };
        let versions = self
            .state
            .engine
            .schema_history(limit as i64)
            .await
            .map_err(ApiError::from)?;
        Ok(Response::new(pb::SchemaHistoryResponse {
            versions: versions
                .into_iter()
                .map(|v| pb::SchemaVersion {
                    version: v.version,
                    applied_at_ms: v.applied_at.timestamp_millis(),
                })
                .collect(),
        }))
    }

    async fn rebuild_embeds(
        &self,
        req: Request<pb::RebuildEmbedsRequest>,
    ) -> Result<Response<pb::RebuildEmbedsResponse>, Status> {
        self.authorize(&req, Role::Admin)?;
        let updated = self
            .state
            .engine
            .rebuild_embeds()
            .await
            .map_err(ApiError::from)?;
        Ok(Response::new(pb::RebuildEmbedsResponse { updated }))
    }

    async fn server_info(
        &self,
        req: Request<pb::ServerInfoRequest>,
    ) -> Result<Response<pb::ServerInfoResponse>, Status> {
        self.authorize(&req, Role::Read)?;
        let t = self.state.engine.topology();
        Ok(Response::new(pb::ServerInfoResponse {
            server_version: env!("CARGO_PKG_VERSION").into(),
            mongodb_version: t.server_version.clone(),
            transactions: t.supports_transactions,
            schema_version: self.state.engine.catalog().version(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_size() {
        let docs = vec![vec![0; 40], vec![0; 40], vec![0; 40], vec![0; 200]];
        let parts = split_chunk(docs, 100);
        let sizes: Vec<usize> = parts.iter().map(|p| p.documents.len()).collect();
        assert_eq!(sizes, vec![2, 1, 1]);
    }
}
