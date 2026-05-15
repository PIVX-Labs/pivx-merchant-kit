//! Sapling proving parameter download + on-disk cache.
//!
//! Loaded once at daemon startup (when shield invoices are enabled in
//! config) so refund tx construction never blocks on a mid-flight
//! ~97MB download. Subsequent restarts read from the on-disk cache.
//!
//! The `verify_and_load_params` call in wallet-kit verifies the SHA256
//! hashes of both files against compiled-in constants before producing
//! a usable prover — operators downloading from a hostile mirror get a
//! load error, not silently broken proofs.

use crate::config::AcceptPolicy;
use crate::error::{Error, Result};
use pivx_wallet_kit::sapling::prover::{verify_and_load_params, SaplingProver};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// CDN mirrors for the params. Same hosts agent-kit uses — the kit
/// verifies hashes, so any compromise of these only causes a load
/// failure, not silent badness.
const PARAM_HOSTS: &[&str] = &["https://pivxla.bz", "https://duddino.com"];

/// Generous timeout for the ~97MB params download. Slow connections
/// shouldn't fail outright.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Decides whether the daemon needs to load Sapling params at startup.
/// Transparent-only deployments skip the ~97MB download entirely.
pub fn shield_enabled(accept: AcceptPolicy) -> bool {
    matches!(accept, AcceptPolicy::Shield | AcceptPolicy::Both)
}

/// Load params from disk if cached, otherwise download from CDN, then
/// SHA256-verify via wallet-kit's `verify_and_load_params`.
///
/// Errors are *not* fatal to the daemon — the caller treats a failure
/// as "shield refund automation unavailable, transparent still works"
/// and logs loudly so the operator notices. Without a prover, shield-
/// channel refund rows stay in `pending` until an operator marks them
/// broadcast manually via the REST API.
pub async fn load_or_download(data_dir: impl AsRef<Path>) -> Result<Arc<SaplingProver>> {
    let dir = data_dir.as_ref().join("params");
    let output_path = dir.join("sapling-output.params");
    let spend_path = dir.join("sapling-spend.params");

    let (output_bytes, spend_bytes) = if output_path.exists() && spend_path.exists() {
        tracing::info!(
            output = %output_path.display(),
            spend = %spend_path.display(),
            "loading sapling params from disk cache"
        );
        (
            std::fs::read(&output_path)?,
            std::fs::read(&spend_path)?,
        )
    } else {
        tracing::info!(
            "sapling params not cached — downloading from CDN (~97MB, one-time)"
        );
        std::fs::create_dir_all(&dir)?;
        let (output, spend) = download_from_any_mirror().await?;
        // Atomic-ish write: write to .tmp then rename. A crash mid-write
        // doesn't leave half-files that fail load on next start.
        atomic_write(&output_path, &output)?;
        atomic_write(&spend_path, &spend)?;
        tracing::info!(
            output_bytes = output.len(),
            spend_bytes = spend.len(),
            "sapling params downloaded and cached"
        );
        (output, spend)
    };

    let prover = verify_and_load_params(&output_bytes, &spend_bytes)
        .map_err(|e| Error::Config(format!("sapling params verify/load: {}", e)))?;
    Ok(Arc::new(prover))
}

async fn download_from_any_mirror() -> Result<(Vec<u8>, Vec<u8>)> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(concat!("pivx-merchant-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("http client build: {}", e)))?;

    let mut last_err: Option<String> = None;
    for host in PARAM_HOSTS {
        match try_download(&client, host).await {
            Ok(pair) => return Ok(pair),
            Err(e) => {
                tracing::warn!(host = %host, err = %e, "sapling params mirror failed, trying next");
                last_err = Some(format!("{}: {}", host, e));
            }
        }
    }
    Err(Error::Config(format!(
        "sapling params download failed from all mirrors: {}",
        last_err.unwrap_or_else(|| "no mirrors configured".into())
    )))
}

async fn try_download(client: &reqwest::Client, host: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let output_url = format!("{}/sapling-output.params", host);
    let spend_url = format!("{}/sapling-spend.params", host);
    let output = fetch_bytes(client, &output_url).await?;
    let spend = fetch_bytes(client, &spend_url).await?;
    Ok((output, spend))
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Config(format!("GET {}: {}", url, e)))?;
    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "GET {} returned {}",
            url,
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Config(format!("read {}: {}", url, e)))?;
    Ok(bytes.to_vec())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("params.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shield_enabled_matches_accept_policy() {
        assert!(!shield_enabled(AcceptPolicy::Transparent));
        assert!(shield_enabled(AcceptPolicy::Shield));
        assert!(shield_enabled(AcceptPolicy::Both));
    }

    // Real download / load coverage is the responsibility of the live
    // e2e flow — a unit test that touches CDN mirrors would either be
    // slow (network) or fake-mock the whole thing (no actual coverage).
}
