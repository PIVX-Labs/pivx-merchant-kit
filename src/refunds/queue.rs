//! `refunds` table CRUD.

use crate::error::{Error, Result};
use crate::refunds::RefundReason;
use crate::storage::Db;
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Refund {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub reason: String,
    pub to_address: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub txid: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub broadcast_at: Option<i64>,
    pub confirmed_at: Option<i64>,
}

#[derive(Debug)]
pub struct NewRefund {
    pub invoice_id: Uuid,
    pub reason: RefundReason,
    pub to_address: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
}

pub async fn insert(db: &Db, r: NewRefund) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let now = unix_now();
    let amount = i64::try_from(r.amount_sat)
        .map_err(|_| Error::Invoice("amount_sat overflow".into()))?;
    let fee = i64::try_from(r.fee_sat)
        .map_err(|_| Error::Invoice("fee_sat overflow".into()))?;
    sqlx::query(
        "INSERT INTO refunds (
            id, invoice_id, reason, to_address, amount_sat, fee_sat,
            status, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(id.to_string())
    .bind(r.invoice_id.to_string())
    .bind(r.reason.as_str())
    .bind(r.to_address)
    .bind(amount)
    .bind(fee)
    .bind(now)
    .execute(db.pool())
    .await?;
    Ok(id)
}

pub async fn get(db: &Db, id: Uuid) -> Result<Option<Refund>> {
    let row = sqlx::query(
        "SELECT id, invoice_id, reason, to_address, amount_sat, fee_sat,
                txid, status, created_at, broadcast_at, confirmed_at
           FROM refunds WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(db.pool())
    .await?;
    row.map(row_to_refund).transpose()
}

pub async fn list(db: &Db, limit: i64) -> Result<Vec<Refund>> {
    let rows = sqlx::query(
        "SELECT id, invoice_id, reason, to_address, amount_sat, fee_sat,
                txid, status, created_at, broadcast_at, confirmed_at
           FROM refunds
       ORDER BY created_at DESC
          LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db.pool())
    .await?;
    rows.into_iter().map(row_to_refund).collect()
}

pub async fn list_for_invoice(db: &Db, invoice_id: Uuid) -> Result<Vec<Refund>> {
    let rows = sqlx::query(
        "SELECT id, invoice_id, reason, to_address, amount_sat, fee_sat,
                txid, status, created_at, broadcast_at, confirmed_at
           FROM refunds
          WHERE invoice_id = ?
       ORDER BY created_at ASC",
    )
    .bind(invoice_id.to_string())
    .fetch_all(db.pool())
    .await?;
    rows.into_iter().map(row_to_refund).collect()
}

/// Adjust the recorded amount + fee on a pending refund. Called by the
/// broadcast worker once it has fetched real UTXOs and computed the
/// actual network fee — keeps the persisted row truthful instead of
/// stuck with the enqueue-time estimate.
pub async fn update_amount_and_fee(
    db: &Db,
    id: Uuid,
    amount_sat: u64,
    fee_sat: u64,
) -> Result<()> {
    sqlx::query("UPDATE refunds SET amount_sat = ?, fee_sat = ? WHERE id = ?")
        .bind(i64::try_from(amount_sat).unwrap_or(i64::MAX))
        .bind(i64::try_from(fee_sat).unwrap_or(i64::MAX))
        .bind(id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Mark a refund dead — pulls it out of the worker's pending set so it
/// won't be retried. Used for non-recoverable conditions: the parent
/// invoice was deleted (refund has no destination), or the recomputed
/// fee makes the refund a net-negative (dust). Operator inspects the
/// dead-letter set via the API.
pub async fn mark_dead(db: &Db, id: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE refunds SET status = 'dead' WHERE id = ?",
    )
    .bind(id.to_string())
    .execute(db.pool())
    .await?;
    tracing::info!(refund_id = %id, reason = %reason, "refund marked dead");
    Ok(())
}

/// Mark a refund broadcast with its on-chain txid.
pub async fn mark_broadcast(db: &Db, id: Uuid, txid: &str, now: i64) -> Result<()> {
    sqlx::query(
        "UPDATE refunds
            SET status = 'broadcast',
                txid = ?,
                broadcast_at = ?
          WHERE id = ?",
    )
    .bind(txid)
    .bind(now)
    .bind(id.to_string())
    .execute(db.pool())
    .await?;
    Ok(())
}

fn row_to_refund(row: sqlx::sqlite::SqliteRow) -> Result<Refund> {
    let id: String = row.try_get("id")?;
    let invoice_id: String = row.try_get("invoice_id")?;
    let amount: i64 = row.try_get("amount_sat")?;
    let fee: i64 = row.try_get("fee_sat")?;
    Ok(Refund {
        id: Uuid::parse_str(&id).map_err(|e| Error::Parse(format!("refund id: {}", e)))?,
        invoice_id: Uuid::parse_str(&invoice_id)
            .map_err(|e| Error::Parse(format!("invoice id: {}", e)))?,
        reason: row.try_get("reason")?,
        to_address: row.try_get("to_address")?,
        amount_sat: u64::try_from(amount)
            .map_err(|_| Error::Parse("amount_sat negative".into()))?,
        fee_sat: u64::try_from(fee).map_err(|_| Error::Parse("fee_sat negative".into()))?,
        txid: row.try_get("txid")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        broadcast_at: row.try_get("broadcast_at")?,
        confirmed_at: row.try_get("confirmed_at")?,
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
