//! Transparent matcher: turns `sync::transparent::DiscoveredUtxo` records
//! into `Payment` rows and drives the invoice state machine.
//!
//! Idempotency lives at the DB layer: the `payments` table's
//! `UNIQUE(txid, vout)` constraint rejects re-insertion of an already-
//! known UTXO, and the matcher swallows that specific error so a
//! polled-twice UTXO doesn't cause noisy log churn.

use crate::config::PaymentsConfig;
use crate::error::{Error, Result};
use crate::payment::Payment;
use crate::storage::{payments, Db};
use crate::sync::transparent::DiscoveredUtxo;

/// Apply a batch of discoveries. Returns the count of newly-inserted
/// payments — duplicates count toward "already seen" and are silently
/// skipped, so this is also a useful "did anything change this tick?"
/// signal.
pub async fn apply(
    db: &Db,
    config: &PaymentsConfig,
    discoveries: Vec<DiscoveredUtxo>,
    now: i64,
) -> Result<usize> {
    let mut inserted = 0usize;
    for d in discoveries {
        let payment = Payment::new(
            d.invoice.id,
            d.utxo.txid.clone(),
            d.utxo.vout,
            d.utxo.amount,
            now,
        );
        match payments::insert(db, &payment).await {
            Ok(()) => {
                inserted += 1;
                tracing::info!(
                    invoice_id = %d.invoice.id,
                    txid = %d.utxo.txid,
                    vout = d.utxo.vout,
                    amount_sat = d.utxo.amount,
                    "new transparent payment matched to invoice"
                );
                super::apply_for(db, &d.invoice, config, now).await?;
            }
            Err(Error::Sqlx(sqlx::Error::Database(e)))
                if e.message().to_lowercase().contains("unique") =>
            {
                // Already-known UTXO. Idempotent re-poll, nothing to do.
                // Don't even log at debug — this is the common case on
                // every tick for any invoice with a pending payment.
            }
            Err(other) => return Err(other),
        }
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcceptPolicy;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use crate::storage::invoices;
    use pivx_wallet_kit::wallet::SerializedUTXO;
    use uuid::Uuid;

    fn cfg() -> PaymentsConfig {
        PaymentsConfig {
            accept: AcceptPolicy::Both,
            confirmations: 3,
            default_expiry_secs: 1800,
            partial_reset_secs: 1800,
        }
    }

    async fn seed_invoice(db: &Db, addr: &str, amount: u64) -> Invoice {
        let inv = Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: amount,
            address: addr.into(),
            hd_index: 0,
            status: InvoiceStatus::Pending,
            created_at: 0,
            expires_at: 100,
            refund_address: None,
            metadata: serde_json::json!({}),
        };
        invoices::insert(db, &inv).await.unwrap();
        inv
    }

    fn utxo(txid: &str, vout: u32, amount: u64) -> SerializedUTXO {
        SerializedUTXO {
            txid: txid.into(),
            vout,
            amount,
            script: String::new(),
            height: 0,
        }
    }

    #[tokio::test]
    async fn applies_single_partial_payment() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "DAddr", 1000).await;
        let count = apply(
            &db,
            &cfg(),
            vec![DiscoveredUtxo {
                invoice: inv.clone(),
                utxo: utxo("tx-a", 0, 400),
            }],
            500,
        )
        .await
        .unwrap();
        assert_eq!(count, 1);

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::PartiallyPaid);
        let stored = payments::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].amount_sat, 400);
    }

    #[tokio::test]
    async fn duplicate_utxo_is_idempotent() {
        // Same UTXO seen twice across two ticks. Second apply should not
        // insert a duplicate Payment row, must not error, and must not
        // shift the invoice into a wrong state.
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "DAddr", 1000).await;
        let utxo_record = DiscoveredUtxo {
            invoice: inv.clone(),
            utxo: utxo("tx-a", 0, 1000),
        };
        let inserted_first = apply(&db, &cfg(), vec![utxo_record.clone()], 500)
            .await
            .unwrap();
        let inserted_second = apply(&db, &cfg(), vec![utxo_record], 500)
            .await
            .unwrap();
        assert_eq!(inserted_first, 1);
        assert_eq!(inserted_second, 0);
        let stored = payments::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn two_partials_drive_invoice_to_confirming() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "DAddr", 1000).await;
        // First partial
        apply(
            &db,
            &cfg(),
            vec![DiscoveredUtxo {
                invoice: inv.clone(),
                utxo: utxo("tx-a", 0, 400),
            }],
            500,
        )
        .await
        .unwrap();
        // Top-up to full
        apply(
            &db,
            &cfg(),
            vec![DiscoveredUtxo {
                invoice: inv.clone(),
                utxo: utxo("tx-b", 0, 600),
            }],
            600,
        )
        .await
        .unwrap();

        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
        let stored = payments::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn overpay_lands_invoice_in_confirming() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "DAddr", 1000).await;
        apply(
            &db,
            &cfg(),
            vec![DiscoveredUtxo {
                invoice: inv.clone(),
                utxo: utxo("tx-over", 0, 1500),
            }],
            500,
        )
        .await
        .unwrap();
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
        // The 500 excess is the refund/donation flow's problem (Stage 7).
    }
}
