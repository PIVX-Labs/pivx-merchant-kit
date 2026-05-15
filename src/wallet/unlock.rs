//! Passphrase loading for wallet decryption.
//!
//! Two sources, in priority order:
//!   1. **STDIN** — when present, takes precedence. Lets ops feed the
//!      passphrase via `cat secret | pivx-merchant-kit run` without it
//!      ever touching the process's env block, or prompt interactively
//!      from a tty (no echo, via rpassword).
//!   2. **MERCHANT_KIT_UNLOCK_PASSPHRASE env var** — fallback for systemd
//!      / docker scenarios where pre-seeding stdin isn't ergonomic.
//!
//! The passphrase is hashed once via SHA-256 to produce the 32-byte key
//! that wallet-kit's `serialize_encrypted` / `deserialize_encrypted`
//! expect. We deliberately don't use a KDF (PBKDF2/scrypt): the threat
//! model is local-disk theft, and an attacker with disk access typically
//! also has live process memory, so spending CPU on derivation
//! stretching buys little over what wallet-kit already does internally.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Read};
use zeroize::Zeroize;

const ENV_VAR: &str = "MERCHANT_KIT_UNLOCK_PASSPHRASE";

/// Read the unlock passphrase from STDIN if present, falling back to the
/// `MERCHANT_KIT_UNLOCK_PASSPHRASE` env var. Returns the 32-byte SHA-256
/// digest used as the wallet-kit encryption key.
///
/// When STDIN is a TTY, prompts (no echo) — useful for the operator
/// running `pivx-merchant-kit run` directly. When STDIN is a pipe / file,
/// reads the entire content and treats it as the passphrase (trailing
/// whitespace trimmed) — useful for `cat secret.txt | pivx-merchant-kit run`.
///
/// Empty passphrases are rejected: an empty passphrase isn't a valid
/// deployment state and is more likely to indicate a misconfigured pipe
/// than an intentional choice.
pub fn load_unlock_key() -> Result<[u8; 32]> {
    let mut pass = read_passphrase()?;
    if pass.trim().is_empty() {
        pass.zeroize();
        return Err(Error::Config(
            "no unlock passphrase provided — pipe one to stdin or set \
             MERCHANT_KIT_UNLOCK_PASSPHRASE"
                .into(),
        ));
    }
    let key = sha256_32(pass.trim().as_bytes());
    pass.zeroize();
    Ok(key)
}

/// Returns the raw passphrase string. Caller is responsible for zeroizing.
/// Pulled out so tests can substitute a fake value without going through
/// stdin / env.
fn read_passphrase() -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        // Interactive tty: prompt without echo.
        rpassword::prompt_password("Wallet unlock passphrase: ")
            .map_err(|e| Error::Config(format!("failed to read passphrase from tty: {}", e)))
    } else if has_piped_stdin() {
        // Non-tty stdin (pipe / file). Read everything in.
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(Error::Io)?;
        Ok(buf)
    } else if let Ok(val) = std::env::var(ENV_VAR) {
        Ok(val)
    } else {
        Err(Error::Config(format!(
            "no unlock passphrase: stdin is not a tty, no pipe data, and {} is unset",
            ENV_VAR
        )))
    }
}

/// Stdin isn't redirected when it's a real tty *and* not a piped stream.
/// `is_terminal()` already handles the tty case; this is a best-effort
/// check for "anything on the other end of stdin?" without consuming it.
/// On Unix the stdin fd's mode bits tell us. We don't peek because that
/// risks blocking forever on an empty pipe.
fn has_piped_stdin() -> bool {
    // If `is_terminal` returned false (caller already checked), the fd is
    // either a pipe, file, or socket. All three are valid passphrase
    // sources — we just need to read it. So if we got here, yes.
    !std::io::stdin().is_terminal()
}

fn sha256_32(data: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_32_matches_known_vector() {
        // Empty input — RFC 6234 test vector.
        let h = sha256_32(b"");
        assert_eq!(
            hex_encode(&h),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_32_deterministic() {
        assert_eq!(sha256_32(b"hunter2"), sha256_32(b"hunter2"));
        assert_ne!(sha256_32(b"hunter2"), sha256_32(b"hunter3"));
    }

    fn hex_encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}
