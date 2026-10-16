//! HTTP/JSON gateway (axum). Request and response shapes mirror the proto
//! messages (snake_case); documents and values are relaxed Extended JSON.

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query as UrlQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bson::{Bson, Document};
use futures::StreamExt;
use mr_core::query::{Expr, Query};
use mr_core::schema::Schema;
use mr_core::write::WriteOp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Role;
use crate::errors::ApiError;

pub fn router(state: AppState) -> Router {
    let limit = state.max_body_bytes;
    Router::new()
        .route("/health", get(health))
        .route("/v1/query", post(query))
        .route("/v1/count", post(count))
        .route("/v1/explain", post(explain))
        .route("/v1/write", post(write))
        .route("/v1/schema", get(get_schema).put(apply_schema))
        .route("/v1/schema/history", get(schema_history))
        .route("/v1/admin/rebuild-embeds", post(rebuild_embeds))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

fn authorize(state: &AppState, headers: &HeaderMap, need: Role) -> Result<(), ApiError> {
    let h = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    state.auth.check(h, need)?;
    Ok(())
}

fn to_json(d: Document) -> Value {
    Bson::Document(d).into_relaxed_extjson()
}

/// Parse a JSON body into `T`, reporting errors as 400 with our error shape.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))
}

async fn health(State(s): State<AppState>) -> Response {
    let t = s.engine.topology();
    let mongo_ok = s.engine.ping().await.is_ok();
    let body = json!({
        "status": if mongo_ok { "ok" } else { "degraded" },
        "mongodb": { "reachable": mongo_ok, "version": t.server_version, "transactions": t.supports_transactions },
        "schema_version": s.engine.catalog().version(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    let status = if mongo_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

async fn query(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Read)?;
    let q: Query = parse(&body)?;
    let stream = s.engine.query(&q).await?;
    let ndjson = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/x-ndjson"));
    if ndjson {
        // Stream one JSON document per line; errors end the stream with an
        // error line (headers are already sent).
        let body = stream.flat_map(|chunk| {
            let lines: Vec<Result<bytes::Bytes, std::io::Error>> = match chunk {
                Ok(docs) => docs
                    .into_iter()
                    .map(|d| {
                        let mut line = serde_json::to_vec(&to_json(d)).unwrap_or_default();
                        line.push(b'\n');
                        Ok(bytes::Bytes::from(line))
                    })
                    .collect(),
                Err(e) => {
                    let mut line =
                        serde_json::to_vec(&json!({ "error": { "message": e.to_string() } }))
                            .unwrap_or_default();
                    line.push(b'\n');
                    vec![Ok(bytes::Bytes::from(line))]
                }
            };
            futures::stream::iter(lines)
        });
        return Ok((
            [(header::CONTENT_TYPE, "application/x-ndjson")],
            Body::from_stream(body),
        )
            .into_response());
    }
    let mut out = vec![];
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        out.extend(chunk?.into_iter().map(to_json));
    }
    Ok(Json(Value::Array(out)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CountBody {
    collection: String,
    #[serde(default)]
    filter: Option<Expr>,
}

async fn count(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Read)?;
    let b: CountBody = parse(&body)?;
    let n = s.engine.count(&b.collection, b.filter.as_ref()).await?;
    Ok(Json(json!({ "count": n })).into_response())
}

async fn explain(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Read)?;
    let q: Query = parse(&body)?;
    Ok(Json(to_json(s.engine.explain(&q).await?)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteBody {
    ops: Vec<WriteOp>,
}

async fn write(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::ReadWrite)?;
    let b: WriteBody = parse(&body)?;
    let r = s.engine.write(b.ops).await?;
    Ok(Json(serde_json::to_value(r).expect("serializable")).into_response())
}

async fn get_schema(State(s): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Read)?;
    let c = s.engine.catalog();
    Ok(Json(json!({ "version": c.version(), "schema": c.schema() })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    schema: Schema,
    #[serde(default)]
    dry_run: bool,
}

async fn apply_schema(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Admin)?;
    let b: ApplyBody = parse(&body)?;
    let rep = s.engine.apply_schema(b.schema, b.dry_run).await?;
    Ok(Json(serde_json::to_value(rep).expect("serializable")).into_response())
}

#[derive(Deserialize)]
struct HistoryParams {
    limit: Option<u32>,
}

async fn schema_history(
    State(s): State<AppState>,
    headers: HeaderMap,
    UrlQuery(p): UrlQuery<HistoryParams>,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Read)?;
    let limit = p.limit.unwrap_or(20).clamp(1, 1000);
    let versions = s.engine.schema_history(limit as i64).await?;
    let out: Vec<Value> = versions
        .into_iter()
        .map(|v| json!({ "version": v.version, "applied_at": v.applied_at.try_to_rfc3339_string().unwrap_or_default() }))
        .collect();
    Ok(Json(json!({ "versions": out })).into_response())
}

async fn rebuild_embeds(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&s, &headers, Role::Admin)?;
    let updated = s.engine.rebuild_embeds().await?;
    Ok(Json(json!({ "updated": updated })).into_response())
}
