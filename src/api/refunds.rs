//! Refund REST endpoints. Read-only for now (refunds are auto-created
//! by the matcher). When Stage 7b lands the actual broadcaster, this
//! file gains a `mark_broadcast` endpoint for the operator-manual
//! workflow.

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
/// Operator workflow until Stage 7b lands: build the refund tx in your
/// wallet (or via pivx-agent-kit), broadcast it, post the txid here so
/// the merchant-kit record matches reality.
pub async fn mark_broadcast(
    State(state): State<Arc<SyncState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<MarkBroadcastRequest>,
) -> Result<Json<RefundResponse>, ApiError> {
    let _ = queue::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
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
