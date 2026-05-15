//! Refunds.
//!
//! Two scenarios produce a refund record (when `config.refunds.enabled`):
//!
//!   - **Partial-expired**: invoice received some payment but didn't reach
//!     the full amount before expiring. The customer gets back whatever
//!     they paid, minus the network fee for the refund tx itself.
//!   - **Overpayment**: invoice received more than `amount_due`. The
//!     excess goes back, again minus the refund-tx fee.
//!
//! This stage handles **detection + persistence + visibility**. The actual
//! tx construction / broadcast is gated on wallet-kit gaining a way to
//! sign transactions with an HD-indexed key (today the builder only signs
//! with the wallet's default transparent key). When that lands, the
//! refund worker layer slots in on top of the queue this stage owns —
//! same wire format, same DB rows, no API changes for the merchant.
//!
//! Until then: operators with `refunds.enabled = true` get a full record
//! of what's owed and to which address, exposed via `GET /v1/refunds`,
//! and can broadcast manually using `pivx-agent-kit` or any other PIVX
//! wallet. Setting `refunds.enabled = false` (the default) opts out
//! entirely — overpayments become donations, partial-expired invoices
//! keep their funds with the merchant.

pub mod queue;

use crate::config::PaymentsConfig;
use crate::error::Result;
use crate::invoice::Invoice;
use crate::storage::{invoices, payments, Db};

/// Fee deducted from refund amounts. Hardcoded for now — wallet-kit's
/// fee estimator (`fees::estimate_transparent_*`) lands here in a
/// follow-up once we have the full builder integration. 10000 sat
/// (= 0.0001 PIV) is comfortably above the protocol minimum for the
/// small txes refunds produce.
const REFUND_FEE_SAT: u64 = 10_000;

/// If the refund amount after fee deduction would be at or below this
/// threshold, skip the refund entirely — it's not worth the on-chain
/// fee to send dust. The operator can still see the never-issued
/// refund attempt in the logs.
const REFUND_DUST_THRESHOLD_SAT: u64 = REFUND_FEE_SAT;

/// Why a refund was created. Persisted on the row so support can tell
/// at a glance whether the customer's expecting a "you didn't pay
/// enough in time" refund vs an "oops, you sent more than the price"
/// refund.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundReason {
    PartialExpired,
    Overpayment,
}

impl RefundReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PartialExpired => "partial_expired",
            Self::Overpayment => "overpayment",
        }
    }
}

/// Called by the sweeper on the Pending/PartiallyPaid → Expired
/// transition. If the invoice was partially paid and has a refund
/// address, enqueue the refund.
pub async fn maybe_enqueue_partial_expired(
    db: &Db,
    invoice: &Invoice,
    config_refunds_enabled: bool,
) -> Result<()> {
    if !config_refunds_enabled {
        return Ok(());
    }
    let Some(refund_address) = invoice.refund_address.clone() else {
        // No refund address means the invoice was created in a pre-
        // refunds-enabled deployment, OR was explicitly created without
        // one. Either way, nothing to refund to — log so the operator
        // notices.
        tracing::warn!(
            invoice_id = %invoice.id,
            "invoice expired with partial payment but no refund_address — \
             funds remain with merchant"
        );
        return Ok(());
    };
    let paid_total = payments::total_amount_for_invoice(db, invoice.id).await?;
    if paid_total == 0 {
        return Ok(()); // Pending expired with zero payments — nothing to refund.
    }
    enqueue(
        db,
        invoice,
        &refund_address,
        paid_total,
        RefundReason::PartialExpired,
    )
    .await
}

/// Called by the confirms layer on the Confirming → Confirmed
/// transition. If the customer paid more than amount_due, enqueue a
/// refund for the excess.
pub async fn maybe_enqueue_overpayment(
    db: &Db,
    invoice: &Invoice,
    config: &PaymentsConfig,
    config_refunds_enabled: bool,
) -> Result<()> {
    if !config_refunds_enabled {
        return Ok(());
    }
    let confirmed_total = payments::confirmed_amount_for_invoice(
        db,
        invoice.id,
        config.confirmations,
    )
    .await?;
    if confirmed_total <= invoice.amount_due_sat {
        return Ok(());
    }
    let Some(refund_address) = invoice.refund_address.clone() else {
        // Overpay without a refund address is a donation; this branch
        // can't actually fire today because the API rejects invoices
        // without a refund_address when refunds.enabled, but we guard
        // here defensively in case a config flip happens after the
        // invoice was created.
        return Ok(());
    };
    let excess = confirmed_total - invoice.amount_due_sat;
    enqueue(db, invoice, &refund_address, excess, RefundReason::Overpayment).await
}

async fn enqueue(
    db: &Db,
    invoice: &Invoice,
    to_address: &str,
    gross_sat: u64,
    reason: RefundReason,
) -> Result<()> {
    // Skip if the refund would be dust after fee deduction.
    if gross_sat <= REFUND_DUST_THRESHOLD_SAT {
        tracing::info!(
            invoice_id = %invoice.id,
            gross_sat = gross_sat,
            fee_sat = REFUND_FEE_SAT,
            reason = %reason.as_str(),
            "refund skipped — net amount would be dust"
        );
        return Ok(());
    }
    let net_sat = gross_sat - REFUND_FEE_SAT;
    queue::insert(
        db,
        queue::NewRefund {
            invoice_id: invoice.id,
            reason,
            to_address: to_address.into(),
            amount_sat: net_sat,
            fee_sat: REFUND_FEE_SAT,
        },
    )
    .await?;
    tracing::info!(
        invoice_id = %invoice.id,
        to_address = %to_address,
        amount_sat = net_sat,
        fee_sat = REFUND_FEE_SAT,
        reason = %reason.as_str(),
        "refund enqueued (broadcast layer lands in Stage 7b — operator can \
         send manually via pivx-agent-kit until then)"
    );
    // Re-fetch and re-emit a webhook so the merchant gets notified
    // that this invoice has a refund obligation. Wrapping in a separate
    // event_type ("invoice.refund_created") would be nicer; for the
    // initial cut we lean on the invoice.expired / invoice.confirmed
    // event the matcher already emits and let the merchant correlate
    // via the refunds API.
    let _ = invoices::get(db, invoice.id).await; // touch invoice to ensure it still exists
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcceptPolicy;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use crate::payment::Payment;
    use uuid::Uuid;

    fn cfg() -> PaymentsConfig {
        PaymentsConfig {
            accept: AcceptPolicy::Both,
            confirmations: 3,
            default_expiry_secs: 1800,
            partial_reset_secs: 1800,
        }
    }

    async fn seed_invoice_with_payment(
        db: &Db,
        amount_due: u64,
        paid: u64,
        confirmations: u32,
        status: InvoiceStatus,
        refund_addr: Option<&str>,
    ) -> Invoice {
        let invoice = Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: amount_due,
            address: format!("Addr-{}", Uuid::new_v4()),
            status,
            created_at: 0,
            expires_at: 0,
            refund_address: refund_addr.map(String::from),
            metadata: serde_json::json!({}),
        };
        invoices::insert(db, &invoice).await.unwrap();
        if paid > 0 {
            let mut p = Payment::new(invoice.id, format!("tx-{}", invoice.id), 0, paid, 1);
            p.confirmations = confirmations;
            payments::insert(db, &p).await.unwrap();
        }
        invoice
    }

    #[tokio::test]
    async fn partial_expired_with_refund_address_enqueues() {
        let db = Db::open_memory().await.unwrap();
        // Paid 400_000 sat (0.004 PIV) — comfortably above the 10000-sat
        // fee + dust floor.
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            400_000,
            0,
            InvoiceStatus::Expired,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_partial_expired(&db, &inv, true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(refunds.len(), 1);
        assert_eq!(refunds[0].to_address, "DRefundAddr");
        assert_eq!(refunds[0].amount_sat, 400_000 - REFUND_FEE_SAT);
        assert_eq!(refunds[0].fee_sat, REFUND_FEE_SAT);
        assert_eq!(refunds[0].reason, "partial_expired");
    }

    #[tokio::test]
    async fn partial_expired_without_refund_address_does_not_enqueue() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            400_000,
            0,
            InvoiceStatus::Expired,
            None,
        )
        .await;
        maybe_enqueue_partial_expired(&db, &inv, true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }

    #[tokio::test]
    async fn refunds_disabled_in_config_does_not_enqueue() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            400_000,
            0,
            InvoiceStatus::Expired,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_partial_expired(&db, &inv, false).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }

    #[tokio::test]
    async fn dust_refund_is_skipped() {
        // Paid 5000, fee is 10000 → net would be negative. Skip.
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            100_000,
            5_000,
            0,
            InvoiceStatus::Expired,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_partial_expired(&db, &inv, true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }

    #[tokio::test]
    async fn zero_paid_does_not_enqueue() {
        // Pending invoice that expired without any payment. Nothing to
        // refund. Sweeper still transitions Expired, just no refund row.
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            0,
            0,
            InvoiceStatus::Expired,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_partial_expired(&db, &inv, true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }

    #[tokio::test]
    async fn overpayment_enqueues_excess_refund() {
        let db = Db::open_memory().await.unwrap();
        // 1000 due, 1500 confirmed → 500 excess, minus 10000 fee = ??? dust.
        // Bump up to keep above dust.
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            1_500_000,
            5, // confirmations >= threshold
            InvoiceStatus::Confirmed,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_overpayment(&db, &inv, &cfg(), true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert_eq!(refunds.len(), 1);
        // 500000 excess minus fee.
        assert_eq!(refunds[0].amount_sat, 500_000 - REFUND_FEE_SAT);
        assert_eq!(refunds[0].reason, "overpayment");
    }

    #[tokio::test]
    async fn exact_payment_does_not_overpay_refund() {
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            1_000_000,
            5,
            InvoiceStatus::Confirmed,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_overpayment(&db, &inv, &cfg(), true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }

    #[tokio::test]
    async fn overpay_below_threshold_not_counted() {
        // Payment confirms only 1 deep but threshold is 3. The
        // "confirmed_total" we compare against is 0, so amount_due
        // isn't exceeded yet from the matcher's perspective. No refund.
        let db = Db::open_memory().await.unwrap();
        let inv = seed_invoice_with_payment(
            &db,
            1_000_000,
            1_500_000,
            1, // only 1 conf, threshold is 3 in cfg()
            InvoiceStatus::Confirmed,
            Some("DRefundAddr"),
        )
        .await;
        maybe_enqueue_overpayment(&db, &inv, &cfg(), true).await.unwrap();
        let refunds = queue::list_for_invoice(&db, inv.id).await.unwrap();
        assert!(refunds.is_empty());
    }
}
