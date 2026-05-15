//! Match raw sync output (transparent UTXOs + decrypted shield notes)
//! against open invoices and drive the state machine.
//!
//! The matcher is the glue between Stage 3b's I/O layer and Stage 1's pure
//! state-transition functions. It owns:
//!   - Idempotent payment insertion (the `UNIQUE(txid, vout)` constraint
//!     means a re-seen UTXO is rejected at the DB layer; the matcher
//!     swallows that specific error and continues.
//!   - Status transitions via [`crate::invoice::next_status_on_payment`].
//!   - Partial-payment expiry reset: an invoice that drops back into
//!     `PartiallyPaid` gets `expires_at = now + partial_reset_secs` so
//!     the customer has a clean window to top up.
//!
//! Sub-modules: `transparent` and `shield`. Each operates independently
//! on the output of its corresponding sync source.

pub mod confirms;
pub mod shield;
pub mod sweeper;
pub mod transparent;

use crate::config::PaymentsConfig;
use crate::error::Result;
use crate::invoice::{next_status_on_payment, Invoice, InvoiceStatus};
use crate::storage::{invoices, payments, Db};
use uuid::Uuid;

/// Apply a freshly-detected payment to its invoice. Pure post-DB-insert
/// step: recompute paid-so-far, transition status if appropriate, reset
/// the expiry clock on a partial payment.
///
/// Caller has already inserted the Payment row. This function only
/// touches the invoice row.
async fn apply_payment_to_invoice(
    db: &Db,
    invoice_id: Uuid,
    config: &PaymentsConfig,
    now: i64,
) -> Result<()> {
    // Re-read the invoice so we have the current status. A concurrent
    // matcher run on the same invoice would see the post-insert state
    // here; SQLite's writer-lock plus a single matcher task means that
    // can't happen in practice today, but reading fresh keeps the
    // function correct under future concurrency too.
    let Some(invoice) = invoices::get(db, invoice_id).await? else {
        // Invoice was deleted between payment insertion and this update.
        // The payment row remains (cascade is set up but we don't delete
        // invoices in production). Nothing to update.
        return Ok(());
    };

    let paid_total = payments::total_amount_for_invoice(db, invoice_id).await?;
    let next_status =
        next_status_on_payment(invoice.status, invoice.amount_due_sat, paid_total);

    if next_status != invoice.status {
        invoices::update_status(db, invoice_id, next_status).await?;
        tracing::info!(
            invoice_id = %invoice_id,
            prev = %invoice.status.as_str(),
            next = %next_status.as_str(),
            paid_sat = paid_total,
            due_sat = invoice.amount_due_sat,
            "invoice state advanced"
        );
    }

    // Partial-payment timer reset. Trigger on any transition into or stay
    // in PartiallyPaid — the customer needs a fresh window every time
    // we observe a top-up that doesn't reach the full amount. We
    // explicitly do NOT extend an already-Confirming invoice; that's
    // immune to expiry by design.
    if next_status == InvoiceStatus::PartiallyPaid {
        let new_expiry = now + config.partial_reset_secs as i64;
        if new_expiry > invoice.expires_at {
            invoices::update_expires_at(db, invoice_id, new_expiry).await?;
            tracing::debug!(
                invoice_id = %invoice_id,
                expires_at = new_expiry,
                "partial-payment timer reset"
            );
        }
    }

    Ok(())
}

/// Apply a freshly-inserted payment for `invoice` to its parent invoice.
/// Convenience wrapper that takes the invoice's id directly. The matcher
/// modules pass through here so the state-machine glue lives in one place.
async fn apply_for(db: &Db, invoice: &Invoice, config: &PaymentsConfig, now: i64) -> Result<()> {
    apply_payment_to_invoice(db, invoice.id, config, now).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcceptPolicy;
    use crate::invoice::{Invoice, PaymentChannel};
    use crate::payment::Payment;

    fn payments_config() -> PaymentsConfig {
        PaymentsConfig {
            accept: AcceptPolicy::Both,
            confirmations: 3,
            default_expiry_secs: 1800,
            partial_reset_secs: 1800,
        }
    }

    fn invoice(addr: &str, amount: u64, status: InvoiceStatus, expires_at: i64) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: amount,
            address: addr.into(),
            status,
            created_at: 0,
            expires_at,
            refund_address: None,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn underpayment_transitions_pending_to_partial() {
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr1", 1000, InvoiceStatus::Pending, 100);
        invoices::insert(&db, &inv).await.unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx1".into(), 0, 400, 1))
            .await
            .unwrap();

        apply_payment_to_invoice(&db, inv.id, &payments_config(), 500)
            .await
            .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::PartiallyPaid);
        // expires_at reset to now + partial_reset_secs.
        assert_eq!(updated.expires_at, 500 + 1800);
    }

    #[tokio::test]
    async fn exact_payment_transitions_pending_to_confirming() {
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr2", 1000, InvoiceStatus::Pending, 100);
        invoices::insert(&db, &inv).await.unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx1".into(), 0, 1000, 1))
            .await
            .unwrap();

        apply_payment_to_invoice(&db, inv.id, &payments_config(), 500)
            .await
            .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
        // Confirming should NOT have its expiry reset (immune by design).
        assert_eq!(updated.expires_at, 100);
    }

    #[tokio::test]
    async fn partial_to_full_transitions_to_confirming() {
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr3", 1000, InvoiceStatus::PartiallyPaid, 100);
        invoices::insert(&db, &inv).await.unwrap();
        // First partial payment was earlier, now a topup arrives.
        payments::insert(&db, &Payment::new(inv.id, "tx1".into(), 0, 400, 1))
            .await
            .unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx2".into(), 0, 600, 2))
            .await
            .unwrap();

        apply_payment_to_invoice(&db, inv.id, &payments_config(), 500)
            .await
            .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn partial_topup_extends_expiry_but_doesnt_shorten_it() {
        // If the invoice already had an expiry further out than
        // now + partial_reset_secs, we keep the longer expiry.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr4", 1000, InvoiceStatus::Pending, 5_000_000);
        invoices::insert(&db, &inv).await.unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx1".into(), 0, 400, 1))
            .await
            .unwrap();

        apply_payment_to_invoice(&db, inv.id, &payments_config(), 500)
            .await
            .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::PartiallyPaid);
        // Still the original far-future expiry — partial reset shouldn't
        // SHORTEN an already-distant expiry.
        assert_eq!(updated.expires_at, 5_000_000);
    }

    #[tokio::test]
    async fn no_state_change_no_status_update() {
        // Invoice already Confirming, another overpay comes in. The
        // status stays Confirming; the function should be a no-op
        // beyond reading state.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr5", 1000, InvoiceStatus::Confirming, 100);
        invoices::insert(&db, &inv).await.unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx1".into(), 0, 1000, 1))
            .await
            .unwrap();
        payments::insert(&db, &Payment::new(inv.id, "tx2".into(), 0, 500, 2))
            .await
            .unwrap();

        apply_payment_to_invoice(&db, inv.id, &payments_config(), 500)
            .await
            .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
        assert_eq!(updated.expires_at, 100);
    }

    #[tokio::test]
    async fn missing_invoice_is_silent_noop() {
        let db = Db::open_memory().await.unwrap();
        let ghost = Uuid::new_v4();
        // Should not error even though the invoice doesn't exist.
        apply_payment_to_invoice(&db, ghost, &payments_config(), 500)
            .await
            .unwrap();
    }
}
