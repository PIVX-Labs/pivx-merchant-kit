//! Invoice records + lifecycle state machine.
//!
//! The state machine is intentionally pure: `next_status_on_payment` and
//! `next_status_on_tick` take the current state and return what the next
//! state should be, with no side effects. The persistence layer and the
//! sync loop are responsible for actually moving the invoice between states
//! and triggering downstream effects (webhook, refund). This makes the
//! state transitions exhaustively testable without standing up the rest
//! of the system.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which payment channel the invoice expects. Set when the invoice is
/// created; the matcher uses it to decide whether to watch the address as
/// a transparent UTXO or a shield note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaymentChannel {
    Transparent,
    Shield,
}

impl PaymentChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::Shield => "shield",
        }
    }
}

impl std::str::FromStr for PaymentChannel {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> crate::error::Result<Self> {
        match s {
            "transparent" => Ok(Self::Transparent),
            "shield" => Ok(Self::Shield),
            other => Err(crate::error::Error::Parse(format!(
                "unknown PaymentChannel: {}",
                other
            ))),
        }
    }
}

/// Invoice lifecycle.
///
/// `Pending → PartiallyPaid → Confirming → Confirmed` is the happy path.
/// `Expired` and `Cancelled` are terminal. Refunds aren't a status — they
/// live in their own table and reference the invoice they belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Pending,
    PartiallyPaid,
    Confirming,
    Confirmed,
    Expired,
    Cancelled,
}

impl InvoiceStatus {
    /// Terminal states never transition again. Useful guard in the matcher:
    /// if we see a payment for a Confirmed invoice, it's an overpay and goes
    /// down the refund path (Stage 7), not a state change.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed | Self::Expired | Self::Cancelled)
    }

    /// Whether the merchant should still be able to cancel this invoice
    /// through the API. Stage 7 narrows this further when refunds are off
    /// (no cancel during Confirming).
    pub fn is_cancellable(self) -> bool {
        matches!(self, Self::Pending | Self::PartiallyPaid)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::PartiallyPaid => "partially_paid",
            Self::Confirming => "confirming",
            Self::Confirmed => "confirmed",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for InvoiceStatus {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> crate::error::Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "partially_paid" => Ok(Self::PartiallyPaid),
            "confirming" => Ok(Self::Confirming),
            "confirmed" => Ok(Self::Confirmed),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(crate::error::Error::Parse(format!(
                "unknown InvoiceStatus: {}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Invoice {
    pub id: Uuid,
    /// Optional merchant-supplied idempotency key. The API rejects duplicate
    /// `external_id`s so retries don't create duplicate invoices.
    pub external_id: Option<String>,
    pub channel: PaymentChannel,
    /// Amount the customer owes, in satoshis.
    pub amount_due_sat: u64,
    /// PIVX address the customer should pay to. Derived once at creation
    /// from the merchant wallet's HD chain (one address per invoice keeps
    /// the matcher simple and on-chain analysis harder).
    pub address: String,
    /// HD slot that derives this invoice's address. Used by the refund
    /// builder to re-derive the spending key — without this, we'd need
    /// to scan the HD tree on every refund. Not serialised in API
    /// responses (internal detail, no value to merchant code).
    #[serde(default, skip_serializing)]
    pub hd_index: u32,
    pub status: InvoiceStatus,
    pub created_at: i64,
    /// Mutable. Set initially to `created_at + default_expiry_secs`. Partial
    /// payments reset it to `now + partial_reset_secs` so the customer gets
    /// a clean window to top up.
    pub expires_at: i64,
    /// Required when `refunds.enabled` in the daemon config. The API layer
    /// enforces presence; here we just store what the caller provided.
    pub refund_address: Option<String>,
    /// Free-form merchant context — order ID, customer ID, anything they
    /// want echoed back in the webhook. Treated as opaque.
    pub metadata: serde_json::Value,
}

/// Decides what the next status should be after observing a payment.
///
/// Pure function: no IO, no allocations beyond the return value, no clock
/// reads. The caller passes in the running paid-so-far total (which it
/// computes by summing prior `Payment` rows). Returning the *new* status
/// rather than mutating in place keeps the call site explicit.
///
/// Confirmation-threshold logic isn't here — that's a per-payment property,
/// driven by `next_status_on_confirmation` below.
pub fn next_status_on_payment(
    current: InvoiceStatus,
    amount_due: u64,
    paid_total_after: u64,
) -> InvoiceStatus {
    // Terminal states ignore new payments at the status level. The caller
    // is responsible for routing overpays-to-Confirmed into the refund flow.
    if current.is_terminal() {
        return current;
    }
    if paid_total_after >= amount_due {
        InvoiceStatus::Confirming
    } else {
        InvoiceStatus::PartiallyPaid
    }
}

/// Decides what the next status should be when a payment crosses the
/// configured confirmation depth.
///
/// `paid_total_confirmed` is the sum of payments that have *all* reached
/// the threshold. We only transition to Confirmed once the full amount-due
/// has cleared — a partial that's confirmed but undercommitted stays in
/// PartiallyPaid territory at the invoice level.
pub fn next_status_on_confirmation(
    current: InvoiceStatus,
    amount_due: u64,
    paid_total_confirmed: u64,
) -> InvoiceStatus {
    if current.is_terminal() {
        return current;
    }
    if paid_total_confirmed >= amount_due {
        InvoiceStatus::Confirmed
    } else {
        // Not enough confirmed value yet. The invoice is still waiting,
        // but we don't downgrade — if it was already Confirming, stay there;
        // if it was PartiallyPaid, stay there. Same input → same output.
        current
    }
}

/// Decides what the next status should be when the expiry clock fires.
///
/// `Confirming` invoices are explicitly **not** expirable — once the
/// customer has paid the full amount, they're entitled to the goods even
/// if confirmations take longer than the expiry window. The matcher caps
/// confirmation latency separately, so a stuck `Confirming` invoice is a
/// systems-level alert, not a customer-facing expiry.
pub fn next_status_on_tick(current: InvoiceStatus, now: i64, expires_at: i64) -> InvoiceStatus {
    if current.is_terminal() {
        return current;
    }
    if now < expires_at {
        return current;
    }
    match current {
        InvoiceStatus::Pending | InvoiceStatus::PartiallyPaid => InvoiceStatus::Expired,
        // Confirming is intentionally immune to expiry — see doc comment.
        InvoiceStatus::Confirming => InvoiceStatus::Confirming,
        // Already terminal, handled above.
        s => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- next_status_on_payment ----

    #[test]
    fn pending_underpaid_becomes_partially_paid() {
        let next = next_status_on_payment(InvoiceStatus::Pending, 1000, 400);
        assert_eq!(next, InvoiceStatus::PartiallyPaid);
    }

    #[test]
    fn pending_exact_becomes_confirming() {
        let next = next_status_on_payment(InvoiceStatus::Pending, 1000, 1000);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn pending_overpaid_becomes_confirming() {
        // Excess is for the refund/donation layer to handle — at the status
        // level, the customer has fulfilled their obligation.
        let next = next_status_on_payment(InvoiceStatus::Pending, 1000, 1500);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn partial_topup_underpaid_stays_partial() {
        let next = next_status_on_payment(InvoiceStatus::PartiallyPaid, 1000, 700);
        assert_eq!(next, InvoiceStatus::PartiallyPaid);
    }

    #[test]
    fn partial_topup_to_full_becomes_confirming() {
        let next = next_status_on_payment(InvoiceStatus::PartiallyPaid, 1000, 1000);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn confirming_invoice_status_unchanged_by_new_payment() {
        // Overpay after the invoice already hit Confirming. The status
        // doesn't regress — Stage 7 handles the excess.
        let next = next_status_on_payment(InvoiceStatus::Confirming, 1000, 1500);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn terminal_states_never_transition_on_payment() {
        for status in [
            InvoiceStatus::Confirmed,
            InvoiceStatus::Expired,
            InvoiceStatus::Cancelled,
        ] {
            let next = next_status_on_payment(status, 1000, 5000);
            assert_eq!(next, status, "{:?} should be sticky", status);
        }
    }

    // ---- next_status_on_confirmation ----

    #[test]
    fn confirming_with_full_confirmed_value_becomes_confirmed() {
        let next = next_status_on_confirmation(InvoiceStatus::Confirming, 1000, 1000);
        assert_eq!(next, InvoiceStatus::Confirmed);
    }

    #[test]
    fn confirming_with_partial_confirmed_value_stays_confirming() {
        // Two payments — one confirmed, one still in mempool. We don't flip
        // to Confirmed until *all* the required value has cleared.
        let next = next_status_on_confirmation(InvoiceStatus::Confirming, 1000, 600);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn partial_with_confirmed_value_below_due_stays_partial() {
        let next = next_status_on_confirmation(InvoiceStatus::PartiallyPaid, 1000, 400);
        assert_eq!(next, InvoiceStatus::PartiallyPaid);
    }

    #[test]
    fn already_confirmed_stays_confirmed() {
        let next = next_status_on_confirmation(InvoiceStatus::Confirmed, 1000, 1000);
        assert_eq!(next, InvoiceStatus::Confirmed);
    }

    // ---- next_status_on_tick ----

    #[test]
    fn pending_past_expiry_becomes_expired() {
        let next = next_status_on_tick(InvoiceStatus::Pending, 200, 100);
        assert_eq!(next, InvoiceStatus::Expired);
    }

    #[test]
    fn partial_past_expiry_becomes_expired() {
        let next = next_status_on_tick(InvoiceStatus::PartiallyPaid, 200, 100);
        assert_eq!(next, InvoiceStatus::Expired);
    }

    #[test]
    fn confirming_past_expiry_stays_confirming() {
        // Customer already paid the full amount — they're entitled to the
        // goods even if confirmations take longer than the window.
        let next = next_status_on_tick(InvoiceStatus::Confirming, 200, 100);
        assert_eq!(next, InvoiceStatus::Confirming);
    }

    #[test]
    fn not_yet_expired_unchanged() {
        let next = next_status_on_tick(InvoiceStatus::Pending, 50, 100);
        assert_eq!(next, InvoiceStatus::Pending);
    }

    #[test]
    fn terminal_states_never_transition_on_tick() {
        for status in [
            InvoiceStatus::Confirmed,
            InvoiceStatus::Expired,
            InvoiceStatus::Cancelled,
        ] {
            let next = next_status_on_tick(status, 10_000, 100);
            assert_eq!(next, status);
        }
    }

    // ---- helpers ----

    #[test]
    fn is_terminal_matches_doc() {
        assert!(!InvoiceStatus::Pending.is_terminal());
        assert!(!InvoiceStatus::PartiallyPaid.is_terminal());
        assert!(!InvoiceStatus::Confirming.is_terminal());
        assert!(InvoiceStatus::Confirmed.is_terminal());
        assert!(InvoiceStatus::Expired.is_terminal());
        assert!(InvoiceStatus::Cancelled.is_terminal());
    }

    #[test]
    fn is_cancellable_matches_doc() {
        assert!(InvoiceStatus::Pending.is_cancellable());
        assert!(InvoiceStatus::PartiallyPaid.is_cancellable());
        assert!(!InvoiceStatus::Confirming.is_cancellable());
        assert!(!InvoiceStatus::Confirmed.is_cancellable());
        assert!(!InvoiceStatus::Expired.is_cancellable());
        assert!(!InvoiceStatus::Cancelled.is_cancellable());
    }

    // ---- realistic scenarios stitched together ----

    #[test]
    fn full_happy_path() {
        // Pending → first payment (full) → Confirming → confs reached → Confirmed.
        let s = InvoiceStatus::Pending;
        let s = next_status_on_payment(s, 1000, 1000);
        assert_eq!(s, InvoiceStatus::Confirming);
        let s = next_status_on_confirmation(s, 1000, 1000);
        assert_eq!(s, InvoiceStatus::Confirmed);
    }

    #[test]
    fn partial_then_topup_path() {
        // Pending → underpay (Partial) → topup (Confirming) → confs → Confirmed.
        let s = InvoiceStatus::Pending;
        let s = next_status_on_payment(s, 1000, 600);
        assert_eq!(s, InvoiceStatus::PartiallyPaid);
        let s = next_status_on_payment(s, 1000, 1000);
        assert_eq!(s, InvoiceStatus::Confirming);
        let s = next_status_on_confirmation(s, 1000, 1000);
        assert_eq!(s, InvoiceStatus::Confirmed);
    }

    #[test]
    fn partial_then_expire_path() {
        // Pending → underpay → time runs out → Expired (refund-eligible if
        // refunds.enabled — that's Stage 7's call, not the matcher's).
        let s = InvoiceStatus::Pending;
        let s = next_status_on_payment(s, 1000, 400);
        assert_eq!(s, InvoiceStatus::PartiallyPaid);
        let s = next_status_on_tick(s, 200, 100);
        assert_eq!(s, InvoiceStatus::Expired);
    }
}
