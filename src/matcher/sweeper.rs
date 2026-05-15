//! Expiry sweeper. Walks non-terminal invoices whose `expires_at <= now`
//! and transitions them to `Expired`.
//!
//! Note: `Confirming` invoices are deliberately not expirable — once the
//! customer has paid the full amount, they're entitled to the goods even
//! if confirmations take longer than the window. The
//! `next_status_on_tick` transition function already encodes that, but
//! the sweeper's SQL also filters for `Pending`/`PartiallyPaid` so
//! Confirming rows never get loaded in the first place.

use crate::config::Config;
use crate::error::Result;
use crate::invoice::{next_status_on_tick, InvoiceStatus};
use crate::storage::{invoices, Db};

/// Sweep all expirable invoices. Returns the count that transitioned.
/// `config` is needed for the refund-detection hook — partial-paid
/// expired invoices may need a refund row created.
pub async fn tick(db: &Db, config: &Config, now: i64) -> Result<usize> {
    let candidates = invoices::list_expirable(db, now).await?;
    let mut expired = 0usize;
    for invoice in candidates {
        let next = next_status_on_tick(invoice.status, now, invoice.expires_at);
        if next != invoice.status && next == InvoiceStatus::Expired {
            invoices::update_status(db, invoice.id, InvoiceStatus::Expired).await?;
            expired += 1;
            tracing::info!(
                invoice_id = %invoice.id,
                expired_at = invoice.expires_at,
                now = now,
                prev = %invoice.status.as_str(),
                "invoice expired"
            );
            // Refund check happens BEFORE the webhook so the webhook
            // payload includes the refund record if one was created.
            crate::refunds::maybe_enqueue_partial_expired(
                db,
                &invoice,
                config.refunds.enabled,
            )
            .await?;
            crate::webhooks::enqueue(
                db,
                invoice.id,
                crate::webhooks::EventType::InvoiceExpired,
            )
            .await?;
        }
    }
    Ok(expired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AcceptPolicy, ApiConfig, Config, NetworkConfig, PaymentsConfig, RefundsConfig,
        SyncConfig, WalletConfig, WebhooksConfig,
    };
    use crate::invoice::{Invoice, PaymentChannel};
    use uuid::Uuid;

    fn test_config() -> Config {
        Config {
            network: NetworkConfig { name: "mainnet".into() },
            wallet: WalletConfig { data_dir: "/tmp".into() },
            sync: SyncConfig {
                rpc_url: "https://x".into(),
                explorer_url: "https://x".into(),
                poll_interval_secs: 30,
            },
            payments: PaymentsConfig {
                accept: AcceptPolicy::Both,
                confirmations: 3,
                default_expiry_secs: 1800,
                partial_reset_secs: 1800,
            },
            refunds: RefundsConfig { enabled: false },
            api: ApiConfig {
                bind: "127.0.0.1:0".into(),
                auth_token: "t".into(),
            },
            webhooks: WebhooksConfig {
                url: "https://x".into(),
                secret: String::new(),
                max_attempts: 10,
            },
        }
    }

    fn invoice(addr: &str, status: InvoiceStatus, expires_at: i64) -> Invoice {
        Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1000,
            address: addr.into(),
            hd_index: 0,
            status,
            created_at: 0,
            expires_at,
            refund_address: None,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn expires_pending_past_deadline() {
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr1", InvoiceStatus::Pending, 100);
        invoices::insert(&db, &inv).await.unwrap();
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 1);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Expired);
    }

    #[tokio::test]
    async fn expires_partially_paid_past_deadline() {
        // Partial-payment-then-expire is a major case — Stage 7 will
        // refund the partial. We just confirm the status transition
        // here.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr2", InvoiceStatus::PartiallyPaid, 100);
        invoices::insert(&db, &inv).await.unwrap();
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 1);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Expired);
    }

    #[tokio::test]
    async fn does_not_expire_confirming() {
        // Customer paid in full — they get the goods even if confs are
        // slow.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr3", InvoiceStatus::Confirming, 100);
        invoices::insert(&db, &inv).await.unwrap();
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 0);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Confirming);
    }

    #[tokio::test]
    async fn does_not_expire_future_deadline() {
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr4", InvoiceStatus::Pending, 9999);
        invoices::insert(&db, &inv).await.unwrap();
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 0);
        let updated = invoices::get(&db, inv.id).await.unwrap().unwrap();
        assert_eq!(updated.status, InvoiceStatus::Pending);
    }

    #[tokio::test]
    async fn does_not_re_expire_already_expired() {
        // Idempotent — sweeping again doesn't error or re-transition.
        let db = Db::open_memory().await.unwrap();
        let inv = invoice("Addr5", InvoiceStatus::Expired, 100);
        invoices::insert(&db, &inv).await.unwrap();
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sweeps_multiple() {
        let db = Db::open_memory().await.unwrap();
        for i in 0..5 {
            let inv = invoice(&format!("Addr{}", i), InvoiceStatus::Pending, 100);
            invoices::insert(&db, &inv).await.unwrap();
        }
        let n = tick(&db, &test_config(), 500).await.unwrap();
        assert_eq!(n, 5);
    }
}
