//! On-chain payment record. One per `(invoice, txid, vout)` — an invoice can
//! receive multiple payments (partial top-ups) and each is tracked
//! independently so their confirmation states evolve in lockstep with the
//! chain.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Payment {
    pub id: Uuid,
    pub invoice_id: Uuid,
    /// Transaction ID this payment was found in.
    pub txid: String,
    /// Output index (vout for transparent, note index for shield).
    pub vout: u32,
    /// Value of this single output, in satoshis.
    pub amount_sat: u64,
    /// Current confirmation depth. Updated by the sync loop as the chain
    /// advances; once it crosses the configured threshold the parent invoice
    /// transitions Confirmed.
    pub confirmations: u32,
    /// Unix seconds when we first observed the payment in the mempool / a
    /// recently-seen block.
    pub seen_at: i64,
    /// Unix seconds when the payment first crossed the confirmation
    /// threshold. `None` until then.
    pub confirmed_at: Option<i64>,
}

impl Payment {
    pub fn new(invoice_id: Uuid, txid: String, vout: u32, amount_sat: u64, now: i64) -> Self {
        Self {
            id: Uuid::new_v4(),
            invoice_id,
            txid,
            vout,
            amount_sat,
            confirmations: 0,
            seen_at: now,
            confirmed_at: None,
        }
    }
}
