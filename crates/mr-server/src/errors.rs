//! Mapping engine and auth errors to gRPC statuses and HTTP responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mr_engine::{Error as EngineError, ErrorKind};
use serde_json::json;
use tonic::{Code, Status};

use crate::auth::AuthError;

/// One error type for both front-ends.
#[derive(Debug)]
pub enum ApiError {
    Auth(AuthError),
    Engine(EngineError),
    BadRequest(String),
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        ApiError::Engine(e)
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        ApiError::Auth(e)
    }
}

impl From<mr_proto::ConvertError> for ApiError {
    fn from(e: mr_proto::ConvertError) -> Self {
        ApiError::BadRequest(e.0)
    }
}

impl ApiError {
    fn parts(&self) -> (&'static str, Code, StatusCode, String) {
        match self {
            ApiError::Auth(AuthError::Unauthenticated) => (
                "unauthenticated",
                Code::Unauthenticated,
                StatusCode::UNAUTHORIZED,
                "missing or invalid API key".into(),
            ),
            ApiError::Auth(AuthError::Forbidden { need }) => (
                "permission_denied",
                Code::PermissionDenied,
                StatusCode::FORBIDDEN,
                format!("this API key lacks the `{need:?}` role"),
            ),
            ApiError::BadRequest(m) => (
                "invalid_argument",
                Code::InvalidArgument,
                StatusCode::BAD_REQUEST,
                m.clone(),
            ),
            ApiError::Engine(e) => {
                let (kind, code, status) = match e.kind() {
                    ErrorKind::InvalidArgument => (
                        "invalid_argument",
                        Code::InvalidArgument,
                        StatusCode::BAD_REQUEST,
                    ),
                    ErrorKind::FailedPrecondition => (
                        "failed_precondition",
                        Code::FailedPrecondition,
                        StatusCode::CONFLICT,
                    ),
                    ErrorKind::Aborted => ("aborted", Code::Aborted, StatusCode::CONFLICT),
                    ErrorKind::NotFound => ("not_found", Code::NotFound, StatusCode::NOT_FOUND),
                    ErrorKind::Unavailable => (
                        "unavailable",
                        Code::Unavailable,
                        StatusCode::SERVICE_UNAVAILABLE,
                    ),
                    ErrorKind::Internal => (
                        "internal",
                        Code::Internal,
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                };
                let message = if e.kind() == ErrorKind::Internal {
                    tracing::error!(error = %e, "internal error");
                    "internal error".to_string()
                } else {
                    e.to_string()
                };
                (kind, code, status, message)
            }
        }
    }
}

impl From<ApiError> for Status {
    fn from(e: ApiError) -> Self {
        let (kind, code, _, message) = e.parts();
        let mut s = Status::new(code, message);
        if let Ok(v) = kind.parse() {
            s.metadata_mut().insert("mr-error-kind", v);
        }
        s
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (kind, _, status, message) = self.parts();
        (
            status,
            Json(json!({ "error": { "kind": kind, "message": message } })),
        )
            .into_response()
    }
}
