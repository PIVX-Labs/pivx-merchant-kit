//! Payment CRUD.
//!
//! Payments are append-mostly: one row per on-chain output. The matcher
//! inserts new rows as it observes outputs, and updates `confirmations`
//! as the chain advances. Deletes only happen on test setup; production
//! never deletes a payment row (chain-finalized data should be auditable).

use crate::error::{Error, Result};
use crate::payment::Payment;
use crate::storage::Db;
use sqlx::Row;
use uuid::Uuid;

pub async fn insert(db: &Db, payment: &Payment) -> Result<()> {
    let amount = i64::try_from(payment.amount_sat).map_err(|_| {
        Error::Invoice("amount_sat exceeds i64 range (SQLite INTEGER ceiling)".into())
    })?;
    sqlx::query(
        "INSERT INTO payments (
            id, invoice_id, txid, vout, amount_sat, confirmations,
            seen_at, confirmed_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(payment.id.to_string())
    .bind(payment.invoice_id.to_string())
    .bind(&payment.txid)
    .bind(payment.vout as i64)
    .bind(amount)
    .bind(payment.confirmations as i64)
    .bind(payment.seen_at)
    .bind(payment.confirmed_at)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Sum of all observed payment values for an invoice, regardless of
/// confirmation depth. Drives the underpay/exact/overpay decision in
/// `next_status_on_payment`.
pub async fn total_amount_for_invoice(db: &Db, invoice_id: Uuid) -> Result<u64> {
    let row: (Option<i64>,) =
        sqlx::query_as("SELECT SUM(amount_sat) FROM payments WHERE invoice_id = ?")
            .bind(invoice_id.to_string())
            .fetch_one(db.pool())
            .await?;
    Ok(row.0.unwrap_or(0).max(0) as u64)
}

/// Sum of confirmed-only payment values (confirmations >= threshold). Drives
/// the Confirming → Confirmed transition.
pub async fn confirmed_amount_for_invoice(
    db: &Db,
    invoice_id: Uuid,
    threshold: u32,
) -> Result<u64> {
    let row: (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(amount_sat) FROM payments
           WHERE invoice_id = ? AND confirmations >= ?",
    )
    .bind(invoice_id.to_string())
    .bind(threshold as i64)
    .fetch_one(db.pool())
    .await?;
    Ok(row.0.unwrap_or(0).max(0) as u64)
}

pub async fn list_for_invoice(db: &Db, invoice_id: Uuid) -> Result<Vec<Payment>> {
    let rows = sqlx::query(
        "SELECT id, invoice_id, txid, vout, amount_sat, confirmations,
                seen_at, confirmed_at
           FROM payments
          WHERE invoice_id = ?
       ORDER BY seen_at ASC",
    )
    .bind(invoice_id.to_string())
    .fetch_all(db.pool())
    .await?;
    rows.into_iter().map(row_to_payment).collect()
}

pub async fn update_confirmations(
    db: &Db,
    payment_id: Uuid,
    confirmations: u32,
    confirmed_at: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE payments
            SET confirmations = ?,
                confirmed_at = COALESCE(confirmed_at, ?)
          WHERE id = ?",
    )
    .bind(confirmations as i64)
    .bind(confirmed_at)
    .bind(payment_id.to_string())
    .execute(db.pool())
    .await?;
    Ok(())
}

fn row_to_payment(row: sqlx::sqlite::SqliteRow) -> Result<Payment> {
    let id_str: String = row.try_get("id")?;
    let invoice_id_str: String = row.try_get("invoice_id")?;
    let amount: i64 = row.try_get("amount_sat")?;
    let confs: i64 = row.try_get("confirmations")?;
    let vout: i64 = row.try_get("vout")?;
    Ok(Payment {
        id: Uuid::parse_str(&id_str)
            .map_err(|e| Error::Parse(format!("payment id not a UUID: {}", e)))?,
        invoice_id: Uuid::parse_str(&invoice_id_str)
            .map_err(|e| Error::Parse(format!("invoice_id not a UUID: {}", e)))?,
        txid: row.try_get("txid")?,
        vout: u32::try_from(vout).map_err(|_| Error::Parse("vout out of range".into()))?,
        amount_sat: u64::try_from(amount)
            .map_err(|_| Error::Parse("negative amount_sat".into()))?,
        confirmations: u32::try_from(confs)
            .map_err(|_| Error::Parse("confirmations out of range".into()))?,
        seen_at: row.try_get("seen_at")?,
        confirmed_at: row.try_get("confirmed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use crate::storage::invoices;

    async fn seed_invoice(db: &Db, addr: &str) -> Invoice {
        let inv = Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1_000_000,
            address: addr.into(),
            hd_index: 0,
            status: InvoiceStatus::Pending,
            created_at: 1_700_000_000,
            expires_at: 1_700_001_800,
            refund_address: None,
            metadata: serde_json::json!({}),
        };
        invoices::insert(db, &inv).await.unwrap();
        inv
    }

    #[tokio::test]
    async fn insert_then_list_roundtrip() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr1").await;
        let p1 = Payment::new(inv.id, "txhash-a".into(), 0, 400_000, 1_700_000_100);
        let p2 = Payment::new(inv.id, "txhash-b".into(), 1, 600_000, 1_700_000_200);
        insert(&db, &p1).await.unwrap();
        insert(&db, &p2).await.unwrap();

        let listed = list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].txid, "txhash-a");
        assert_eq!(listed[1].txid, "txhash-b");
    }

    #[tokio::test]
    async fn total_sums_all_payments() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr2").await;
        insert(
            &db,
            &Payment::new(inv.id, "tx-a".into(), 0, 400_000, 1_700_000_100),
        )
        .await
        .unwrap();
        insert(
            &db,
            &Payment::new(inv.id, "tx-b".into(), 0, 600_000, 1_700_000_200),
        )
        .await
        .unwrap();
        let total = total_amount_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(total, 1_000_000);
    }

    #[tokio::test]
    async fn total_for_invoice_with_no_payments_is_zero() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr3").await;
        let total = total_amount_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn confirmed_amount_only_counts_above_threshold() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr4").await;
        let mut deep = Payment::new(inv.id, "tx-deep".into(), 0, 700_000, 1_700_000_100);
        deep.confirmations = 5;
        let mut shallow = Payment::new(inv.id, "tx-shallow".into(), 0, 300_000, 1_700_000_200);
        shallow.confirmations = 1;
        insert(&db, &deep).await.unwrap();
        insert(&db, &shallow).await.unwrap();

        // threshold 3: only `deep` counts
        assert_eq!(
            confirmed_amount_for_invoice(&db, inv.id, 3).await.unwrap(),
            700_000
        );
        // threshold 1: both count
        assert_eq!(
            confirmed_amount_for_invoice(&db, inv.id, 1).await.unwrap(),
            1_000_000
        );
        // threshold 6: neither
        assert_eq!(
            confirmed_amount_for_invoice(&db, inv.id, 6).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn duplicate_txid_vout_rejected() {
        // A single on-chain output funding two invoices makes no sense and
        // would skew totals. The UNIQUE(txid, vout) constraint stops it.
        let db = Db::open_memory().await.unwrap();
        let a = seed_invoice(&db, "PAddr5a").await;
        let b = seed_invoice(&db, "PAddr5b").await;
        let p1 = Payment::new(a.id, "dup-tx".into(), 7, 100, 1);
        let p2 = Payment::new(b.id, "dup-tx".into(), 7, 100, 2);
        insert(&db, &p1).await.unwrap();
        let err = insert(&db, &p2).await.unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("unique"));
    }

    #[tokio::test]
    async fn update_confirmations_advances_count_and_seals_confirmed_at() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr6").await;
        let p = Payment::new(inv.id, "tx-conf".into(), 0, 500_000, 1_700_000_100);
        insert(&db, &p).await.unwrap();

        update_confirmations(&db, p.id, 1, Some(1_700_000_200))
            .await
            .unwrap();
        let listed = list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(listed[0].confirmations, 1);
        assert_eq!(listed[0].confirmed_at, Some(1_700_000_200));

        // Calling again with a later confirmed_at should NOT overwrite —
        // the original confirmed_at is preserved via COALESCE.
        update_confirmations(&db, p.id, 5, Some(1_700_000_999))
            .await
            .unwrap();
        let listed = list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(listed[0].confirmations, 5);
        assert_eq!(listed[0].confirmed_at, Some(1_700_000_200));
    }

    #[tokio::test]
    async fn cascading_delete_removes_payments() {
        // Per the FK definition, deleting an invoice cascades to its
        // payments. We rely on this only in tests / dev resets; production
        // never deletes invoice rows.
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice(&db, "PAddr7").await;
        insert(
            &db,
            &Payment::new(inv.id, "tx-c".into(), 0, 100, 1_700_000_100),
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM invoices WHERE id = ?")
            .bind(inv.id.to_string())
            .execute(db.pool())
            .await
            .unwrap();
        let listed = list_for_invoice(&db, inv.id).await.unwrap();
        assert!(listed.is_empty());
    }
}
