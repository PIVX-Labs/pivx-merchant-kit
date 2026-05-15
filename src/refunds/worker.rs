//! Refund broadcast worker.
//!
//! Picks up `refunds.status = 'pending'` rows where the parent invoice's
//! channel is transparent (shield refund automation is deferred — needs
//! Sapling prover params on disk). For each pending refund:
//!
//!   1. Look up the parent invoice to recover its HD index + address.
//!   2. Fetch the UTXO set at the invoice address from Blockbook.
//!   3. Call `wallet-kit::create_raw_transparent_transaction_from_utxos`
//!      with the wallet's BIP39 seed, the invoice's HD slot, the fetched
//!      UTXOs, the customer's refund_address, and the persisted
//!      `amount_sat` (which already has the fee deducted).
//!   4. Broadcast via the configured PIVX RPC (`sendrawtransaction`).
//!   5. Mark the refund row broadcast with the returned txid.
//!
//! On failure (UTXO fetch, build, broadcast), the row stays pending and
//! we retry on the next tick. There's no backoff cap here — refunds are
//! a small set and a stuck refund is the operator's problem to debug.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::invoice::PaymentChannel;
use crate::refunds::queue::{self, Refund};
use crate::storage::{invoices, Db};
use crate::sync::http::{ExplorerClient, RpcClient};
use crate::sync::SyncState;
use crate::wallet::{bip39_seed, Wallet};
use pivx_wallet_kit::transparent::builder::create_raw_transparent_transaction_from_utxos;
use pivx_wallet_kit::wallet::parse_blockbook_utxos;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Poll interval for the refund worker. Refunds are rare relative to
/// invoice creation; 30s is plenty for responsiveness without hammering
/// the explorer.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

pub async fn run(state: Arc<SyncState>) {
    if !state.config.refunds.enabled {
        tracing::info!("refunds disabled in config — broadcast worker not starting");
        return;
    }
    tracing::info!("refund broadcast worker starting");
    loop {
        if let Err(e) = tick(&state).await {
            tracing::warn!(err = %e, "refund worker tick failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = state.shutdown.notified() => {
                tracing::info!("refund worker received shutdown signal");
                return;
            }
        }
    }
}

async fn tick(state: &SyncState) -> Result<()> {
    let pending = queue::list(&state.db, 100)
        .await?
        .into_iter()
        .filter(|r| r.status == "pending")
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(());
    }

    for refund in pending {
        if let Err(e) = broadcast_one(
            &state.db,
            &state.wallet,
            &state.explorer,
            &state.rpc,
            &state.config,
            &refund,
        )
        .await
        {
            tracing::warn!(
                refund_id = %refund.id,
                err = %e,
                "refund broadcast failed; will retry next tick"
            );
        }
    }
    Ok(())
}

async fn broadcast_one(
    db: &Db,
    wallet: &Mutex<Wallet>,
    explorer: &ExplorerClient,
    rpc: &RpcClient,
    _config: &Config,
    refund: &Refund,
) -> Result<()> {
    // Resolve the parent invoice for its channel + HD slot + source
    // address.
    let invoice = invoices::get(db, refund.invoice_id).await?.ok_or_else(|| {
        Error::Invoice(format!(
            "refund {} references missing invoice {}",
            refund.id, refund.invoice_id
        ))
    })?;

    // Shield refunds need the Sapling prover loaded with proving
    // params — a deployment step we don't ship yet. Leave shield
    // refunds for the operator workflow until then.
    if invoice.channel == PaymentChannel::Shield {
        tracing::debug!(
            refund_id = %refund.id,
            invoice_id = %invoice.id,
            "shield refund — automated broadcast not yet supported, \
             operator can mark broadcast manually via the API"
        );
        return Ok(());
    }

    // Fetch the current UTXO set at the invoice address. We always
    // re-fetch rather than relying on the local payments table because
    // the chain is authoritative — the merchant might have already
    // swept the address manually, or the explorer's view may have
    // changed (reorg).
    let raw = explorer.utxos_for_address(&invoice.address).await?;
    let utxos = parse_blockbook_utxos(&raw);
    if utxos.is_empty() {
        return Err(Error::Invoice(format!(
            "no UTXOs at invoice address {} — already swept?",
            invoice.address
        )));
    }

    // Refine the fee + refund amount now that we know the real UTXO
    // set. Enqueue had to guess (1 input default); the truth might be
    // bigger and the per-byte fee math changes accordingly. Keep the
    // gross refund amount invariant (the customer's expectation —
    // what they paid for the partial case, the excess for overpay)
    // and let the fee absorb the recalculation.
    //
    // gross = refund.amount_sat + refund.fee_sat (what we conceptually
    // owed the customer pre-fee). For partial-expired this equals the
    // customer's actual payment; for overpay it equals the excess.
    let gross_sat = refund.amount_sat + refund.fee_sat;
    let exact_fee = crate::refunds::estimate_fee(utxos.len());
    if gross_sat <= exact_fee {
        // Recomputed fee makes this a dust refund — skip + clean up.
        tracing::info!(
            refund_id = %refund.id,
            gross_sat = gross_sat,
            exact_fee = exact_fee,
            utxos = utxos.len(),
            "refund became dust after fee refinement; marking dead"
        );
        return Ok(());
    }
    let exact_amount = gross_sat - exact_fee;
    if exact_amount != refund.amount_sat || exact_fee != refund.fee_sat {
        queue::update_amount_and_fee(db, refund.id, exact_amount, exact_fee).await?;
        tracing::debug!(
            refund_id = %refund.id,
            old_amount = refund.amount_sat,
            new_amount = exact_amount,
            old_fee = refund.fee_sat,
            new_fee = exact_fee,
            "refund amount/fee refined from actual UTXO count"
        );
    }

    // Build and sign. The wallet holds the seed; we drop the lock as
    // soon as we have the bip39 seed bytes so other tasks aren't
    // blocked on the broadcast.
    let txhex = {
        let wallet_guard = wallet.lock().await;
        let seed = bip39_seed(&wallet_guard)?;
        let result = create_raw_transparent_transaction_from_utxos(
            &seed,
            0, // external chain
            invoice.hd_index,
            &utxos,
            &refund.to_address,
            exact_amount,
        )
        .map_err(|e| Error::Invoice(format!("refund tx build failed: {}", e)))?;
        result.txhex
    };

    // Broadcast. RPC returns the txid on success.
    let txid = rpc.send_raw_transaction(&txhex).await?;

    queue::mark_broadcast(db, refund.id, &txid, unix_now()).await?;
    tracing::info!(
        refund_id = %refund.id,
        invoice_id = %invoice.id,
        to = %refund.to_address,
        amount_sat = exact_amount,
        fee_sat = exact_fee,
        txid = %txid,
        "refund broadcast"
    );
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
