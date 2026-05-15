//! Bearer-token auth middleware. Single shared token from config;
//! constant-time compared.

use crate::api::error::ApiError;
use crate::sync::SyncState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

/// Tower middleware: rejects requests without a valid `Authorization:
/// Bearer <token>` header. Mounted at the router layer; `/healthz` is
/// outside its scope and stays open.
pub async fn require_bearer(
    State(state): State<Arc<SyncState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;

    let presented = header
        .strip_prefix("Bearer ")
        .ok_or(ApiError::Unauthorized)?
        .trim();

    if !constant_time_eq(presented.as_bytes(), state.config.api.auth_token.as_bytes()) {
        return Err(ApiError::Unauthorized);
    }
    Ok(next.run(request).await)
}

/// Constant-time byte-slice equality. Hand-rolled because pulling in
/// `subtle` for a single 8-line function isn't worth the dep churn. The
/// XOR-accumulate pattern is the same one `subtle` uses internally.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hello!"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
