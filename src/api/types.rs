//! Request/response DTOs for the REST API. Kept separate from the
//! storage types so the wire format can evolve independently from the
//! internal schema.

use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
use crate::payment::Payment;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    /// Optional merchant idempotency key. If a previous invoice exists
    /// with this `external_id`, the API returns it as-is (200 OK) instead
    /// of creating a duplicate.
    #[serde(default)]
    pub external_id: Option<String>,
    pub channel: PaymentChannel,
    pub amount_due_sat: u64,
    /// Override the daemon's default expiry for just this invoice. Caps
    /// at 7 days to avoid a typo creating an invoice that never expires
    /// in practice.
    #[serde(default)]
    pub expires_in_secs: Option<u64>,
    /// Required when the daemon has `refunds.enabled = true`. The API
    /// rejects requests missing this in that mode.
    #[serde(default)]
    pub refund_address: Option<String>,
    /// Opaque merchant context (order ID, customer ID, etc.). Echoed
    /// back in the webhook payload so the merchant can correlate.
    #[serde(default = "serde_json::Value::default")]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct InvoiceResponse {
    pub id: Uuid,
    pub external_id: Option<String>,
    pub channel: PaymentChannel,
    pub amount_due_sat: u64,
    pub amount_paid_sat: u64,
    pub address: String,
    pub status: InvoiceStatus,
    pub created_at: i64,
    pub expires_at: i64,
    pub refund_address: Option<String>,
    pub metadata: serde_json::Value,
    pub payments: Vec<PaymentResponse>,
}

#[derive(Debug, Serialize)]
pub struct PaymentResponse {
    pub txid: String,
    pub vout: u32,
    pub amount_sat: u64,
    pub confirmations: u32,
    pub seen_at: i64,
    pub confirmed_at: Option<i64>,
}

impl PaymentResponse {
    pub fn from_payment(p: Payment) -> Self {
        Self {
            txid: p.txid,
            vout: p.vout,
            amount_sat: p.amount_sat,
            confirmations: p.confirmations,
            seen_at: p.seen_at,
            confirmed_at: p.confirmed_at,
        }
    }
}

impl InvoiceResponse {
    pub fn from_invoice(invoice: Invoice, payments: Vec<Payment>) -> Self {
        let amount_paid_sat = payments.iter().map(|p| p.amount_sat).sum();
        Self {
            id: invoice.id,
            external_id: invoice.external_id,
            channel: invoice.channel,
            amount_due_sat: invoice.amount_due_sat,
            amount_paid_sat,
            address: invoice.address,
            status: invoice.status,
            created_at: invoice.created_at,
            expires_at: invoice.expires_at,
            refund_address: invoice.refund_address,
            metadata: invoice.metadata,
            payments: payments.into_iter().map(PaymentResponse::from_payment).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InvoiceListResponse {
    pub invoices: Vec<InvoiceResponse>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub status: Option<InvoiceStatus>,
    #[serde(default)]
    pub limit: Option<i64>,
}
