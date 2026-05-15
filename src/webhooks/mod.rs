//! Webhook delivery.
//!
//! Events the matcher produces (invoice.confirmed, invoice.expired,
//! invoice.cancelled) are persisted into `webhook_deliveries` rows.
//! A background worker fetches due rows on a tick, POSTs them to the
//! configured URL with an HMAC-SHA256 body signature, and either marks
//! them delivered or schedules an exponential-backoff retry. After
//! `max_attempts` failed attempts the row lands in the dead-letter
//! state for the operator to inspect.
//!
//! Merchant verifies:
//!   1. `X-Merchant-Signature` matches `hmac_sha256(body, secret)` (hex)
//!   2. `X-Merchant-Delivery-Id` hasn't been processed before
//!
//! These two together give HMAC authenticity + idempotent processing.

pub mod delivery;
pub mod queue;

use crate::error::Result;
use crate::storage::Db;
use crate::sync::SyncState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Event types the daemon emits. Keep additive — merchants should be
/// able to register their handlers once and tolerate the daemon
/// introducing new event types they don't care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    InvoiceConfirmed,
    InvoiceExpired,
    InvoiceCancelled,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvoiceConfirmed => "invoice.confirmed",
            Self::InvoiceExpired => "invoice.expired",
            Self::InvoiceCancelled => "invoice.cancelled",
        }
    }
}

/// Run the webhook worker until shutdown. Polls every 5 seconds for due
/// deliveries; that's quick enough to give snappy delivery without
/// hammering SQLite. Stops cleanly on shutdown signal.
pub async fn run(state: Arc<SyncState>) {
    let interval = Duration::from_secs(5);
    tracing::info!("webhook worker starting");
    loop {
        if let Err(e) = tick(&state).await {
            tracing::warn!(err = %e, "webhook worker tick failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = state.shutdown.notified() => {
                tracing::info!("webhook worker received shutdown signal");
                return;
            }
        }
    }
}

async fn tick(state: &SyncState) -> Result<()> {
    let now = unix_now();
    let due = queue::fetch_due(&state.db, now, 50).await?;
    if due.is_empty() {
        return Ok(());
    }
    for delivery in due {
        if let Err(e) = delivery::deliver_one(&state.db, &state.config.webhooks, delivery).await {
            tracing::warn!(err = %e, "single webhook delivery encountered an error");
        }
    }
    Ok(())
}

/// Enqueue an event for asynchronous delivery. Called by the matcher
/// when an invoice transitions Confirmed (and, in future, Expired
/// or Cancelled).
///
/// The payload is the full invoice response — same shape the API
/// returns from GET /v1/invoices/:id, so the merchant can consume it
/// without re-fetching.
pub async fn enqueue(
    db: &Db,
    invoice_id: Uuid,
    event_type: EventType,
) -> Result<()> {
    let invoice_opt = crate::storage::invoices::get(db, invoice_id).await?;
    let Some(invoice) = invoice_opt else {
        return Ok(());
    };
    let payments = crate::storage::payments::list_for_invoice(db, invoice_id).await?;
    let invoice_payload =
        crate::api::types::InvoiceResponse::from_invoice(invoice, payments);

    let payload = WebhookPayload {
        event_id: Uuid::new_v4(),
        event_type,
        created_at: unix_now(),
        invoice: invoice_payload,
    };
    queue::insert(db, invoice_id, event_type, &payload, unix_now()).await?;
    tracing::info!(
        invoice_id = %invoice_id,
        event = %event_type.as_str(),
        "webhook delivery enqueued"
    );
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub event_id: Uuid,
    #[serde(serialize_with = "serialize_event_type")]
    pub event_type: EventType,
    pub created_at: i64,
    pub invoice: crate::api::types::InvoiceResponse,
}

fn serialize_event_type<S: serde::Serializer>(
    e: &EventType,
    ser: S,
) -> std::result::Result<S::Ok, S::Error> {
    ser.serialize_str(e.as_str())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
