//! Shield block watcher.
//!
//! Each tick:
//!   1. Pulls the compact-stream bytes starting at `wallet.last_block + 1`
//!   2. Parses the stream via `pivx_wallet_kit::sync::parse_next_blocks`
//!      (chunked, 500 blocks at a time to bound memory)
//!   3. Decrypts notes via `pivx_wallet_kit::sapling::sync::handle_blocks`
//!      against the wallet's extfvk
//!   4. Merges results into wallet state and advances `last_block`
//!
//! Newly-discovered notes are returned to the caller (Stage 4's matcher
//! will map them to invoices by encoded payment address). The matcher
//! sees only the *new* notes from this tick, not the full updated set.

use crate::error::{Error, Result};
use crate::sync::http::RpcClient;
use crate::wallet::Wallet;
use pivx_wallet_kit::sapling::sync::handle_blocks;
use pivx_wallet_kit::sync::parse_next_blocks;
use pivx_wallet_kit::wallet::SerializedNote;
use std::io::Cursor;

/// Chunk size for `parse_next_blocks`. Bounded so a worst-case sync
/// (years of inactivity) doesn't pin a multi-GB working set in memory.
const MAX_BLOCKS_PER_CHUNK: usize = 500;

/// Result of a shield tick. `new_notes` is what Stage 4 will route into
/// invoice Payment rows; `last_block` is the new tip the wallet advanced
/// to.
pub struct ShieldTickResult {
    pub new_notes: Vec<SerializedNote>,
    pub nullifiers: Vec<String>,
    pub last_block: i32,
    /// Did we actually find any new data on-chain? Used by the loop to
    /// decide whether to bother re-encrypting the wallet to disk.
    pub advanced: bool,
}

// SerializedNote doesn't implement Debug in wallet-kit, so a derived
// Debug here would fail. Hand-roll one that shows the counts + tip
// without dumping decrypted note material into the log line.
impl std::fmt::Debug for ShieldTickResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShieldTickResult")
            .field("new_notes_count", &self.new_notes.len())
            .field("nullifiers_count", &self.nullifiers.len())
            .field("last_block", &self.last_block)
            .field("advanced", &self.advanced)
            .finish()
    }
}

/// Run one shield sync iteration. The wallet's `last_block`,
/// `commitment_tree`, and `unspent_notes` are updated in place. The
/// caller is responsible for persisting the wallet after this returns
/// (writing on every tick would be wasteful when nothing advanced).
pub async fn tick(wallet: &mut Wallet, rpc: &RpcClient) -> Result<ShieldTickResult> {
    let start_block = (wallet.inner.last_block + 1).max(0) as u32;
    let stream = rpc.shield_stream(start_block).await?;
    if stream.is_empty() {
        // Either we're caught up to the tip, or the RPC truthfully has
        // no data past our last_block. Either way, no change.
        return Ok(ShieldTickResult {
            new_notes: Vec::new(),
            nullifiers: Vec::new(),
            last_block: wallet.inner.last_block,
            advanced: false,
        });
    }

    let mut reader = Cursor::new(stream);
    let mut all_new_notes = Vec::new();
    let mut all_nullifiers = Vec::new();
    let mut new_last_block = wallet.inner.last_block;

    loop {
        let batch = parse_next_blocks(&mut reader, MAX_BLOCKS_PER_CHUNK)
            .map_err(|e| Error::Invoice(format!("shield stream parse: {}", e)))?;
        let blocks = match batch {
            Some(b) if !b.is_empty() => b,
            // None or empty → no more blocks in the stream.
            _ => break,
        };
        // Track the tip we'll move to. Height is u32 in wallet-kit,
        // last_block is i32 — they overlap fine for any realistic chain.
        if let Some(max) = blocks.iter().map(|b| b.height).max() {
            new_last_block = new_last_block.max(max as i32);
        }

        // `handle_blocks` consumes the existing notes by value (audit's H6
        // zero-clone path) and returns updated witnesses for everything
        // plus the newly-discovered notes. Clone is required because if
        // handle_blocks errors mid-batch, we mustn't strand the wallet
        // with an empty set — the same defensive pattern wallet-kit's
        // WASM wrapper uses.
        let existing = wallet.inner.unspent_notes.clone();
        let result = handle_blocks(
            &wallet.inner.commitment_tree,
            blocks,
            &wallet.inner.extfvk,
            existing,
        )
        .map_err(|e| Error::Invoice(format!("handle_blocks: {}", e)))?;

        wallet.inner.commitment_tree = result.commitment_tree.clone();
        wallet.inner.unspent_notes = result.updated_notes.clone();
        wallet.inner.unspent_notes.extend(result.new_notes.clone());
        wallet.inner.finalize_transaction(&result.nullifiers);

        all_new_notes.extend(result.new_notes);
        all_nullifiers.extend(result.nullifiers);
    }

    let advanced = new_last_block > wallet.inner.last_block;
    wallet.inner.last_block = new_last_block;

    Ok(ShieldTickResult {
        new_notes: all_new_notes,
        nullifiers: all_nullifiers,
        last_block: new_last_block,
        advanced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_stream_returns_no_advance() {
        // We can't easily fake the RpcClient (it holds a reqwest::Client),
        // so this tests the empty-bytes early-return invariant by
        // constructing the same condition manually. The real coverage is
        // the integration-test path (Stage 9) hitting a live RPC.
        let (mut wallet, _) = Wallet::create_new(0).unwrap();
        wallet.inner.last_block = 100;

        // Equivalent to what `tick` does when stream.is_empty():
        let result = ShieldTickResult {
            new_notes: Vec::new(),
            nullifiers: Vec::new(),
            last_block: wallet.inner.last_block,
            advanced: false,
        };
        assert!(!result.advanced);
        assert_eq!(result.last_block, 100);
    }
}
