//! Wallet bootstrap: create / import / load / save with encrypted persistence.
//!
//! This module owns the lifecycle around `pivx_wallet_kit::wallet::WalletData`:
//! it generates fresh wallets, imports from mnemonics, encrypts/decrypts to
//! disk via the wallet-kit boundary, and provides per-invoice HD address
//! derivation that hooks into the SQLite cursor from Stage 2.
//!
//! The plaintext mnemonic and BIP39 seed live inside `WalletData` and are
//! cleared on drop via zeroize (wallet-kit's discipline). Nothing in
//! merchant-kit holds them outside of method scopes.

pub mod derive;
pub mod unlock;

use crate::error::{Error, Result};
use pivx_wallet_kit::wallet::{
    self as wk_wallet, deserialize_encrypted, import_wallet, serialize_encrypted, WalletData,
};
use std::path::{Path, PathBuf};
use zeroize::Zeroize;

/// File name inside the configured `data_dir` for the encrypted wallet JSON.
pub const WALLET_FILE: &str = "wallet.json";

/// Wraps wallet-kit's WalletData with the bits merchant-kit needs around it.
/// The inner `WalletData` is `pub` for convenience inside the crate, but
/// external callers should go through the helper methods below.
pub struct Wallet {
    pub inner: WalletData,
}

/// Custom Debug that elides the inner WalletData. The struct holds
/// post-decryption secrets (bip39 mnemonic, sapling spending key derivable
/// from the seed), so leaking them through `{:?}` in a log line would
/// undo all the encryption work. Debug output stays useful but harmless.
impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("extfvk_prefix", &self.inner.extfvk.get(..16))
            .field("last_block", &self.inner.last_block)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Create a brand-new wallet from a freshly generated 24-word BIP39
    /// mnemonic. Returns the wallet plus the mnemonic string so the caller
    /// can present it to the operator for backup (first-run only — the
    /// daemon never logs or echoes the mnemonic afterwards).
    ///
    /// The mnemonic is generated here rather than inside wallet-kit
    /// because wallet-kit's `WalletData::mnemonic` is `pub(crate)` —
    /// `create_new_wallet` *does* generate one, but we have no way to read
    /// it back. Generating locally and feeding it into `import_wallet` is
    /// functionally identical and keeps the API symmetric.
    pub fn create_new(current_height: u32) -> Result<(Self, String)> {
        use rand_core::RngCore;
        let mut entropy = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut entropy);
        let mnemonic_obj = bip39::Mnemonic::from_entropy(&entropy)
            .map_err(|e| Error::Invoice(format!("bip39 generation failed: {}", e)))?;
        entropy.zeroize();
        let mnemonic = mnemonic_obj.to_string();
        let data = import_wallet(&mnemonic, current_height)
            .map_err(|e| Error::Invoice(format!("wallet creation failed: {}", e)))?;
        Ok((Self { inner: data }, mnemonic))
    }

    /// Import a wallet from an existing BIP39 mnemonic. The mnemonic is
    /// validated by wallet-kit; invalid phrases return an error.
    pub fn import(mnemonic: &str, current_height: u32) -> Result<Self> {
        let data = import_wallet(mnemonic, current_height)
            .map_err(|e| Error::Invoice(format!("wallet import failed: {}", e)))?;
        Ok(Self { inner: data })
    }

    /// Decrypt and load a wallet from a `wallet.json` file on disk.
    /// Wrong-passphrase failures surface as a clear error from wallet-kit's
    /// `deserialize_encrypted` (which re-derives the extfvk and rejects
    /// mismatches), not as silent garbage.
    pub fn from_encrypted_file(path: impl AsRef<Path>, key: &[u8; 32]) -> Result<Self> {
        let json = std::fs::read_to_string(path.as_ref())?;
        let data = deserialize_encrypted(&json, key)
            .map_err(|e| Error::Invoice(format!("wallet decrypt failed: {}", e)))?;
        Ok(Self { inner: data })
    }

    /// Encrypt and persist the wallet to `wallet.json`. Writes are atomic
    /// (write to tmp, fsync, rename) so a crash mid-write can't leave a
    /// truncated wallet file that fails to decrypt.
    pub fn save_encrypted(&self, path: impl AsRef<Path>, key: &[u8; 32]) -> Result<()> {
        let json = serialize_encrypted(&self.inner, key)
            .map_err(|e| Error::Invoice(format!("wallet encrypt failed: {}", e)))?;
        atomic_write(path.as_ref(), json.as_bytes())?;
        Ok(())
    }

    pub fn shield_extfvk(&self) -> &str {
        &self.inner.extfvk
    }

    /// Resolve `wallet.json` path under the configured data directory.
    pub fn file_in(data_dir: impl AsRef<Path>) -> PathBuf {
        data_dir.as_ref().join(WALLET_FILE)
    }
}

/// Atomic file write — write to `path.tmp`, fsync, then rename over `path`.
/// On filesystems with sane rename semantics (ext4, APFS, NTFS) the rename
/// is the only step that's observable: readers see either the old file or
/// the new file, never a truncated one.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, contents)?;
    // Best-effort fsync of the file; ignore the platform-specific
    // "couldn't open for sync" surface.
    if let Ok(file) = std::fs::File::open(&tmp) {
        let _ = file.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Wraps wallet-kit's `WalletData::get_bip39_seed` to return the kit's
/// `Zeroizing<Vec<u8>>` directly — re-exported here so callers don't need
/// to import wallet-kit themselves for the seed type. Used by derive.rs
/// and Stage 7's refund builder.
pub fn bip39_seed(wallet: &Wallet) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    wallet
        .inner
        .get_bip39_seed()
        .map_err(|e| Error::Invoice(format!("get_bip39_seed: {}", e)))
}

/// Re-export of wallet-kit's `WalletData` for downstream callers that
/// need to drive the inner data structure directly (sync loop, builders).
pub use wk_wallet::WalletData as InnerWalletData;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_new_roundtrips_through_encrypted_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join(WALLET_FILE);
        let key = [0x42u8; 32];

        let (wallet, mnemonic_a) = Wallet::create_new(0).unwrap();
        // 24 words for a fresh wallet (256-bit entropy).
        assert_eq!(mnemonic_a.split_whitespace().count(), 24);
        let extfvk_pre = wallet.shield_extfvk().to_string();
        wallet.save_encrypted(&path, &key).unwrap();

        // Reload from disk should reproduce the same wallet — extfvk is
        // deterministic from the mnemonic, so an equal extfvk proves the
        // secret material round-tripped intact. We can't compare the
        // mnemonic directly because wallet-kit's field is pub(crate).
        let reloaded = Wallet::from_encrypted_file(&path, &key).unwrap();
        assert_eq!(reloaded.shield_extfvk(), extfvk_pre);
    }

    #[test]
    fn import_with_known_mnemonic_produces_deterministic_extfvk() {
        // Same mnemonic, same checkpoint -> same extfvk every time.
        let phrase = "abandon abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon about";
        let a = Wallet::import(phrase, 0).unwrap();
        let b = Wallet::import(phrase, 0).unwrap();
        assert_eq!(a.shield_extfvk(), b.shield_extfvk());
    }

    #[test]
    fn import_rejects_invalid_mnemonic() {
        let err = Wallet::import("definitely not a real bip39 phrase", 0).unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("wallet"));
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join(WALLET_FILE);
        let key = [0x42u8; 32];
        let bad = [0x43u8; 32];

        let (wallet, _) = Wallet::create_new(0).unwrap();
        wallet.save_encrypted(&path, &key).unwrap();

        let err = Wallet::from_encrypted_file(&path, &bad).unwrap_err();
        assert!(format!("{}", err).to_lowercase().contains("decrypt"));
    }

    #[test]
    fn atomic_write_leaves_no_partial_files_on_success() {
        // After a successful save, only the target file exists — no
        // leftover `.tmp` sibling.
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join(WALLET_FILE);
        let key = [0x42u8; 32];
        let (wallet, _) = Wallet::create_new(0).unwrap();
        wallet.save_encrypted(&path, &key).unwrap();

        let entries: Vec<_> = std::fs::read_dir(tmpdir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], WALLET_FILE);
    }
}
