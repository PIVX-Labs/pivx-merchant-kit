//! `webhook_deliveries` repo: enqueue, fetch-due, mark-delivered,
//! schedule-retry, mark-dead.

use crate::error::{Error, Result};
use crate::webhooks::{EventType, WebhookPayload};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct WebhookDelivery {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub event_type: String,
    pub payload: String,
    pub attempts: u32,
}

pub async fn insert(
    db: &crate::storage::Db,
    invoice_id: Uuid,
    event_type: EventType,
    payload: &WebhookPayload,
    now: i64,
) -> Result<()> {
    let json = serde_json::to_string(payload)?;
    sqlx::query(
        "INSERT INTO webhook_deliveries (
            id, invoice_id, event_type, payload, attempts,
            next_attempt_at, status, created_at
         ) VALUES (?, ?, ?, ?, 0, ?, 'pending', ?)",
    )
    .bind(payload.event_id.to_string())
    .bind(invoice_id.to_string())
    .bind(event_type.as_str())
    .bind(json)
    .bind(now)
    .bind(now)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Fetch up to `limit` pending deliveries whose `next_attempt_at <= now`,
/// ordered oldest-first so a long-stuck delivery doesn't get starved
/// behind newer ones.
pub async fn fetch_due(
    db: &crate::storage::Db,
    now: i64,
    limit: i64,
) -> Result<Vec<WebhookDelivery>> {
    let rows = sqlx::query(
        "SELECT id, invoice_id, event_type, payload, attempts
           FROM webhook_deliveries
          WHERE status = 'pending' AND next_attempt_at <= ?
       ORDER BY next_attempt_at ASC
          LIMIT ?",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(db.pool())
    .await?;

    rows.into_iter()
        .map(|r| {
            let id: String = r.try_get("id")?;
            let invoice_id: String = r.try_get("invoice_id")?;
            let attempts: i64 = r.try_get("attempts")?;
            Ok(WebhookDelivery {
                id: Uuid::parse_str(&id)
                    .map_err(|e| Error::Parse(format!("webhook id: {}", e)))?,
                invoice_id: Uuid::parse_str(&invoice_id)
                    .map_err(|e| Error::Parse(format!("webhook invoice_id: {}", e)))?,
                event_type: r.try_get("event_type")?,
                payload: r.try_get("payload")?,
                attempts: u32::try_from(attempts).unwrap_or(0),
            })
        })
        .collect()
}

pub async fn mark_delivered(
    db: &crate::storage::Db,
    id: Uuid,
    status_code: u16,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries
            SET status = 'delivered',
                attempts = attempts + 1,
                last_status_code = ?,
                last_error = NULL,
                delivered_at = ?
          WHERE id = ?",
    )
    .bind(status_code as i64)
    .bind(now)
    .bind(id.to_string())
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn schedule_retry(
    db: &crate::storage::Db,
    id: Uuid,
    new_attempts: u32,
    next_attempt_at: i64,
    last_error: &str,
    last_status_code: Option<u16>,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries
            SET attempts = ?,
                next_attempt_at = ?,
                last_error = ?,
                last_status_code = ?
          WHERE id = ?",
    )
    .bind(new_attempts as i64)
    .bind(next_attempt_at)
    .bind(last_error)
    .bind(last_status_code.map(|c| c as i64))
    .bind(id.to_string())
    .execute(db.pool())
    .await?;
    Ok(())
}

pub async fn mark_dead(
    db: &crate::storage::Db,
    id: Uuid,
    new_attempts: u32,
    last_error: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE webhook_deliveries
            SET status = 'dead',
                attempts = ?,
                last_error = ?
          WHERE id = ?",
    )
    .bind(new_attempts as i64)
    .bind(last_error)
    .bind(id.to_string())
    .execute(db.pool())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use crate::storage::{invoices, Db};

    /// Seed an invoice into the DB so a webhook FK-referencing it has a
    /// target. The webhook tests don't care what's on the invoice — only
    /// that the row exists.
    async fn seed_invoice(db: &Db, invoice_id: Uuid) {
        let inv = Invoice {
            id: invoice_id,
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1000,
            address: format!("Daddr-{}", invoice_id),
            hd_index: 0,
            status: InvoiceStatus::Confirmed,
            created_at: 1,
            expires_at: 2,
            refund_address: None,
            metadata: serde_json::json!({}),
        };
        invoices::insert(db, &inv).await.unwrap();
    }

    fn payload(invoice_id: Uuid) -> WebhookPayload {
        WebhookPayload {
            event_id: Uuid::new_v4(),
            event_type: EventType::InvoiceConfirmed,
            created_at: 100,
            invoice: crate::api::types::InvoiceResponse {
                id: invoice_id,
                external_id: None,
                channel: crate::invoice::PaymentChannel::Transparent,
                amount_due_sat: 1000,
                amount_paid_sat: 1000,
                address: "Daddr".into(),
                status: crate::invoice::InvoiceStatus::Confirmed,
                created_at: 1,
                expires_at: 2,
                refund_address: None,
                metadata: serde_json::json!({}),
                payments: vec![],
            },
        }
    }

    #[tokio::test]
    async fn insert_then_fetch_due_returns_row() {
        let db = Db::open_memory().await.unwrap();
        let invoice_id = Uuid::new_v4();
        seed_invoice(&db, invoice_id).await;
        let p = payload(invoice_id);
        let event_id = p.event_id;
        insert(&db, invoice_id, EventType::InvoiceConfirmed, &p, 100)
            .await
            .unwrap();
        let due = fetch_due(&db, 9999, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, event_id);
        assert_eq!(due[0].event_type, "invoice.confirmed");
    }

    #[tokio::test]
    async fn fetch_due_respects_next_attempt_window() {
        let db = Db::open_memory().await.unwrap();
        let invoice_id = Uuid::new_v4();
        seed_invoice(&db, invoice_id).await;
        insert(
            &db,
            invoice_id,
            EventType::InvoiceConfirmed,
            &payload(invoice_id),
            100,
        )
        .await
        .unwrap();
        // Schedule retry far in the future.
        let due_now = fetch_due(&db, 9999, 10).await.unwrap();
        let id = due_now[0].id;
        schedule_retry(&db, id, 1, 1_000_000_000, "boom", Some(500))
            .await
            .unwrap();
        // Before the window, no rows.
        let before = fetch_due(&db, 500, 10).await.unwrap();
        assert!(before.is_empty());
        // After the window, the row resurfaces.
        let after = fetch_due(&db, 1_000_000_001, 10).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].attempts, 1);
    }

    #[tokio::test]
    async fn mark_delivered_removes_from_pending_set() {
        let db = Db::open_memory().await.unwrap();
        let invoice_id = Uuid::new_v4();
        seed_invoice(&db, invoice_id).await;
        let p = payload(invoice_id);
        let event_id = p.event_id;
        insert(&db, invoice_id, EventType::InvoiceConfirmed, &p, 100)
            .await
            .unwrap();
        mark_delivered(&db, event_id, 200, 123).await.unwrap();
        let due = fetch_due(&db, 9999, 10).await.unwrap();
        assert!(due.is_empty());
    }

    #[tokio::test]
    async fn mark_dead_removes_from_pending_set() {
        let db = Db::open_memory().await.unwrap();
        let invoice_id = Uuid::new_v4();
        seed_invoice(&db, invoice_id).await;
        let p = payload(invoice_id);
        let event_id = p.event_id;
        insert(&db, invoice_id, EventType::InvoiceConfirmed, &p, 100)
            .await
            .unwrap();
        mark_dead(&db, event_id, 10, "max attempts").await.unwrap();
        let due = fetch_due(&db, 9999, 10).await.unwrap();
        assert!(due.is_empty());
    }
}
