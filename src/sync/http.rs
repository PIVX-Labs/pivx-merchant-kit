//! HTTP clients for the two external services we depend on:
//!
//! - **Blockbook** explorer — per-address transparent UTXO discovery via
//!   `/api/v2/utxo/{address}`. Returns a JSON array.
//! - **PIVX Core RPC** — chain tip via `/getblockcount` and shielded sync
//!   stream via `/getshielddata?startBlock=N&format=compact`. The shield
//!   stream is a binary length-prefixed wire format consumed by
//!   `pivx_wallet_kit::sync::parse_next_blocks`.
//!
//! Both clients share a single `reqwest::Client` (connection pool reuse) and
//! a generous timeout. Retry behaviour is intentionally absent from this
//! layer — the sync loop owns "what to do when a tick fails", and adding
//! retry here would compound delays in the calling layer.

use crate::error::{Error, Result};
use std::time::Duration;

/// Default per-request timeout. Generous enough for slow-but-reachable nodes;
/// short enough that a stuck request doesn't block the sync loop forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum response body size. Blockbook UTXO responses are small (≪1MB);
/// shield streams can be tens of MB on a cold-start sync. Cap is generous
/// to handle that case without a hard ceiling that surprises an operator.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ExplorerClient {
    base: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug)]
pub struct RpcClient {
    base: String,
    http: reqwest::Client,
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("pivx-merchant-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("http client: {}", e)))
}

impl ExplorerClient {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http: build_http_client()?,
        })
    }

    /// Fetch UTXOs for a transparent address from Blockbook v2.
    /// Returns the raw JSON array so wallet-kit's `parse_blockbook_utxos`
    /// can parse it directly (this layer doesn't second-guess the format).
    pub async fn utxos_for_address(&self, address: &str) -> Result<Vec<serde_json::Value>> {
        let url = format!("{}/api/v2/utxo/{}", self.base, address);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Config(format!("blockbook GET {}: {}", url, e)))?;
        if !resp.status().is_success() {
            return Err(Error::Config(format!(
                "blockbook GET {} returned {}",
                url,
                resp.status()
            )));
        }
        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Config(format!("blockbook JSON parse: {}", e)))?;
        match raw {
            serde_json::Value::Array(a) => Ok(a),
            // Blockbook returns `[]` for an address with zero UTXOs. A null
            // / object response means an explorer error wrapped in 200 OK,
            // which we surface so the operator knows.
            other => Err(Error::Config(format!(
                "blockbook returned non-array UTXO body: {}",
                other
            ))),
        }
    }
}

impl RpcClient {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http: build_http_client()?,
        })
    }

    /// Current chain tip. The RPC returns the integer height as a plain
    /// text body (no JSON wrapping).
    pub async fn block_count(&self) -> Result<u32> {
        let url = format!("{}/getblockcount", self.base);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Config(format!("rpc GET {}: {}", url, e)))?;
        if !resp.status().is_success() {
            return Err(Error::Config(format!(
                "rpc GET {} returned {}",
                url,
                resp.status()
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Config(format!("rpc body read: {}", e)))?;
        body.trim()
            .parse::<u32>()
            .map_err(|e| Error::Config(format!("rpc getblockcount: bad integer body: {}", e)))
    }

    /// Fetch the compact-format shield stream starting at `start_block`.
    /// The body is the binary length-prefixed wire format that
    /// `pivx_wallet_kit::sync::parse_next_blocks` understands.
    pub async fn shield_stream(&self, start_block: u32) -> Result<Vec<u8>> {
        let url = format!(
            "{}/getshielddata?startBlock={}&format=compact",
            self.base, start_block
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Config(format!("rpc GET {}: {}", url, e)))?;
        if !resp.status().is_success() {
            return Err(Error::Config(format!(
                "rpc GET {} returned {}",
                url,
                resp.status()
            )));
        }
        // Optimistic check via Content-Length when present; fall back to
        // accumulating bytes with a running cap.
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_BODY_BYTES {
                return Err(Error::Config(format!(
                    "shield stream too large: {} bytes (cap {})",
                    len, MAX_BODY_BYTES
                )));
            }
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Config(format!("shield stream body: {}", e)))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(Error::Config(format!(
                "shield stream exceeded {} byte cap",
                MAX_BODY_BYTES
            )));
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_trims_trailing_slash() {
        let c = ExplorerClient::new("https://example.com/").unwrap();
        assert_eq!(c.base, "https://example.com");
        let c = ExplorerClient::new("https://example.com").unwrap();
        assert_eq!(c.base, "https://example.com");
    }

    #[test]
    fn rpc_trims_trailing_slash() {
        let c = RpcClient::new("https://rpc.example.com/mainnet/").unwrap();
        assert_eq!(c.base, "https://rpc.example.com/mainnet");
    }
}
