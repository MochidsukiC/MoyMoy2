//! API error type. Infrastructure failures (DB, pool, blocking-join) become a
//! `500` with a JSON `{ok:false,error,detail}` body; domain outcomes that are not
//! faults (insufficient balance, unknown recipient) are returned as a `200`
//! `{ok:false,error,...}` body from the handler itself, never as an `ApiError`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// An infrastructure-level failure surfaced to the client as JSON.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub detail: String,
}

impl ApiError {
    pub fn internal(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            detail: detail.into(),
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            detail: detail.into(),
        }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            detail: detail.into(),
        }
    }

    /// A missing/invalid/expired session — the `AuthedAccount` extractor's
    /// rejection. The app treats `401` as "log in again".
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log server faults loudly (never swallow); the client gets a terse body.
        if self.status.is_server_error() {
            tracing::error!(code = self.code, detail = %self.detail, "request failed");
        }
        let body = Json(json!({ "ok": false, "error": self.code, "detail": self.detail }));
        (self.status, body).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for ApiError {}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(format!("sqlite: {e}"))
    }
}
impl From<r2d2::Error> for ApiError {
    fn from(e: r2d2::Error) -> Self {
        ApiError::internal(format!("db pool: {e}"))
    }
}
impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> Self {
        ApiError::internal(format!("blocking task: {e}"))
    }
}
