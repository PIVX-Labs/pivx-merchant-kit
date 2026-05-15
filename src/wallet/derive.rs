//! HD address derivation hooked to the SQLite cursor.
//!
//! Every new invoice claims the next index from `storage::hd_cursor` and
//! derives an address from it. Transparent uses a straight BIP44 path with
//! a monotonic counter. Shield uses Sapling diversifiers via
//! `wallet-kit::keys::shield_address_at` — the same starting index may
//! yield a slightly higher *used* index because ~50% of diversifiers are
//! invalid by spec, and the caller's cursor advances past skipped indices.

use crate::error::{Error, Result};
use crate::invoice::PaymentChannel;
use crate::storage::{hd_cursor, Db};
use crate::wallet::Wallet;
use pivx_wallet_kit::keys;

/// Result of a derivation: the on-chain address and the HD index used.
/// Persisted on the invoice row so refunds (Stage 7) can re-derive the
/// spending key for this exact slot without a separate index of
/// addresses to keys.
pub struct DerivedAddress {
    pub address: String,
    pub hd_index: u32,
}

/// Derive the next address for `channel`. Atomically advances the cursor
/// in SQLite before deriving — two concurrent invoice creates can't
/// collide on the same HD index. For shield, the cursor may skip ahead by
/// 1+ if the starting diversifier is invalid; the actual used index is
/// what we persist.
pub async fn next_address(
    db: &Db,
    wallet: &Wallet,
    channel: PaymentChannel,
) -> Result<DerivedAddress> {
    let start = hd_cursor::next_index(db, channel).await?;

    match channel {
        PaymentChannel::Transparent => {
            // Transparent derivation is straight BIP44 — every index is
            // valid. `start` is the index we use.
            let seed = crate::wallet::bip39_seed(wallet)?;
            let (address, _pubkey, _privkey) = keys::transparent_key_from_bip39_seed(&seed, 0, start)
                .map_err(|e| Error::Invoice(format!("transparent key derive failed: {}", e)))?;
            Ok(DerivedAddress {
                address,
                hd_index: start,
            })
        }
        PaymentChannel::Shield => {
            // Shield uses Sapling diversifiers — the kit scans forward
            // from `start` and returns the first valid index. If the kit
            // skipped any invalid diversifiers, the cursor should leap
            // past them too so the *next* invoice picks the right
            // starting point.
            let (used_idx, address) =
                keys::shield_address_at(wallet.shield_extfvk(), start).map_err(|e| {
                    Error::Invoice(format!("shield key derive failed: {}", e))
                })?;
            // If the kit picked a higher index than we claimed, fast-forward
            // the cursor so we don't issue the same skipped diversifiers
            // again on subsequent calls. `next_index` already returned `start`
            // and incremented to `start+1`; we need to advance to `used_idx+1`.
            if used_idx > start {
                advance_shield_cursor_to(db, used_idx + 1).await?;
            }
            Ok(DerivedAddress {
                address,
                hd_index: used_idx,
            })
        }
    }
}

/// Push the shield cursor forward to `to_index` (inclusive set point).
/// Used by `next_address` to skip past invalid diversifiers reported by
/// the Sapling library. Only advances — never moves backward.
async fn advance_shield_cursor_to(db: &Db, to_index: u32) -> Result<()> {
    sqlx::query(
        "UPDATE hd_cursor
            SET shield_next = MAX(shield_next, ?)
          WHERE id = 1",
    )
    .bind(to_index as i64)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a Db + Wallet pair for a derivation test.
    async fn test_setup() -> (Db, Wallet) {
        let db = Db::open_memory().await.unwrap();
        let (wallet, _mnemonic) = Wallet::create_new(0).unwrap();
        (db, wallet)
    }

    #[tokio::test]
    async fn transparent_first_derivation_is_index_zero() {
        let (db, wallet) = test_setup().await;
        let d = next_address(&db, &wallet, PaymentChannel::Transparent)
            .await
            .unwrap();
        assert_eq!(d.hd_index, 0);
        // PIVX transparent addresses start with 'D' (mainnet prefix 30).
        assert!(d.address.starts_with('D'), "got: {}", d.address);
    }

    #[tokio::test]
    async fn transparent_derivations_are_distinct_and_advance() {
        let (db, wallet) = test_setup().await;
        let a = next_address(&db, &wallet, PaymentChannel::Transparent)
            .await
            .unwrap();
        let b = next_address(&db, &wallet, PaymentChannel::Transparent)
            .await
            .unwrap();
        let c = next_address(&db, &wallet, PaymentChannel::Transparent)
            .await
            .unwrap();
        assert_eq!(a.hd_index, 0);
        assert_eq!(b.hd_index, 1);
        assert_eq!(c.hd_index, 2);
        assert_ne!(a.address, b.address);
        assert_ne!(b.address, c.address);
    }

    #[tokio::test]
    async fn shield_derivations_produce_valid_addresses() {
        let (db, wallet) = test_setup().await;
        let d = next_address(&db, &wallet, PaymentChannel::Shield)
            .await
            .unwrap();
        // PIVX shield addresses start with "ps".
        assert!(d.address.starts_with("ps"), "got: {}", d.address);
    }

    #[tokio::test]
    async fn shield_derivations_advance_cursor_past_skipped_indices() {
        // Each call must yield a strictly higher hd_index than the last,
        // and consecutive addresses must differ. The cursor in the DB
        // must advance past whatever the diversifier library skipped so
        // we never derive the same diversifier twice.
        let (db, wallet) = test_setup().await;
        let mut prev_idx: Option<u32> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..16 {
            let d = next_address(&db, &wallet, PaymentChannel::Shield)
                .await
                .unwrap();
            if let Some(p) = prev_idx {
                assert!(d.hd_index > p, "non-monotonic shield index: {} <= {}", d.hd_index, p);
            }
            assert!(seen.insert(d.address.clone()), "duplicate shield address: {}", d.address);
            prev_idx = Some(d.hd_index);
        }
        assert_eq!(seen.len(), 16);
    }

    #[tokio::test]
    async fn transparent_and_shield_cursors_advance_independently() {
        // Test invariant: transparent derivations don't bump the shield
        // cursor and vice versa. The actual shield index value depends on
        // which diversifier scan finds first (could be 0, 1, 3, etc. for
        // the wallet's first valid diversifier), so we check the cursor
        // *positions* before and after each call, not specific indices.
        use crate::storage::hd_cursor;
        let (db, wallet) = test_setup().await;

        assert_eq!(hd_cursor::peek(&db, PaymentChannel::Shield).await.unwrap(), 0);

        // Two transparent derivations advance transparent_next, leave
        // shield_next alone.
        let _ = next_address(&db, &wallet, PaymentChannel::Transparent).await.unwrap();
        let _ = next_address(&db, &wallet, PaymentChannel::Transparent).await.unwrap();
        assert_eq!(hd_cursor::peek(&db, PaymentChannel::Transparent).await.unwrap(), 2);
        assert_eq!(hd_cursor::peek(&db, PaymentChannel::Shield).await.unwrap(), 0);

        // A shield derivation advances shield_next but not transparent_next.
        let s = next_address(&db, &wallet, PaymentChannel::Shield).await.unwrap();
        assert_eq!(hd_cursor::peek(&db, PaymentChannel::Transparent).await.unwrap(), 2);
        assert!(
            hd_cursor::peek(&db, PaymentChannel::Shield).await.unwrap() > s.hd_index,
            "shield cursor should be past the used index"
        );
    }
}
