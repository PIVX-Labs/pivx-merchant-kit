//! Sync loop driver.
//!
//! Spawned as a tokio task from `cli::run_daemon`. On each tick:
//!   1. Walk all open-invoice transparent addresses, pull their UTXOs
//!      from Blockbook (transparent::tick).
//!   2. Pull the compact shield stream from PIVX Core RPC and apply
//!      decrypted blocks to the wallet (shield::tick).
//!   3. If the wallet advanced, re-encrypt and atomically write
//!      `wallet.json` to disk.
//!
//! The matcher (Stage 4) will hook in to step 1's output and step 2's
//! `new_notes` — turning them into Payment rows and driving state
//! transitions. For Stage 3b, observation is enough.

pub mod http;
pub mod shield;
pub mod transparent;

use crate::config::Config;
use crate::error::Result;
use crate::storage::Db;
use crate::sync::http::{ExplorerClient, RpcClient};
use crate::wallet::Wallet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

/// Shared sync state. Held behind `Arc` so the future captures cheaply
/// and so the API layer (Stage 5) can borrow it from another task.
pub struct SyncState {
    pub db: Db,
    pub wallet: Arc<Mutex<Wallet>>,
    pub wallet_path: PathBuf,
    /// 32-byte unlock key, held in memory so the sync loop can re-encrypt
    /// the wallet after each successful advance. Zeroized on drop.
    pub unlock_key: Zeroizing<[u8; 32]>,
    pub config: Config,
    pub explorer: ExplorerClient,
    pub rpc: RpcClient,
    /// Tripped on Ctrl-C / SIGTERM. Workers check it between ticks
    /// to break out cleanly instead of being aborted mid-write.
    pub shutdown: Arc<tokio::sync::Notify>,
}

impl SyncState {
    pub fn new(
        db: Db,
        wallet: Wallet,
        wallet_path: PathBuf,
        unlock_key: [u8; 32],
        config: Config,
    ) -> Result<Arc<Self>> {
        let explorer = ExplorerClient::new(&config.sync.explorer_url)?;
        let rpc = RpcClient::new(&config.sync.rpc_url)?;
        Ok(Arc::new(Self {
            db,
            wallet: Arc::new(Mutex::new(wallet)),
            wallet_path,
            unlock_key: Zeroizing::new(unlock_key),
            config,
            explorer,
            rpc,
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }))
    }
}

/// Loop until shutdown is signalled, sleeping `poll_interval_secs`
/// between ticks. Failures in any sub-stage log a warning and continue —
/// a network hiccup shouldn't kill the daemon. On shutdown signal,
/// returns cleanly so the caller can persist last state.
pub async fn run(state: Arc<SyncState>) {
    let interval = Duration::from_secs(state.config.sync.poll_interval_secs);
    tracing::info!(
        poll_interval_secs = state.config.sync.poll_interval_secs,
        "sync loop starting"
    );

    loop {
        if let Err(e) = run_tick(&state).await {
            tracing::warn!(err = %e, "sync tick failed");
        }
        // Interruptable sleep — wakes immediately on shutdown.
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = state.shutdown.notified() => {
                tracing::info!("sync loop received shutdown signal");
                return;
            }
        }
    }
}

/// One full tick. Public so integration tests can drive single
/// iterations without spawning the loop.
///
/// Sequence:
///  1. Transparent UTXO discovery → match → state machine
///  2. Shield block sync → match → state machine
///  3. Confirmation-depth update across all in-flight payments
///  4. Expiry sweeper for stale invoices
///  5. Persist wallet on advance
pub async fn run_tick(state: &SyncState) -> Result<()> {
    use crate::matcher;

    let now = unix_now();
    let chain_tip = state.rpc.block_count().await.unwrap_or(0);

    // Transparent doesn't need the wallet mutex (it only reads invoice DB
    // rows + makes HTTP calls). Run it first since it's fast.
    let discovered = transparent::tick(&state.db, &state.explorer).await?;
    let t_matched = matcher::transparent::apply(
        &state.db,
        &state.config.payments,
        discovered,
        now,
    )
    .await?;
    if t_matched > 0 {
        tracing::info!(count = t_matched, "transparent matcher applied");
    }

    // Shield mutates wallet state — hold the lock for the duration of the
    // sync iteration. The API layer's reads will queue behind it, which is
    // fine: the API rarely reads wallet state, and when it does it's a
    // millisecond-scale query.
    let shield_result = {
        let mut wallet = state.wallet.lock().await;
        shield::tick(&mut wallet, &state.rpc).await?
    };
    let s_matched = matcher::shield::apply(
        &state.db,
        &state.config.payments,
        shield_result.new_notes,
        now,
    )
    .await?;
    if s_matched > 0 {
        tracing::info!(count = s_matched, "shield matcher applied");
    }

    // Confirmation depth + Confirming → Confirmed sweep.
    let conf_updated = matcher::confirms::tick(
        &state.db,
        &state.config.payments,
        state.config.refunds.enabled,
        chain_tip,
        now,
    )
    .await?;
    if conf_updated > 0 {
        tracing::debug!(
            updated = conf_updated,
            chain_tip = chain_tip,
            "payment confirmations refreshed"
        );
    }

    // Expiry sweeper.
    let expired = matcher::sweeper::tick(&state.db, &state.config, now).await?;
    if expired > 0 {
        tracing::info!(count = expired, "invoices expired");
    }

    if shield_result.advanced {
        tracing::info!(
            nullifiers = shield_result.nullifiers.len(),
            last_block = shield_result.last_block,
            "shield sync advanced"
        );
        // Persist wallet to disk so a daemon restart doesn't re-sync from
        // scratch. We re-acquire the lock briefly to read the canonical
        // state and write it out.
        let wallet = state.wallet.lock().await;
        wallet.save_encrypted(&state.wallet_path, &state.unlock_key)?;
    }

    Ok(())
}

/// Unix seconds, captured once per tick so all state transitions
/// within a single tick share the same "now" — keeps the partial-
/// payment timer and the expiry sweeper consistent with each other.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
