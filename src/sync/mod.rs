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
        }))
    }
}

/// Loop forever, sleeping `poll_interval_secs` between ticks. Failures in
/// either branch log a warning and continue — a network hiccup shouldn't
/// kill the daemon.
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
        tokio::time::sleep(interval).await;
    }
}

/// One full tick. Public so Stage 4's matcher can drive single iterations
/// for integration testing without spawning the loop.
pub async fn run_tick(state: &SyncState) -> Result<()> {
    // Transparent doesn't need the wallet mutex (it only reads invoice DB
    // rows + makes HTTP calls). Run it first since it's fast.
    let discovered = transparent::tick(&state.db, &state.explorer).await?;
    if !discovered.is_empty() {
        tracing::info!(
            count = discovered.len(),
            "transparent sync discovered UTXO records (matcher lands in Stage 4)"
        );
    }

    // Shield mutates wallet state — hold the lock for the duration of the
    // sync iteration. The API layer's reads will queue behind it, which is
    // fine: the API rarely reads wallet state, and when it does it's a
    // millisecond-scale query.
    let shield_result = {
        let mut wallet = state.wallet.lock().await;
        shield::tick(&mut wallet, &state.rpc).await?
    };

    if shield_result.advanced {
        tracing::info!(
            new_notes = shield_result.new_notes.len(),
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
