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
    /// Block height the payment was mined at. `0` means we observed it
    /// in the mempool but no block has confirmed it yet. The
    /// confirmation matcher uses `chain_tip - block_height + 1` (when
    /// non-zero) so confirmation depth tracks the chain, not poll ticks.
    pub block_height: u32,
    /// Current confirmation depth, recomputed from `block_height`
    /// against the chain tip on every confirmation sweep. Naturally
    /// drops back to 0 on a deep-enough reorg.
    pub confirmations: u32,
    /// Unix seconds when we first observed the payment in the mempool /
    /// a recently-seen block.
    pub seen_at: i64,
    /// Unix seconds when the payment first crossed the configured
    /// confirmation threshold. `None` until then; `COALESCE`-protected
    /// in storage so it can't accidentally regress on subsequent
    /// updates.
    pub confirmed_at: Option<i64>,
}

impl Payment {
    pub fn new(
        invoice_id: Uuid,
        txid: String,
        vout: u32,
        amount_sat: u64,
        block_height: u32,
        now: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            invoice_id,
            txid,
            vout,
            amount_sat,
            block_height,
            confirmations: 0,
            seen_at: now,
            confirmed_at: None,
        }
    }
}
