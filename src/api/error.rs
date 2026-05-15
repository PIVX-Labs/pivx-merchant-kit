//! API error type. Implements `IntoResponse` so handlers can `?` through
//! `Result<_, ApiError>` and get sensible JSON error bodies for free.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    NotFound,
    BadRequest(String),
    Conflict(String),
    Internal(crate::error::Error),
}

impl ApiError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Unauthorized => "invalid or missing bearer token".into(),
            Self::NotFound => "resource not found".into(),
            Self::BadRequest(m) => m.clone(),
            Self::Conflict(m) => m.clone(),
            // Internal errors don't leak details to the client — log
            // server-side, return a generic message.
            Self::Internal(e) => {
                tracing::error!(err = %e, "api: internal error");
                "internal server error".into()
            }
        }
    }
}

impl From<crate::error::Error> for ApiError {
    fn from(e: crate::error::Error) -> Self {
        Self::Internal(e)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = ErrorBody {
            error: self.message(),
            code: code.into(),
        };
        (status, Json(body)).into_response()
    }
}
