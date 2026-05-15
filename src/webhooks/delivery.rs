//! Single webhook delivery attempt. Signs, POSTs, updates the row.

use crate::config::WebhooksConfig;
use crate::error::Result;
use crate::storage::Db;
use crate::webhooks::queue::{self, WebhookDelivery};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// Cap retry delay so an unreachable webhook eventually gives up. With
/// `max_attempts = 10` and exponential backoff capped at 1 hour, the
/// worst-case total wait is well under a day — long enough for the
/// merchant to notice and fix, short enough not to clog the table.
const MAX_RETRY_SECS: u64 = 3600;

/// Per-request timeout. Generous enough for slow merchants, short enough
/// that a hung server doesn't pin the worker loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn deliver_one(
    db: &Db,
    cfg: &WebhooksConfig,
    delivery: WebhookDelivery,
) -> Result<()> {
    let now = super::unix_now();

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("pivx-merchant-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| crate::error::Error::Config(format!("webhook client: {}", e)))?;

    let mut req = client
        .post(&cfg.url)
        .header("content-type", "application/json")
        .header("x-merchant-event-type", delivery.event_type.as_str())
        .header("x-merchant-delivery-id", delivery.id.to_string())
        .body(delivery.payload.clone());

    // HMAC signature is opt-in. If no secret is configured, the
    // X-Merchant-Signature header is omitted entirely — receivers
    // running on a trusted network don't need to verify anything,
    // the body is just plain JSON.
    if !cfg.secret.is_empty() {
        let signature = sign(&delivery.payload, &cfg.secret);
        req = req.header("x-merchant-signature", signature);
    }
    let res = req.send().await;

    let new_attempts = delivery.attempts + 1;
    match res {
        Ok(resp) if resp.status().is_success() => {
            queue::mark_delivered(db, delivery.id, resp.status().as_u16(), now).await?;
            tracing::info!(
                delivery_id = %delivery.id,
                event = %delivery.event_type,
                status = resp.status().as_u16(),
                "webhook delivered"
            );
            Ok(())
        }
        Ok(resp) => {
            let code = resp.status().as_u16();
            let err = format!("HTTP {}", code);
            if new_attempts >= cfg.max_attempts {
                tracing::warn!(
                    delivery_id = %delivery.id,
                    attempts = new_attempts,
                    "webhook dead-lettered after max attempts"
                );
                queue::mark_dead(db, delivery.id, new_attempts, &err).await
            } else {
                let next_at = now + backoff_secs(new_attempts);
                tracing::info!(
                    delivery_id = %delivery.id,
                    status = code,
                    attempts = new_attempts,
                    next_attempt_at = next_at,
                    "webhook retry scheduled"
                );
                queue::schedule_retry(db, delivery.id, new_attempts, next_at, &err, Some(code))
                    .await
            }
        }
        Err(e) => {
            // Connection refused / DNS / timeout — also retriable.
            let err = format!("transport: {}", e);
            if new_attempts >= cfg.max_attempts {
                tracing::warn!(
                    delivery_id = %delivery.id,
                    attempts = new_attempts,
                    "webhook dead-lettered after max attempts"
                );
                queue::mark_dead(db, delivery.id, new_attempts, &err).await
            } else {
                let next_at = now + backoff_secs(new_attempts);
                tracing::info!(
                    delivery_id = %delivery.id,
                    err = %e,
                    attempts = new_attempts,
                    next_attempt_at = next_at,
                    "webhook retry scheduled"
                );
                queue::schedule_retry(db, delivery.id, new_attempts, next_at, &err, None).await
            }
        }
    }
}

/// HMAC-SHA256 of the body using the configured secret. Returned hex-
/// encoded so the header is plain-ASCII without base64 padding nonsense.
pub fn sign(body: &str, secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any-length key");
    mac.update(body.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Exponential backoff in seconds: 2, 4, 8, 16, 32... capped at
/// MAX_RETRY_SECS. `attempts` is the new attempt count (so attempts=1
/// means the *first* retry — 2s wait).
pub fn backoff_secs(attempts: u32) -> i64 {
    let exp = attempts.min(20);
    let raw = 2u64.saturating_pow(exp);
    raw.min(MAX_RETRY_SECS) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_deterministic_and_hex_64() {
        let a = sign("hello", "secret");
        let b = sign("hello", "secret");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        // sha256 produces lowercase hex chars only.
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn sign_changes_with_body() {
        let a = sign("hello", "secret");
        let b = sign("world", "secret");
        assert_ne!(a, b);
    }

    #[test]
    fn sign_changes_with_secret() {
        let a = sign("hello", "secret-a");
        let b = sign("hello", "secret-b");
        assert_ne!(a, b);
    }

    #[test]
    fn sign_matches_known_vector() {
        // RFC 4231 Test Case 1: key = 0x0b * 20, data = "Hi There"
        // We use string inputs so we deliberately don't repro the byte-
        // level vector here — instead check against a Python reference
        // generated by `hmac.new(b"key", b"The quick brown fox jumps
        // over the lazy dog", hashlib.sha256).hexdigest()`.
        let got = sign("The quick brown fox jumps over the lazy dog", "key");
        assert_eq!(
            got,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        // Caps at MAX_RETRY_SECS (3600).
        assert_eq!(backoff_secs(12), 3600);
        assert_eq!(backoff_secs(100), 3600);
    }
}
