//! SQLite-backed persistence.
//!
//! Single-file deployment: one DB holds invoices, payments, HD cursor,
//! webhook delivery queue, and refund records. Migrations live in
//! `./migrations/` and are applied at startup via [`Db::open`] — operators
//! never need to run migration commands by hand.
//!
//! Repo functions take a `&Db` so callers can hold multiple pools across
//! their components without leaking the `sqlx::SqlitePool` type. Internally
//! the `Db` is just a transparent wrapper around the pool.

use crate::error::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

pub mod hd_cursor;
pub mod invoices;
pub mod payments;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (or create) the SQLite database at `path`, applying any pending
    /// migrations. `WAL` mode is enabled so the API handler and the sync
    /// loop can read while the matcher writes without blocking each other.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let url = format!("sqlite://{}", path.as_ref().display());
        Self::open_url(&url).await
    }

    /// Open with a raw `sqlite://` or `sqlite::memory:` URL. Used by tests
    /// to spin up an in-memory DB without touching disk.
    pub async fn open_url(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            // Cap connections — SQLite can't actually parallelise writes
            // (single-writer lock), so a fat pool just queues. The reads
            // share connections fine via WAL.
            .max_connections(8)
            .connect_with(opts)
            .await?;

        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// In-memory DB for tests. Each call returns a fresh, isolated DB
    /// with migrations applied — no shared state between tests.
    #[cfg(test)]
    pub async fn open_memory() -> Result<Self> {
        Self::open_url("sqlite::memory:").await
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_in_memory_with_migrations() {
        let db = Db::open_memory().await.unwrap();
        // hd_cursor's seed row should exist after migration.
        let row: (i64, i64) =
            sqlx::query_as("SELECT transparent_next, shield_next FROM hd_cursor WHERE id = 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(row, (0, 0));
    }

    #[tokio::test]
    async fn opens_on_disk_with_migrations() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();
        // NamedTempFile keeps the file open with a handle on some platforms;
        // dropping it deletes the file. Pass just the path to Db::open and
        // let SQLite take over.
        drop(tmp);
        let db = Db::open(&path).await.unwrap();
        // Re-opening should work without error and find the existing schema.
        let db2 = Db::open(&path).await.unwrap();
        // Sanity: both pools hit the same schema.
        let n1: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM invoices")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let n2: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM invoices")
            .fetch_one(db2.pool())
            .await
            .unwrap();
        assert_eq!(n1, n2);
        let _ = std::fs::remove_file(&path);
    }
}
