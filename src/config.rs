//! TOML config for the daemon.
//!
//! Every option that affects business logic — confirmation depth, partial
//! payment behaviour, refund policy — lives here so operators can tune them
//! without code changes. Defaults are conservative (3 confirmations, refunds
//! off, transparent+shield both accepted).

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub network: NetworkConfig,
    pub wallet: WalletConfig,
    pub sync: SyncConfig,
    pub payments: PaymentsConfig,
    #[serde(default)]
    pub refunds: RefundsConfig,
    pub api: ApiConfig,
    pub webhooks: WebhooksConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkConfig {
    /// `"mainnet"` is the only supported value today. Testnet support is
    /// planned but gated behind wallet-kit testnet params.
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletConfig {
    /// Directory the daemon owns. Encrypted wallet state, SQLite db, and the
    /// HD address-pool cursor all live here. The daemon will create it if
    /// missing.
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncConfig {
    /// PIVX Core RPC endpoint exposing the compact-stream API used by
    /// wallet-kit for shield sync.
    pub rpc_url: String,
    /// Blockbook explorer used for transparent UTXO discovery. Can be the
    /// same host as `rpc_url` if the operator runs a combined node, but is
    /// kept separate so each can be swapped independently.
    pub explorer_url: String,
    /// How often the sync loop polls the chain for new blocks / UTXO state.
    #[serde(default = "defaults::poll_interval_secs")]
    pub poll_interval_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptPolicy {
    /// Only transparent invoices may be created.
    Transparent,
    /// Only shield invoices may be created.
    Shield,
    /// Both channels are available; the caller picks per-invoice.
    Both,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaymentsConfig {
    /// Which payment channels the daemon will offer to API callers.
    pub accept: AcceptPolicy,
    /// Confirmations required before the invoice transitions Confirmed and
    /// the webhook fires. 0 = zero-conf (microtransaction territory; the
    /// daemon logs a warning at startup).
    pub confirmations: u32,
    /// Default expiry for newly-created invoices. Callers can override
    /// per-invoice via the API.
    #[serde(default = "defaults::expiry_secs")]
    pub default_expiry_secs: u64,
    /// On partial payment, the invoice's expiry is reset to
    /// `now + partial_reset_secs`. Reset (not extend) keeps the customer
    /// experience predictable — they get a clean countdown to top up.
    #[serde(default = "defaults::partial_reset_secs")]
    pub partial_reset_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RefundsConfig {
    /// When `true`, the daemon will:
    ///  - require a customer-supplied refund address on every invoice
    ///  - refund partial payments on invoices that expire before Confirming
    ///  - refund the excess on overpaid invoices (network fee deducted from
    ///    the refund amount, so the merchant pays no fee for the courtesy).
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    /// Address the HTTP control plane binds to. Default is loopback because
    /// the daemon should sit behind a reverse proxy that handles TLS; binding
    /// public-facing without TLS would leak the bearer token.
    pub bind: String,
    /// Bearer token required in `Authorization: Bearer <token>`. Generated
    /// fresh per deployment; refuse to start if it's the placeholder value.
    pub auth_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhooksConfig {
    /// HTTP endpoint the daemon POSTs events to. Set to empty to disable
    /// webhooks entirely (the daemon will still record events in the DB,
    /// callers just have to poll the API).
    pub url: String,
    /// **Optional** HMAC-SHA256 secret. When set, every delivery body is
    /// signed and the digest is sent in the `X-Merchant-Signature` header
    /// (hex). When empty/missing, deliveries are unsigned — fine for
    /// internal-network deployments where the merchant and the daemon
    /// share a trusted network path. Set this when the webhook target
    /// is exposed to the internet so the receiver can prove a request
    /// genuinely came from this daemon.
    #[serde(default)]
    pub secret: String,
    /// Maximum delivery attempts before the event lands in the dead letter
    /// table. Exponential backoff between attempts.
    #[serde(default = "defaults::max_attempts")]
    pub max_attempts: u32,
}

mod defaults {
    pub fn poll_interval_secs() -> u64 {
        30
    }
    pub fn expiry_secs() -> u64 {
        1800 // 30 minutes
    }
    pub fn partial_reset_secs() -> u64 {
        1800 // 30 minutes
    }
    pub fn max_attempts() -> u32 {
        10
    }
}

impl Config {
    /// Load a config from a TOML file on disk. Validates the result —
    /// surfacing bad values here means the daemon never starts in a state
    /// that would silently behave wrong.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate semantic constraints that the TOML grammar can't express.
    /// Keep this exhaustive: every footgun belongs here, not at first-use
    /// time deep in the call stack.
    pub fn validate(&self) -> Result<()> {
        if self.network.name != "mainnet" {
            return Err(Error::Config(format!(
                "unsupported network: {} (only `mainnet` is supported today)",
                self.network.name
            )));
        }
        if self.api.auth_token.is_empty() || self.api.auth_token == "REPLACE_ME" {
            return Err(Error::Config(
                "api.auth_token must be set to a non-default value".into(),
            ));
        }
        // webhooks.secret is intentionally optional. We only reject the
        // literal placeholder so a half-configured deployment surfaces
        // visibly. Empty / missing is a legitimate "I don't need HMAC"
        // signal for internal-network setups.
        if self.webhooks.secret == "REPLACE_ME" {
            return Err(Error::Config(
                "webhooks.secret is set to the placeholder — either pick a real \
                 secret or remove the field entirely (unsigned deliveries are \
                 fine for internal networks)"
                    .into(),
            ));
        }
        if self.webhooks.max_attempts == 0 {
            return Err(Error::Config(
                "webhooks.max_attempts must be at least 1".into(),
            ));
        }
        if self.payments.default_expiry_secs < 60 {
            return Err(Error::Config(
                "payments.default_expiry_secs must be at least 60s — anything \
                 shorter is unreachable in practice given chain propagation"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_config_toml() -> &'static str {
        r#"
[network]
name = "mainnet"

[wallet]
data_dir = "./data"

[sync]
rpc_url = "https://rpc.pivxla.bz/mainnet"
explorer_url = "https://explorer.pivxla.bz"

[payments]
accept = "both"
confirmations = 3

[api]
bind = "127.0.0.1:7474"
auth_token = "real-token-here"

[webhooks]
url = "https://example.com/webhook"
secret = "real-secret-here"
"#
    }

    #[test]
    fn parses_minimal_valid_config() {
        let cfg: Config = toml::from_str(good_config_toml()).unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.payments.accept, AcceptPolicy::Both);
        assert_eq!(cfg.payments.confirmations, 3);
        // Defaults wired up correctly:
        assert_eq!(cfg.sync.poll_interval_secs, 30);
        assert_eq!(cfg.payments.default_expiry_secs, 1800);
        assert_eq!(cfg.webhooks.max_attempts, 10);
        assert!(!cfg.refunds.enabled);
    }

    #[test]
    fn accept_policy_serdes_lowercase() {
        let cfg: Config =
            toml::from_str(&good_config_toml().replace(r#"accept = "both""#, r#"accept = "shield""#))
                .unwrap();
        assert_eq!(cfg.payments.accept, AcceptPolicy::Shield);

        let cfg: Config = toml::from_str(
            &good_config_toml().replace(r#"accept = "both""#, r#"accept = "transparent""#),
        )
        .unwrap();
        assert_eq!(cfg.payments.accept, AcceptPolicy::Transparent);
    }

    #[test]
    fn rejects_unknown_network() {
        let toml = good_config_toml().replace(r#"name = "mainnet""#, r#"name = "regtest""#);
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_placeholder_auth_token() {
        let toml = good_config_toml()
            .replace(r#"auth_token = "real-token-here""#, r#"auth_token = "REPLACE_ME""#);
        let cfg: Config = toml::from_str(&toml).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("auth_token"));
    }

    #[test]
    fn rejects_placeholder_webhook_secret() {
        let toml = good_config_toml()
            .replace(r#"secret = "real-secret-here""#, r#"secret = "REPLACE_ME""#);
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn allows_empty_webhook_secret_for_internal_networks() {
        // Empty secret = "I don't want signatures, this is internal-only".
        // Should validate cleanly — the daemon will just send unsigned
        // deliveries.
        let toml = good_config_toml()
            .replace(r#"secret = "real-secret-here""#, r#"secret = """#);
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(cfg.webhooks.secret.is_empty());
    }

    #[test]
    fn allows_missing_webhook_secret_field() {
        // `secret` field absent entirely — same as empty, default String.
        let toml = good_config_toml()
            .replace("\nsecret = \"real-secret-here\"\n", "\n");
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(cfg.webhooks.secret.is_empty());
    }

    #[test]
    fn rejects_zero_max_attempts() {
        let mut cfg: Config = toml::from_str(good_config_toml()).unwrap();
        cfg.webhooks.max_attempts = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_microscopic_expiry() {
        let mut cfg: Config = toml::from_str(good_config_toml()).unwrap();
        cfg.payments.default_expiry_secs = 10;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn allows_zero_conf() {
        // zero-conf is an opt-in choice the operator can make for microtx;
        // we don't refuse it at config-load time. The daemon logs a warning
        // at startup instead.
        let mut cfg: Config = toml::from_str(good_config_toml()).unwrap();
        cfg.payments.confirmations = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn refunds_default_off() {
        // Refunds being opt-in is load-bearing: turning it on requires every
        // invoice to carry a refund address, which is a breaking API change
        // for callers. Default off keeps the simple path simple.
        let cfg: Config = toml::from_str(good_config_toml()).unwrap();
        assert!(!cfg.refunds.enabled);
    }

    #[test]
    fn refunds_can_be_enabled() {
        let toml = format!("{}\n[refunds]\nenabled = true\n", good_config_toml());
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.refunds.enabled);
    }
}
