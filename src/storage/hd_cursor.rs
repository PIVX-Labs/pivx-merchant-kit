//! HD address-derivation cursor.
//!
//! The wallet derives a fresh address per invoice. To avoid scanning the
//! existing derived range on every new invoice, the daemon persists the
//! next index to use per channel. The single-row constraint on the table
//! schema (`CHECK (id = 1)`) guarantees there's exactly one cursor — no
//! need to filter by id at the application level.
//!
//! `next_index` is a "claim" operation: it atomically returns the current
//! value and increments it, so two concurrent invoice-create requests get
//! distinct indices even under contention.

use crate::error::Result;
use crate::invoice::PaymentChannel;
use crate::storage::Db;

/// Read the current cursor without advancing it. Useful for diagnostics
/// and the daemon's startup log.
pub async fn peek(db: &Db, channel: PaymentChannel) -> Result<u32> {
    let column = column_for(channel);
    let row: (i64,) = sqlx::query_as(&format!("SELECT {} FROM hd_cursor WHERE id = 1", column))
        .fetch_one(db.pool())
        .await?;
    Ok(u32::try_from(row.0).unwrap_or(0))
}

/// Atomically read-and-increment. Returns the index that was current
/// *before* the increment — i.e. the index the caller should now use to
/// derive its address. Two concurrent callers get different values
/// because SQLite serialises writers.
///
/// Implementation: single `UPDATE ... RETURNING` statement. The first
/// version of this used a BEGIN / SELECT / UPDATE / COMMIT sequence,
/// which deadlocked under high contention because the deferred
/// transaction needed a lock upgrade after the SELECT, and 100
/// concurrent tasks all racing for the upgrade saw `SQLITE_BUSY`. The
/// `RETURNING` form is a single statement so SQLite's writer lock
/// covers it end-to-end — no lock-upgrade window, no deadlocks.
pub async fn next_index(db: &Db, channel: PaymentChannel) -> Result<u32> {
    let column = column_for(channel);
    let row: (i64,) = sqlx::query_as(&format!(
        "UPDATE hd_cursor
            SET {col} = {col} + 1
          WHERE id = 1
      RETURNING {col} - 1",
        col = column
    ))
    .fetch_one(db.pool())
    .await?;
    Ok(u32::try_from(row.0).unwrap_or(0))
}

fn column_for(channel: PaymentChannel) -> &'static str {
    match channel {
        PaymentChannel::Transparent => "transparent_next",
        PaymentChannel::Shield => "shield_next",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initial_cursors_are_zero() {
        let db = Db::open_memory().await.unwrap();
        assert_eq!(peek(&db, PaymentChannel::Transparent).await.unwrap(), 0);
        assert_eq!(peek(&db, PaymentChannel::Shield).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn next_index_returns_pre_increment_value() {
        let db = Db::open_memory().await.unwrap();
        let first = next_index(&db, PaymentChannel::Transparent).await.unwrap();
        assert_eq!(first, 0);
        let second = next_index(&db, PaymentChannel::Transparent).await.unwrap();
        assert_eq!(second, 1);
        // Peek reflects the post-increment state.
        assert_eq!(peek(&db, PaymentChannel::Transparent).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn transparent_and_shield_cursors_advance_independently() {
        let db = Db::open_memory().await.unwrap();
        let _ = next_index(&db, PaymentChannel::Transparent).await.unwrap();
        let _ = next_index(&db, PaymentChannel::Transparent).await.unwrap();
        let s = next_index(&db, PaymentChannel::Shield).await.unwrap();
        assert_eq!(s, 0);
        assert_eq!(peek(&db, PaymentChannel::Transparent).await.unwrap(), 2);
        assert_eq!(peek(&db, PaymentChannel::Shield).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_claims_produce_unique_indices() {
        // The whole point of this module: 100 concurrent invoice-create
        // tasks must each get a distinct HD index. If the read+increment
        // wasn't transactional, we'd see duplicates and the matcher would
        // try to derive the same address twice (rejected by the address
        // UNIQUE constraint, but a confusing failure mode).
        let db = Db::open_memory().await.unwrap();
        let mut handles = vec![];
        for _ in 0..100 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                next_index(&db, PaymentChannel::Transparent).await.unwrap()
            }));
        }
        let mut got: Vec<u32> = vec![];
        for h in handles {
            got.push(h.await.unwrap());
        }
        got.sort_unstable();
        let unique: std::collections::HashSet<_> = got.iter().copied().collect();
        assert_eq!(unique.len(), 100, "duplicate HD indices issued");
        assert_eq!(got.first(), Some(&0));
        assert_eq!(got.last(), Some(&99));
    }
}
