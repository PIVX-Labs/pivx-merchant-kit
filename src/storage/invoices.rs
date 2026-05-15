//! Invoice CRUD.
//!
//! All functions take `&Db` rather than a `SqlitePool` so the public API
//! is uniform across repos and the implementation can later swap in a
//! transaction-aware variant without changing call sites.

use crate::error::{Error, Result};
use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
use crate::storage::Db;
use sqlx::Row;
use std::str::FromStr;
use uuid::Uuid;

pub async fn insert(db: &Db, invoice: &Invoice) -> Result<()> {
    let metadata = serde_json::to_string(&invoice.metadata)?;
    let amount_due = i64::try_from(invoice.amount_due_sat).map_err(|_| {
        Error::Invoice("amount_due_sat exceeds i64 range (SQLite INTEGER ceiling)".into())
    })?;
    sqlx::query(
        "INSERT INTO invoices (
            id, external_id, channel, amount_due_sat, address, hd_index,
            status, refund_address, metadata, created_at, expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(invoice.id.to_string())
    .bind(invoice.external_id.as_deref())
    .bind(invoice.channel.as_str())
    .bind(amount_due)
    .bind(&invoice.address)
    .bind(0_i64) // hd_index — Stage 3 wires this through; placeholder for now
    .bind(invoice.status.as_str())
    .bind(invoice.refund_address.as_deref())
    .bind(metadata)
    .bind(invoice.created_at)
    .bind(invoice.expires_at)
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn get(db: &Db, id: Uuid) -> Result<Option<Invoice>> {
    let row = sqlx::query(
        "SELECT id, external_id, channel, amount_due_sat, address, status,
                refund_address, metadata, created_at, expires_at
           FROM invoices
          WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(db.pool())
    .await?;
    row.map(row_to_invoice).transpose()
}

pub async fn get_by_external_id(db: &Db, external_id: &str) -> Result<Option<Invoice>> {
    let row = sqlx::query(
        "SELECT id, external_id, channel, amount_due_sat, address, status,
                refund_address, metadata, created_at, expires_at
           FROM invoices
          WHERE external_id = ?",
    )
    .bind(external_id)
    .fetch_optional(db.pool())
    .await?;
    row.map(row_to_invoice).transpose()
}

pub async fn get_by_address(db: &Db, address: &str) -> Result<Option<Invoice>> {
    let row = sqlx::query(
        "SELECT id, external_id, channel, amount_due_sat, address, status,
                refund_address, metadata, created_at, expires_at
           FROM invoices
          WHERE address = ?",
    )
    .bind(address)
    .fetch_optional(db.pool())
    .await?;
    row.map(row_to_invoice).transpose()
}

pub async fn update_status(db: &Db, id: Uuid, status: InvoiceStatus) -> Result<()> {
    sqlx::query("UPDATE invoices SET status = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn update_expires_at(db: &Db, id: Uuid, expires_at: i64) -> Result<()> {
    sqlx::query("UPDATE invoices SET expires_at = ? WHERE id = ?")
        .bind(expires_at)
        .bind(id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Selection filter for `list`. Keeping it a small struct rather than a long
/// arg list lets callers (the API layer in Stage 5) extend without churn.
#[derive(Debug, Default, Clone)]
pub struct InvoiceFilter {
    pub status: Option<InvoiceStatus>,
    pub limit: Option<i64>,
}

pub async fn list(db: &Db, filter: InvoiceFilter) -> Result<Vec<Invoice>> {
    let limit = filter.limit.unwrap_or(100).clamp(1, 1000);
    let rows = match filter.status {
        Some(s) => {
            sqlx::query(
                "SELECT id, external_id, channel, amount_due_sat, address, status,
                        refund_address, metadata, created_at, expires_at
                   FROM invoices
                  WHERE status = ?
               ORDER BY created_at DESC
                  LIMIT ?",
            )
            .bind(s.as_str())
            .bind(limit)
            .fetch_all(db.pool())
            .await?
        }
        None => {
            sqlx::query(
                "SELECT id, external_id, channel, amount_due_sat, address, status,
                        refund_address, metadata, created_at, expires_at
                   FROM invoices
               ORDER BY created_at DESC
                  LIMIT ?",
            )
            .bind(limit)
            .fetch_all(db.pool())
            .await?
        }
    };
    rows.into_iter().map(row_to_invoice).collect()
}

/// Find invoices that are due for status review by the expiry sweeper.
/// Returns non-terminal invoices whose `expires_at <= now`.
pub async fn list_expirable(db: &Db, now: i64) -> Result<Vec<Invoice>> {
    let rows = sqlx::query(
        "SELECT id, external_id, channel, amount_due_sat, address, status,
                refund_address, metadata, created_at, expires_at
           FROM invoices
          WHERE expires_at <= ?
            AND status IN ('pending', 'partially_paid')",
    )
    .bind(now)
    .fetch_all(db.pool())
    .await?;
    rows.into_iter().map(row_to_invoice).collect()
}

fn row_to_invoice(row: sqlx::sqlite::SqliteRow) -> Result<Invoice> {
    let id_str: String = row.try_get("id")?;
    let channel_str: String = row.try_get("channel")?;
    let status_str: String = row.try_get("status")?;
    let amount_due: i64 = row.try_get("amount_due_sat")?;
    let metadata_str: String = row.try_get("metadata")?;
    Ok(Invoice {
        id: Uuid::parse_str(&id_str)
            .map_err(|e| Error::Parse(format!("invoice id not a UUID: {}", e)))?,
        external_id: row.try_get("external_id")?,
        channel: PaymentChannel::from_str(&channel_str)?,
        amount_due_sat: u64::try_from(amount_due)
            .map_err(|_| Error::Parse("negative amount_due_sat".into()))?,
        address: row.try_get("address")?,
        status: InvoiceStatus::from_str(&status_str)?,
        refund_address: row.try_get("refund_address")?,
        metadata: serde_json::from_str(&metadata_str)?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Db;

    fn sample_invoice(addr: &str) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 100_000_000, // 1 PIV
            address: addr.into(),
            status: InvoiceStatus::Pending,
            created_at: 1_700_000_000,
            expires_at: 1_700_001_800,
            refund_address: None,
            metadata: serde_json::json!({"order": "ORD-1"}),
        }
    }

    #[tokio::test]
    async fn insert_then_get_roundtrip() {
        let db = Db::open_memory().await.unwrap();
        let inv = sample_invoice("DAddrA");
        insert(&db, &inv).await.unwrap();
        let got = get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(got.id, inv.id);
        assert_eq!(got.address, "DAddrA");
        assert_eq!(got.amount_due_sat, 100_000_000);
        assert_eq!(got.channel, PaymentChannel::Transparent);
        assert_eq!(got.status, InvoiceStatus::Pending);
        assert_eq!(got.metadata["order"], "ORD-1");
    }

    #[tokio::test]
    async fn duplicate_address_rejected() {
        let db = Db::open_memory().await.unwrap();
        let a = sample_invoice("DAddrB");
        let mut b = sample_invoice("DAddrB");
        b.id = Uuid::new_v4();
        insert(&db, &a).await.unwrap();
        let err = insert(&db, &b).await.unwrap_err();
        // SQLite reports as UNIQUE constraint failed
        assert!(format!("{}", err).to_lowercase().contains("unique"));
    }

    #[tokio::test]
    async fn duplicate_external_id_rejected() {
        let db = Db::open_memory().await.unwrap();
        let mut a = sample_invoice("DAddrC");
        a.external_id = Some("ord-42".into());
        let mut b = sample_invoice("DAddrD");
        b.id = Uuid::new_v4();
        b.external_id = Some("ord-42".into());
        insert(&db, &a).await.unwrap();
        let err = insert(&db, &b).await.unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("unique"));
    }

    #[tokio::test]
    async fn get_by_external_id_finds_match() {
        let db = Db::open_memory().await.unwrap();
        let mut inv = sample_invoice("DAddrE");
        inv.external_id = Some("ord-99".into());
        insert(&db, &inv).await.unwrap();
        let got = get_by_external_id(&db, "ord-99").await.unwrap().unwrap();
        assert_eq!(got.id, inv.id);
        assert!(get_by_external_id(&db, "ord-nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_by_address_finds_match() {
        let db = Db::open_memory().await.unwrap();
        let inv = sample_invoice("DAddrF");
        insert(&db, &inv).await.unwrap();
        let got = get_by_address(&db, "DAddrF").await.unwrap().unwrap();
        assert_eq!(got.id, inv.id);
    }

    #[tokio::test]
    async fn update_status_persists() {
        let db = Db::open_memory().await.unwrap();
        let inv = sample_invoice("DAddrG");
        insert(&db, &inv).await.unwrap();
        update_status(&db, inv.id, InvoiceStatus::Confirming)
            .await
            .unwrap();
        let got = get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(got.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn update_expires_at_persists() {
        let db = Db::open_memory().await.unwrap();
        let inv = sample_invoice("DAddrH");
        insert(&db, &inv).await.unwrap();
        update_expires_at(&db, inv.id, 1_700_005_000).await.unwrap();
        let got = get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(got.expires_at, 1_700_005_000);
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let db = Db::open_memory().await.unwrap();
        let a = sample_invoice("DAddr1");
        let mut b = sample_invoice("DAddr2");
        b.id = Uuid::new_v4();
        b.status = InvoiceStatus::Confirming;
        insert(&db, &a).await.unwrap();
        insert(&db, &b).await.unwrap();

        let pending = list(
            &db,
            InvoiceFilter {
                status: Some(InvoiceStatus::Pending),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].address, "DAddr1");

        let all = list(&db, InvoiceFilter::default()).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_expirable_only_returns_non_terminal_past_expiry() {
        let db = Db::open_memory().await.unwrap();
        // expired pending
        let mut a = sample_invoice("Expiring1");
        a.expires_at = 100;
        // not yet expired
        let mut b = sample_invoice("NotYet");
        b.id = Uuid::new_v4();
        b.expires_at = 9999;
        // already confirmed — should never be in the expirable set
        let mut c = sample_invoice("AlreadyConfirmed");
        c.id = Uuid::new_v4();
        c.expires_at = 100;
        c.status = InvoiceStatus::Confirmed;
        // partial + expired — should also surface
        let mut d = sample_invoice("PartialExpired");
        d.id = Uuid::new_v4();
        d.expires_at = 100;
        d.status = InvoiceStatus::PartiallyPaid;
        insert(&db, &a).await.unwrap();
        insert(&db, &b).await.unwrap();
        insert(&db, &c).await.unwrap();
        insert(&db, &d).await.unwrap();

        let expirable = list_expirable(&db, 500).await.unwrap();
        let addrs: Vec<&str> = expirable.iter().map(|i| i.address.as_str()).collect();
        assert!(addrs.contains(&"Expiring1"));
        assert!(addrs.contains(&"PartialExpired"));
        assert!(!addrs.contains(&"NotYet"));
        assert!(!addrs.contains(&"AlreadyConfirmed"));
    }

    #[tokio::test]
    async fn concurrent_inserts_serialise_safely() {
        // SQLite's writer lock means concurrent INSERTs from multiple tasks
        // shouldn't corrupt anything — they queue. Verify by spawning N
        // tasks that each insert and reading the final count back.
        let db = Db::open_memory().await.unwrap();
        let mut handles = vec![];
        for i in 0..20 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                let inv = sample_invoice(&format!("Addr-{:03}", i));
                insert(&db, &inv).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let all = list(&db, InvoiceFilter::default()).await.unwrap();
        assert_eq!(all.len(), 20);
    }
}
