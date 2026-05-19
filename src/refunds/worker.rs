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
use crate::wallet::Wallet;
use pivx_wallet_kit::sapling::builder::create_shield_transaction;
use pivx_wallet_kit::sapling::prover::SaplingProver;
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
            state.prover.as_deref(),
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
    prover: Option<&SaplingProver>,
    _config: &Config,
    refund: &Refund,
) -> Result<()> {
    // Resolve the parent invoice for its channel + HD slot + source
    // address.
    let Some(invoice) = invoices::get(db, refund.invoice_id).await? else {
        // Parent invoice is gone (manual delete?). Refund can never
        // succeed — mark dead so we don't retry every 30s forever.
        queue::mark_dead(db, refund.id, "parent invoice missing").await?;
        return Ok(());
    };

    if invoice.channel == PaymentChannel::Shield {
        return broadcast_shield(db, wallet, rpc, prover, refund, &invoice).await;
    }

    // Transparent path from here on. Fetch the current UTXO set at the
    // invoice address — chain is authoritative. We re-fetch every tick
    // rather than trusting the local payments table because the
    // merchant might have manually swept the address, or the
    // explorer's view may have changed (reorg).
    let raw = explorer.utxos_for_address(&invoice.address).await?;
    let utxos = dedupe_utxos(parse_blockbook_utxos(&raw));
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
        // Recomputed fee makes this a dust refund — net would be at
        // or below zero. Mark dead so the worker stops retrying.
        // Operator can see the dead-lettered row via GET /v1/refunds
        // and decide whether the customer needs out-of-band handling.
        queue::mark_dead(
            db,
            refund.id,
            &format!("dust: gross {} <= fee {}", gross_sat, exact_fee),
        )
        .await?;
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
        let seed = crate::wallet::bip39_seed(&wallet_guard)?;
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

/// Dedupe UTXOs by `(txid, vout)`. Blockbook can briefly return the same
/// output twice during indexing — once with `confirmations: 0` (mempool
/// view) and once with the real height — and `parse_blockbook_utxos`
/// doesn't dedupe. Handing both copies to the tx builder produces a
/// `bad-txns-inputs-duplicate` rejection from the node. Keep the first
/// occurrence (insertion order) so the result is stable.
fn dedupe_utxos(utxos: Vec<pivx_wallet_kit::wallet::SerializedUTXO>) -> Vec<pivx_wallet_kit::wallet::SerializedUTXO> {
    let mut seen = std::collections::HashSet::new();
    utxos
        .into_iter()
        .filter(|u| seen.insert((u.txid.clone(), u.vout)))
        .collect()
}

/// Shield refund broadcast path.
///
/// Sapling spending semantics: every note received at any of the wallet's
/// diversified addresses decrypts under the SAME extsk and lands in
/// `wallet.unspent_notes` via the shield sync. So a shield refund doesn't
/// need to "select notes belonging to this invoice" — it just calls
/// wallet-kit's `create_shield_transaction` with the desired amount and
/// destination address. The builder picks notes from the pool, sends
/// the requested amount to the customer's refund address, and routes
/// change back to the wallet's default shield address.
///
/// This means shield refunds work even if the wallet has been swept or
/// rotated between when the invoice was paid and when the refund fires
/// — the customer gets exactly what's owed.
///
/// Requires a loaded Sapling prover. Without one (CDN unreachable at
/// startup, or transparent-only config), this returns Ok() and leaves
/// the refund row pending — the operator can retry by restarting the
/// daemon once the CDN is back.
async fn broadcast_shield(
    db: &Db,
    wallet: &Mutex<Wallet>,
    rpc: &RpcClient,
    prover: Option<&SaplingProver>,
    refund: &Refund,
    invoice: &crate::invoice::Invoice,
) -> Result<()> {
    let Some(prover) = prover else {
        tracing::debug!(
            refund_id = %refund.id,
            invoice_id = %invoice.id,
            "shield refund pending — sapling prover not loaded (CDN unreachable \
             at startup?); retry after daemon restart, or mark broadcast manually \
             via POST /v1/refunds/:id/broadcast"
        );
        return Ok(());
    };

    // Sapling needs the chain tip to anchor the spend at a future
    // block. Use tip + 1 so the tx is valid the moment it's mined.
    let chain_tip = rpc.block_count().await?;
    let target_height = chain_tip.saturating_add(1);

    // Recompute amount using the shield fee schedule. The row was
    // enqueued with the transparent estimator (~2,280 sat) which is
    // ~1000x too low for Sapling — one Spend + two Outputs is ~2.38M
    // sat at the wallet-kit fee-per-byte rate. Without this
    // adjustment the shield builder rejects the refund as
    // insufficient.
    //
    // gross = what the customer originally paid (recovered from the
    // row's amount + fee).
    let gross_sat = refund.amount_sat + refund.fee_sat;
    let shield_fee = pivx_wallet_kit::fees::estimate_fee(0, 0, 1, 2);
    if gross_sat <= shield_fee {
        // Mark dead so the worker stops retrying. With Sapling fees
        // at ~2.4M sat, this fires on any partial smaller than that —
        // the merchant ends up keeping a tiny amount the customer
        // can't economically recover. Operators who care should
        // accept transparent for small-ticket flows.
        queue::mark_dead(
            db,
            refund.id,
            &format!(
                "shield dust: gross {} <= shield_fee {}",
                gross_sat, shield_fee
            ),
        )
        .await?;
        tracing::warn!(
            refund_id = %refund.id,
            invoice_id = %invoice.id,
            gross_sat = gross_sat,
            shield_fee = shield_fee,
            "shield refund would be dust after fee — marked dead"
        );
        return Ok(());
    }
    let exact_amount = gross_sat - shield_fee;
    if exact_amount != refund.amount_sat || shield_fee != refund.fee_sat {
        queue::update_amount_and_fee(db, refund.id, exact_amount, shield_fee).await?;
        tracing::debug!(
            refund_id = %refund.id,
            old_amount = refund.amount_sat,
            new_amount = exact_amount,
            old_fee = refund.fee_sat,
            new_fee = shield_fee,
            "shield refund amount/fee refined to match Sapling fee schedule"
        );
    }

    // create_shield_transaction takes the amount the recipient gets;
    // it picks notes from wallet.unspent_notes itself and computes
    // its own fee via select_shield_notes. As long as our estimate
    // matches the kit's internal estimator (same fees::estimate_fee
    // helper), the builder finds enough notes and produces a clean
    // tx with change back to the wallet's default shield address.
    let txhex = {
        let mut wallet_guard = wallet.lock().await;
        let result = create_shield_transaction(
            &mut wallet_guard.inner,
            &refund.to_address,
            exact_amount,
            "", // empty memo — we could carry the invoice external_id here, but
                // each memo costs an extra Output description (~948k sat fee
                // bump) and the customer's wallet already knows what they paid
                // for. Skip.
            target_height,
            prover,
        )
        .map_err(|e| Error::Invoice(format!("shield refund tx build failed: {}", e)))?;
        result.txhex
    };

    let txid = rpc.send_raw_transaction(&txhex).await?;
    queue::mark_broadcast(db, refund.id, &txid, unix_now()).await?;
    tracing::info!(
        refund_id = %refund.id,
        invoice_id = %invoice.id,
        to = %refund.to_address,
        amount_sat = exact_amount,
        fee_sat = shield_fee,
        txid = %txid,
        "shield refund broadcast"
    );
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
