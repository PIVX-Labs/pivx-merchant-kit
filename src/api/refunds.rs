//! Refund REST endpoints. Refunds are auto-created by the matcher and
//! auto-broadcast by the refund worker. The `mark_broadcast` endpoint
//! exists for the operator-manual fallback: if auto-broadcast can't
//! work (Sapling prover unavailable, RPC outage, edge case), the
//! operator can build the tx in another wallet and record the txid here
//! so the merchant-kit DB matches reality.

use crate::api::error::ApiError;
use crate::refunds::queue;
use crate::sync::SyncState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct RefundResponse {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub reason: String,
    pub to_address: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub txid: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub broadcast_at: Option<i64>,
    pub confirmed_at: Option<i64>,
}

impl From<queue::Refund> for RefundResponse {
    fn from(r: queue::Refund) -> Self {
        Self {
            id: r.id,
            invoice_id: r.invoice_id,
            reason: r.reason,
            to_address: r.to_address,
            amount_sat: r.amount_sat,
            fee_sat: r.fee_sat,
            txid: r.txid,
            status: r.status,
            created_at: r.created_at,
            broadcast_at: r.broadcast_at,
            confirmed_at: r.confirmed_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RefundListResponse {
    pub refunds: Vec<RefundResponse>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// GET /v1/refunds
pub async fn list(
    State(state): State<Arc<SyncState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<RefundListResponse>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let refunds = queue::list(&state.db, limit).await?;
    Ok(Json(RefundListResponse {
        refunds: refunds.into_iter().map(RefundResponse::from).collect(),
    }))
}

/// GET /v1/refunds/:id
pub async fn get(
    State(state): State<Arc<SyncState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<RefundResponse>, ApiError> {
    let refund = queue::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(refund.into()))
}

#[derive(Debug, Deserialize)]
pub struct MarkBroadcastRequest {
    pub txid: String,
}

/// POST /v1/refunds/:id/broadcast
///
/// Operator fallback for when the daemon's auto-broadcast couldn't run
/// (Sapling prover unavailable, RPC outage, etc.): the operator builds
/// the refund tx in another wallet, broadcasts it, then records the
/// txid here so the merchant-kit DB matches reality.
///
/// Refuses to overwrite a refund that's already in any non-pending
/// status — once the daemon (or a previous manual call) has recorded a
/// broadcast, the txid is immutable. Without this guard an operator (or
/// anyone with the bearer token) could overwrite the real on-chain
/// txid with a fake hex string, breaking reconciliation.
pub async fn mark_broadcast(
    State(state): State<Arc<SyncState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<MarkBroadcastRequest>,
) -> Result<Json<RefundResponse>, ApiError> {
    let existing = queue::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    if existing.status != "pending" {
        return Err(ApiError::Conflict(format!(
            "refund is already in status `{}` with txid {:?} — cannot overwrite. \
             Use the existing record or create a new refund row if a re-broadcast \
             is genuinely needed.",
            existing.status, existing.txid
        )));
    }
    let txid = req.txid.trim();
    if txid.len() != 64 || !txid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest(
            "txid must be 64-char hex".into(),
        ));
    }
    queue::mark_broadcast(&state.db, id, txid, unix_now()).await?;
    let refund = queue::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    tracing::info!(refund_id = %id, txid = %txid, "refund marked broadcast by operator");
    Ok(Json(refund.into()))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
