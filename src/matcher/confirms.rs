//! Confirmation-depth tracking and Confirming → Confirmed transitions.
//!
//! Every payment row carries a `block_height` (the chain height the tx
//! was mined into, or 0 if still in the mempool) and a `confirmations`
//! count. Each tick:
//!
//!   1. Fetches the current chain tip.
//!   2. For every payment on an in-flight invoice, recomputes
//!      `confirmations = max(0, chain_tip - block_height + 1)` when
//!      `block_height != 0`, else 0 (still mempool).
//!   3. Re-evaluates each affected invoice. If the sum of payments at or
//!      above the threshold covers `amount_due_sat`, transitions
//!      Confirming → Confirmed and emits the webhook.
//!
//! **Reorg safety**: the count naturally decreases if the tip rolls
//! back. The `confirmed_at` timestamp uses `COALESCE` so the
//! first-confirm moment is preserved across reorgs.
//!
//! **Threshold semantics**: a 5-conf partial + 1-conf top-up with
//! threshold = 3 still leaves the invoice Confirming until the top-up
//! matures, because `confirmed_amount_for_invoice` only sums payments
//! at or above the threshold.

use crate::config::PaymentsConfig;
use crate::error::Result;
use crate::invoice::{next_status_on_confirmation, InvoiceStatus};
use crate::storage::{invoices, payments, Db};
use sqlx::Row;

pub async fn tick(
    db: &Db,
    config: &PaymentsConfig,
    refunds_enabled: bool,
    chain_tip: u32,
    now: i64,
) -> Result<usize> {
    let rows = sqlx::query(
        "SELECT p.id, p.invoice_id, p.block_height, p.confirmations
           FROM payments p
           JOIN invoices i ON i.id = p.invoice_id
          WHERE i.status IN ('pending', 'partially_paid', 'confirming')",
    )
    .fetch_all(db.pool())
    .await?;

    let mut updated = 0usize;
    let mut affected_invoices = std::collections::BTreeSet::new();
    for row in rows {
        let payment_id_str: String = row.try_get("id")?;
        let invoice_id_str: String = row.try_get("invoice_id")?;
        let block_height: i64 = row.try_get("block_height")?;
        let confirmations: i64 = row.try_get("confirmations")?;
        let payment_id = uuid::Uuid::parse_str(&payment_id_str)
            .map_err(|e| crate::error::Error::Parse(format!("payment id: {}", e)))?;
        let invoice_id = uuid::Uuid::parse_str(&invoice_id_str)
            .map_err(|e| crate::error::Error::Parse(format!("invoice id: {}", e)))?;

        // Compute the on-chain confirmation depth from the height the
        // payment was mined at (0 = mempool) and the current chain tip.
        // Naturally drops back to 0 on a reorg that uncrowns the block.
        let block_height_u = u32::try_from(block_height).unwrap_or(0);
        let new_confirmations: u32 = if block_height_u == 0 || chain_tip < block_height_u {
            0
        } else {
            chain_tip.saturating_sub(block_height_u).saturating_add(1)
        };

        let confirmed_at = if new_confirmations >= 1 && confirmations == 0 {
            Some(now)
        } else {
            None
        };

        if new_confirmations as i64 != confirmations {
            payments::update_confirmations(db, payment_id, new_confirmations, confirmed_at).await?;
            updated += 1;
            affected_invoices.insert(invoice_id);
        } else if confirmations > 0
            && config.confirmations > 0
            && new_confirmations >= config.confirmations
        {
            // Belt-and-braces: payments already at or above threshold
            // must still re-evaluate their parent invoice. Handles the
            // edge case where confirmations didn't change this tick but
            // the parent invoice is still Confirming (e.g. a fresh
            // backfill where payments were inserted at confs >=
            // threshold and the count happens to match this tip's
            // computation exactly).
            affected_invoices.insert(invoice_id);
        }
    }

    // Special case for zero-conf deployments: with threshold = 0, every
    // mempool-seen payment counts. The block-height-based computation
    // above keeps mempool payments at confirmations = 0, so they pass
    // the `>= 0` threshold check trivially. Make sure their parent
    // invoices get re-evaluated by surfacing them here even if no
    // confirmation count changed.
    if config.confirmations == 0 {
        let zero_conf_rows = sqlx::query(
            "SELECT DISTINCT p.invoice_id
               FROM payments p
               JOIN invoices i ON i.id = p.invoice_id
              WHERE i.status = 'confirming'",
        )
        .fetch_all(db.pool())
        .await?;
        for row in zero_conf_rows {
            let invoice_id_str: String = row.try_get("invoice_id")?;
            let invoice_id = uuid::Uuid::parse_str(&invoice_id_str)
                .map_err(|e| crate::error::Error::Parse(format!("invoice id: {}", e)))?;
            affected_invoices.insert(invoice_id);
        }
    }

    // Re-evaluate every affected invoice. If its confirmed amount has
    // crossed the threshold, mark it Confirmed.
    for invoice_id in affected_invoices {
        let Some(invoice) = invoices::get(db, invoice_id).await? else {
            continue;
        };
        if invoice.status != InvoiceStatus::Confirming {
            continue;
        }
        let confirmed = payments::confirmed_amount_for_invoice(
            db,
            invoice_id,
            config.confirmations,
        )
        .await?;
        let next = next_status_on_confirmation(invoice.status, invoice.amount_due_sat, confirmed);
        if next == InvoiceStatus::Confirmed {
            invoices::update_status(db, invoice_id, InvoiceStatus::Confirmed).await?;
            tracing::info!(
                invoice_id = %invoice_id,
                amount_due = invoice.amount_due_sat,
                confirmed_sat = confirmed,
                threshold = config.confirmations,
                chain_tip = chain_tip,
                "invoice confirmed — enqueuing webhook"
            );
            // Reload the invoice so we have the freshest status
            // (just updated) for the refund check.
            let refreshed = invoices::get(db, invoice_id).await?.unwrap_or(invoice);
            crate::refunds::maybe_enqueue_overpayment(
                db, &refreshed, config, refunds_enabled,
            )
            .await?;
            crate::webhooks::enqueue(
                db,
                invoice_id,
                crate::webhooks::EventType::InvoiceConfirmed,
            )
            .await?;
        }
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcceptPolicy;
    use crate::invoice::{Invoice, PaymentChannel};
    use crate::payment::Payment;
    use crate::storage::invoices;
    use uuid::Uuid;

    fn cfg(confirmations: u32) -> PaymentsConfig {
        PaymentsConfig {
            accept: AcceptPolicy::Both,
            confirmations,
            default_expiry_secs: 1800,
            partial_reset_secs: 1800,
        }
    }

    fn invoice(addr: &str, amount: u64, status: InvoiceStatus) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: amount,
            address: addr.into(),
            hd_index: 0,
            status,
            created_at: 0,
            expires_at: 100,
            refund_address: None,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn payment_at_chain_tip_gets_one_confirmation() {
        // Payment mined at the same height as the chain tip = 1 conf.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr1", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        // block_height = 100 (matches the chain tip we pass to tick)
        let p = Payment::new(inv.id, "tx1".into(), 0, 1000, 100, 1);
        payments::insert(&db, &p).await.unwrap();

        let updated = tick(&db, &cfg(3), false, 100, 5).await.unwrap();
        assert_eq!(updated, 1);
        let listed = payments::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(listed[0].confirmations, 1);
        assert_eq!(listed[0].confirmed_at, Some(5));
    }

    #[tokio::test]
    async fn mempool_payment_stays_at_zero_confirmations() {
        // Payment with block_height = 0 (mempool, not yet mined) stays
        // at zero confirmations no matter how many ticks fire — this is
        // the core fix for the v0.1.0 audit finding B1. The old code
        // incremented +1 per tick regardless of chain advancement.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr1b", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        let p = Payment::new(inv.id, "tx1".into(), 0, 1000, 0, 1);
        payments::insert(&db, &p).await.unwrap();

        for _ in 0..10 {
            tick(&db, &cfg(3), false, 100, 5).await.unwrap();
        }

        let listed = payments::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(listed[0].confirmations, 0);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        // Stays Confirming since 0 confs < threshold 3.
        assert_eq!(updated.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn confirms_track_chain_tip_advancement() {
        // Payment mined at height 100. Chain advances tick-by-tick.
        // Confirmations follow chain reality exactly.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr2", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        let p = Payment::new(inv.id, "tx1".into(), 0, 1000, 100, 1);
        payments::insert(&db, &p).await.unwrap();

        // Tip at 100 = 1 conf
        tick(&db, &cfg(3), false, 100, 5).await.unwrap();
        assert_eq!(
            payments::list_for_invoice(&db, inv.id).await.unwrap()[0].confirmations,
            1
        );
        // Tip at 101 = 2 confs
        tick(&db, &cfg(3), false, 101, 5).await.unwrap();
        assert_eq!(
            payments::list_for_invoice(&db, inv.id).await.unwrap()[0].confirmations,
            2
        );
        // Tip at 102 = 3 confs → invoice should now be Confirmed
        tick(&db, &cfg(3), false, 102, 5).await.unwrap();
        assert_eq!(
            payments::list_for_invoice(&db, inv.id).await.unwrap()[0].confirmations,
            3
        );
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirmed);
    }

    #[tokio::test]
    async fn confirms_drop_on_reorg() {
        // Reorg: the chain rolls back below the payment's mined height.
        // Confirmations naturally drop to 0 (the saturating_sub branch).
        //
        // Threshold = 100 here so the payment never confirms the
        // invoice during the test — once an invoice goes to Confirmed
        // (terminal), the matcher stops updating its payments' confs,
        // which is correct behavior but not what we're testing here.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("AddrR", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        let p = Payment::new(inv.id, "tx1".into(), 0, 1000, 100, 1);
        payments::insert(&db, &p).await.unwrap();

        // Tip at 105 = 6 confs
        tick(&db, &cfg(100), false, 105, 5).await.unwrap();
        assert_eq!(
            payments::list_for_invoice(&db, inv.id).await.unwrap()[0].confirmations,
            6
        );
        // Reorg: tip rolls back to 99 (below the payment's height).
        // The payment is effectively orphaned until re-mined.
        tick(&db, &cfg(100), false, 99, 5).await.unwrap();
        assert_eq!(
            payments::list_for_invoice(&db, inv.id).await.unwrap()[0].confirmations,
            0
        );
    }

    #[tokio::test]
    async fn confirming_invoice_with_partial_confirms_below_threshold_stays_confirming() {
        // Two payments totalling 1000, only one mined deep enough. Invoice
        // stays Confirming because the confirmed amount is below the
        // amount due.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr3", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        // deep mined at height 95, tip 100 = 6 confs
        let deep = Payment::new(inv.id, "tx-deep".into(), 0, 400, 95, 1);
        // shallow mined at height 100 (= tip), 1 conf
        let shallow = Payment::new(inv.id, "tx-shallow".into(), 0, 600, 100, 2);
        payments::insert(&db, &deep).await.unwrap();
        payments::insert(&db, &shallow).await.unwrap();

        tick(&db, &cfg(3), false, 100, 5).await.unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        // Threshold 3: only `deep` (6 confs) counts. Confirmed amount
        // is 400, below the 1000 due. Stays Confirming.
        assert_eq!(updated.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn confirmed_invoices_ignored() {
        // Invoice already Confirmed → status stays Confirmed regardless
        // of further chain advancement.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr4", 1000, InvoiceStatus::Confirmed);
        invoices::insert(&db, &inv).await.unwrap();
        let p = Payment::new(inv.id, "tx".into(), 0, 1000, 95, 1);
        payments::insert(&db, &p).await.unwrap();
        tick(&db, &cfg(3), false, 100, 5).await.unwrap();
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirmed);
    }

    #[tokio::test]
    async fn zero_conf_threshold_confirms_mempool_payment() {
        // Threshold = 0 means any payment (even mempool) clears the bar.
        // The invoice transitions Confirming → Confirmed on the next
        // tick regardless of whether the payment has been mined.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr5", 1000, InvoiceStatus::Confirming);
        invoices::insert(&db, &inv).await.unwrap();
        let p = Payment::new(inv.id, "tx".into(), 0, 1000, 0, 1); // mempool
        payments::insert(&db, &p).await.unwrap();
        tick(&db, &cfg(0), false, 100, 5).await.unwrap();
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirmed);
    }
}
