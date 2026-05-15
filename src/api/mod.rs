//! HTTP control plane.
//!
//! Merchant backends drive the daemon through this layer: create invoices,
//! poll their state, cancel them. Webhooks (Stage 6) push state changes
//! out without the merchant having to poll.
//!
//! Auth: bearer token from `config.api.auth_token`, checked in a tower
//! middleware. The token is compared constant-time to defang timing
//! attacks even though they're a marginal threat for a short string.
//!
//! All endpoints under `/v1/*` require auth. `/healthz` is unauthenticated
//! so load balancers and uptime monitors can probe without secrets.

pub mod auth;
pub mod error;
pub mod invoices;
pub mod refunds;
pub mod types;

use crate::sync::SyncState;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub fn router(state: Arc<SyncState>) -> Router {
    let authed = Router::new()
        .route("/v1/invoices", post(invoices::create).get(invoices::list))
        .route("/v1/invoices/:id", get(invoices::get))
        .route("/v1/invoices/:id/cancel", post(invoices::cancel))
        .route("/v1/refunds", get(refunds::list))
        .route("/v1/refunds/:id", get(refunds::get))
        .route("/v1/refunds/:id/broadcast", post(refunds::mark_broadcast))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health))
        .merge(authed)
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
