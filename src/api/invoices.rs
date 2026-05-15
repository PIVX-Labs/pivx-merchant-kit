//! Invoice REST endpoints.

use crate::api::error::ApiError;
use crate::api::types::{
    CreateInvoiceRequest, InvoiceListResponse, InvoiceResponse, ListQuery,
};
use crate::config::AcceptPolicy;
use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
use crate::storage::{invoices, payments};
use crate::sync::SyncState;
use crate::wallet::derive;
use axum::extract::{Path, Query, State};
use axum::Json;
use std::sync::Arc;
use uuid::Uuid;

/// Maximum invoice expiry the API will accept. A typo turning seconds
/// into days can otherwise create an invoice that effectively never
/// expires; this caps the blast radius.
const MAX_EXPIRY_SECS: u64 = 60 * 60 * 24 * 7;

/// POST /v1/invoices
pub async fn create(
    State(state): State<Arc<SyncState>>,
    Json(req): Json<CreateInvoiceRequest>,
) -> Result<Json<InvoiceResponse>, ApiError> {
    // --- Validation -------------------------------------------------
    if req.amount_due_sat == 0 {
        return Err(ApiError::BadRequest("amount_due_sat must be > 0".into()));
    }
    match (state.config.payments.accept, req.channel) {
        (AcceptPolicy::Transparent, PaymentChannel::Shield) => {
            return Err(ApiError::BadRequest(
                "this daemon is configured to accept transparent payments only".into(),
            ));
        }
        (AcceptPolicy::Shield, PaymentChannel::Transparent) => {
            return Err(ApiError::BadRequest(
                "this daemon is configured to accept shield payments only".into(),
            ));
        }
        _ => {}
    }
    if state.config.refunds.enabled && req.refund_address.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::BadRequest(
            "refunds are enabled — every invoice must include a refund_address".into(),
        ));
    }
    let expiry_secs = req
        .expires_in_secs
        .unwrap_or(state.config.payments.default_expiry_secs);
    if expiry_secs == 0 || expiry_secs > MAX_EXPIRY_SECS {
        return Err(ApiError::BadRequest(format!(
            "expires_in_secs must be between 1 and {}",
            MAX_EXPIRY_SECS
        )));
    }

    // --- Idempotency: external_id collision returns existing -------
    if let Some(ext) = req.external_id.as_deref() {
        if let Some(existing) = invoices::get_by_external_id(&state.db, ext).await? {
            let payments = payments::list_for_invoice(&state.db, existing.id).await?;
            return Ok(Json(InvoiceResponse::from_invoice(existing, payments)));
        }
    }

    // --- Derive next address ---------------------------------------
    let wallet = state.wallet.lock().await;
    let derived = derive::next_address(&state.db, &wallet, req.channel).await?;
    drop(wallet);

    // --- Persist ----------------------------------------------------
    let now = unix_now();
    let invoice = Invoice {
        id: Uuid::new_v4(),
        external_id: req.external_id.clone(),
        channel: req.channel,
        amount_due_sat: req.amount_due_sat,
        address: derived.address,
        status: InvoiceStatus::Pending,
        created_at: now,
        expires_at: now + expiry_secs as i64,
        refund_address: req.refund_address,
        metadata: req.metadata,
    };
    invoices::insert(&state.db, &invoice).await?;

    tracing::info!(
        invoice_id = %invoice.id,
        external_id = ?invoice.external_id,
        channel = %invoice.channel.as_str(),
        amount_due_sat = invoice.amount_due_sat,
        address = %invoice.address,
        "invoice created"
    );

    Ok(Json(InvoiceResponse::from_invoice(invoice, Vec::new())))
}

/// GET /v1/invoices/:id
pub async fn get(
    State(state): State<Arc<SyncState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, ApiError> {
    let invoice = invoices::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    let payments = payments::list_for_invoice(&state.db, id).await?;
    Ok(Json(InvoiceResponse::from_invoice(invoice, payments)))
}

/// GET /v1/invoices?status=pending&limit=50
pub async fn list(
    State(state): State<Arc<SyncState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<InvoiceListResponse>, ApiError> {
    let invoices_list = invoices::list(
        &state.db,
        invoices::InvoiceFilter {
            status: q.status,
            limit: q.limit,
        },
    )
    .await?;
    let mut out = Vec::with_capacity(invoices_list.len());
    for invoice in invoices_list {
        let ps = payments::list_for_invoice(&state.db, invoice.id).await?;
        out.push(InvoiceResponse::from_invoice(invoice, ps));
    }
    Ok(Json(InvoiceListResponse { invoices: out }))
}

/// POST /v1/invoices/:id/cancel
pub async fn cancel(
    State(state): State<Arc<SyncState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, ApiError> {
    let invoice = invoices::get(&state.db, id).await?.ok_or(ApiError::NotFound)?;
    if !invoice.status.is_cancellable() {
        return Err(ApiError::Conflict(format!(
            "invoice is {}, only pending or partially_paid can be cancelled",
            invoice.status.as_str()
        )));
    }
    invoices::update_status(&state.db, id, InvoiceStatus::Cancelled).await?;
    let payments = payments::list_for_invoice(&state.db, id).await?;
    let mut invoice = invoice;
    invoice.status = InvoiceStatus::Cancelled;
    tracing::info!(invoice_id = %id, "invoice cancelled by API");
    Ok(Json(InvoiceResponse::from_invoice(invoice, payments)))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AcceptPolicy, ApiConfig, Config, NetworkConfig, PaymentsConfig, RefundsConfig,
        SyncConfig, WalletConfig, WebhooksConfig,
    };
    use crate::storage::Db;
    use crate::sync::http::{ExplorerClient, RpcClient};
    use crate::wallet::Wallet;
    use std::path::PathBuf;
    use tokio::sync::Mutex;
    use zeroize::Zeroizing;

    fn test_config(refunds_enabled: bool, accept: AcceptPolicy) -> Config {
        Config {
            network: NetworkConfig {
                name: "mainnet".into(),
            },
            wallet: WalletConfig {
                data_dir: "/tmp".into(),
            },
            sync: SyncConfig {
                rpc_url: "https://example.com".into(),
                explorer_url: "https://example.com".into(),
                poll_interval_secs: 30,
            },
            payments: PaymentsConfig {
                accept,
                confirmations: 3,
                default_expiry_secs: 1800,
                partial_reset_secs: 1800,
            },
            refunds: RefundsConfig {
                enabled: refunds_enabled,
            },
            api: ApiConfig {
                bind: "127.0.0.1:0".into(),
                auth_token: "test-token".into(),
            },
            webhooks: WebhooksConfig {
                url: "https://example.com".into(),
                secret: "test-secret".into(),
                max_attempts: 10,
            },
        }
    }

    async fn test_state(refunds_enabled: bool, accept: AcceptPolicy) -> Arc<SyncState> {
        let db = Db::open_memory().await.unwrap();
        let (wallet, _mnemonic) = Wallet::create_new(0).unwrap();
        let config = test_config(refunds_enabled, accept);
        // Build SyncState manually since SyncState::new() spins up clients
        // pointing at the real URLs from config; we want in-process tests
        // that don't touch the network.
        Arc::new(SyncState {
            db,
            wallet: Arc::new(Mutex::new(wallet)),
            wallet_path: PathBuf::from("/tmp/test-wallet.json"),
            unlock_key: Zeroizing::new([0u8; 32]),
            explorer: ExplorerClient::new(&config.sync.explorer_url).unwrap(),
            rpc: RpcClient::new(&config.sync.rpc_url).unwrap(),
            config,
        })
    }

    #[tokio::test]
    async fn create_invoice_happy_path() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let req = CreateInvoiceRequest {
            external_id: Some("ord-1".into()),
            channel: PaymentChannel::Transparent,
            amount_due_sat: 100_000_000,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::json!({"order": "ord-1"}),
        };
        let Json(resp) = create(State(state.clone()), Json(req)).await.unwrap();
        assert_eq!(resp.amount_due_sat, 100_000_000);
        assert_eq!(resp.amount_paid_sat, 0);
        assert_eq!(resp.status, InvoiceStatus::Pending);
        assert!(resp.address.starts_with('D'));
        assert!(resp.payments.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_zero_amount() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let req = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 0,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        let err = create(State(state), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_rejects_disallowed_channel() {
        // Shield-only daemon, transparent request -> 400.
        let state = test_state(false, AcceptPolicy::Shield).await;
        let req = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        let err = create(State(state), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_requires_refund_address_when_refunds_enabled() {
        let state = test_state(true, AcceptPolicy::Both).await;
        let req = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        let err = create(State(state), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("refund_address")));
    }

    #[tokio::test]
    async fn create_with_refund_address_when_refunds_enabled_succeeds() {
        let state = test_state(true, AcceptPolicy::Both).await;
        let req = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1000,
            expires_in_secs: None,
            refund_address: Some("DRefundXX".into()),
            metadata: serde_json::Value::Null,
        };
        let resp = create(State(state), Json(req)).await.unwrap();
        assert_eq!(resp.refund_address.as_deref(), Some("DRefundXX"));
    }

    #[tokio::test]
    async fn create_rejects_zero_or_excessive_expiry() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let bad_zero = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1,
            expires_in_secs: Some(0),
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        assert!(matches!(
            create(State(state.clone()), Json(bad_zero)).await.unwrap_err(),
            ApiError::BadRequest(_)
        ));

        let bad_huge = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1,
            expires_in_secs: Some(MAX_EXPIRY_SECS + 1),
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        assert!(matches!(
            create(State(state), Json(bad_huge)).await.unwrap_err(),
            ApiError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn idempotent_create_returns_existing() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let make = || CreateInvoiceRequest {
            external_id: Some("ord-idem".into()),
            channel: PaymentChannel::Transparent,
            amount_due_sat: 5_000,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        let first = create(State(state.clone()), Json(make())).await.unwrap();
        let second = create(State(state), Json(make())).await.unwrap();
        // Second call returns the same invoice — no new id, no new address.
        assert_eq!(first.id, second.id);
        assert_eq!(first.address, second.address);
    }

    #[tokio::test]
    async fn get_returns_invoice_with_payments() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let req = CreateInvoiceRequest {
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 1000,
            expires_in_secs: None,
            refund_address: None,
            metadata: serde_json::Value::Null,
        };
        let created = create(State(state.clone()), Json(req)).await.unwrap();
        let fetched = super::get(State(state), Path(created.id)).await.unwrap();
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn get_missing_invoice_returns_not_found() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let err = super::get(State(state), Path(Uuid::new_v4())).await.unwrap_err();
        assert!(matches!(err, ApiError::NotFound));
    }

    #[tokio::test]
    async fn cancel_pending_invoice_works() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let created = create(
            State(state.clone()),
            Json(CreateInvoiceRequest {
                external_id: None,
                channel: PaymentChannel::Transparent,
                amount_due_sat: 1000,
                expires_in_secs: None,
                refund_address: None,
                metadata: serde_json::Value::Null,
            }),
        )
        .await
        .unwrap();
        let cancelled = cancel(State(state), Path(created.id)).await.unwrap();
        assert_eq!(cancelled.status, InvoiceStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_confirming_invoice_conflicts() {
        let state = test_state(false, AcceptPolicy::Both).await;
        let created = create(
            State(state.clone()),
            Json(CreateInvoiceRequest {
                external_id: None,
                channel: PaymentChannel::Transparent,
                amount_due_sat: 1000,
                expires_in_secs: None,
                refund_address: None,
                metadata: serde_json::Value::Null,
            }),
        )
        .await
        .unwrap();
        // Force into Confirming.
        invoices::update_status(&state.db, created.id, InvoiceStatus::Confirming)
            .await
            .unwrap();
        let err = cancel(State(state), Path(created.id)).await.unwrap_err();
        assert!(matches!(err, ApiError::Conflict(_)));
    }
}
