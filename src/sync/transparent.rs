//! Transparent address watcher.
//!
//! Each tick walks the set of non-terminal invoices (Pending / PartiallyPaid)
//! and asks Blockbook for UTXOs at each address. The raw UTXO records are
//! returned to the caller — Stage 4's matcher turns them into Payment rows
//! and drives the invoice state machine. This layer does no matching itself
//! to keep the sync loop's responsibility narrow: I/O + parsing.
//!
//! Many invoices on a busy merchant means many Blockbook requests per tick.
//! We sequentialize them deliberately rather than fanning out concurrently:
//! a polite client doesn't hammer a shared explorer, and the work fits well
//! within a single poll interval at the scales we expect (hundreds of open
//! invoices). If a deployment needs thousands of concurrent open invoices,
//! the right answer is a websocket subscription, not a fatter fan-out.

use crate::error::Result;
use crate::invoice::Invoice;
use crate::storage::{invoices, Db};
use crate::sync::http::ExplorerClient;
use pivx_wallet_kit::wallet::{parse_blockbook_utxos, SerializedUTXO};

/// A discovered UTXO that the matcher will need to turn into a Payment row.
/// Carries the invoice it belongs to so Stage 4 doesn't have to re-resolve
/// it from the address.
#[derive(Clone)]
pub struct DiscoveredUtxo {
    pub invoice: Invoice,
    pub utxo: SerializedUTXO,
}

// SerializedUTXO doesn't derive Debug in wallet-kit, so we hand-roll one
// that shows the fields the matcher would log without pulling in the rest.
impl std::fmt::Debug for DiscoveredUtxo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveredUtxo")
            .field("invoice_id", &self.invoice.id)
            .field("address", &self.invoice.address)
            .field("txid", &self.utxo.txid)
            .field("vout", &self.utxo.vout)
            .field("amount_sat", &self.utxo.amount)
            .field("height", &self.utxo.height)
            .finish()
    }
}

/// Run one transparent sync tick. Returns every UTXO observed at any
/// watched invoice address, including ones we've already seen — the
/// matcher (Stage 4) is responsible for idempotency via the
/// `UNIQUE(txid, vout)` constraint on the payments table.
///
/// Failures fetching a single address don't abort the whole tick: we log
/// and continue. That keeps a temporary explorer hiccup on one address
/// from blocking detection on all the others.
pub async fn tick(db: &Db, explorer: &ExplorerClient) -> Result<Vec<DiscoveredUtxo>> {
    // Watch invoices that can still receive payments. Confirmed / Expired /
    // Cancelled invoices don't need polling — their addresses are still
    // monitorable via Stage 7's overpay-refund flow, but that's a different
    // layer that triggers on its own schedule.
    let watchlist = watch_list(db).await?;
    if watchlist.is_empty() {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    for invoice in watchlist {
        match explorer.utxos_for_address(&invoice.address).await {
            Ok(raw) => {
                let parsed = parse_blockbook_utxos(&raw);
                for utxo in parsed {
                    found.push(DiscoveredUtxo {
                        invoice: invoice.clone(),
                        utxo,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(
                    address = %invoice.address,
                    err = %e,
                    "blockbook UTXO fetch failed; continuing with other addresses"
                );
            }
        }
    }
    Ok(found)
}

/// List of invoices the watcher should poll on this tick. Bundled as a
/// separate function so it's clear (and unit-testable) which statuses
/// we're watching — every other module that needs "still listening for
/// payments?" criteria should call this.
pub async fn watch_list(db: &Db) -> Result<Vec<Invoice>> {
    let mut out = Vec::new();
    // Two queries because the `InvoiceFilter` only accepts a single status.
    // We deliberately don't add an `IN` variant to the filter yet — until
    // there's a second caller that needs it, the duplication is cheaper
    // than the abstraction.
    for status in [
        crate::invoice::InvoiceStatus::Pending,
        crate::invoice::InvoiceStatus::PartiallyPaid,
    ] {
        let batch = invoices::list(
            db,
            invoices::InvoiceFilter {
                status: Some(status),
                limit: Some(1000),
            },
        )
        .await?;
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use uuid::Uuid;

    async fn seed(db: &Db, addr: &str, status: InvoiceStatus) {
        let inv = Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Transparent,
            amount_due_sat: 100_000_000,
            address: addr.into(),
            hd_index: 0,
            status,
            created_at: 1,
            expires_at: 9999,
            refund_address: None,
            metadata: serde_json::json!({}),
        };
        invoices::insert(db, &inv).await.unwrap();
    }

    #[tokio::test]
    async fn watch_list_includes_pending_and_partial_only() {
        let db = Db::open_memory().await.unwrap();
        seed(&db, "DPending", InvoiceStatus::Pending).await;
        seed(&db, "DPartial", InvoiceStatus::PartiallyPaid).await;
        seed(&db, "DConfirming", InvoiceStatus::Confirming).await;
        seed(&db, "DConfirmed", InvoiceStatus::Confirmed).await;
        seed(&db, "DExpired", InvoiceStatus::Expired).await;
        seed(&db, "DCancelled", InvoiceStatus::Cancelled).await;

        let watched = watch_list(&db).await.unwrap();
        let addrs: Vec<&str> = watched.iter().map(|i| i.address.as_str()).collect();
        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&"DPending"));
        assert!(addrs.contains(&"DPartial"));
    }

    #[tokio::test]
    async fn watch_list_empty_when_no_open_invoices() {
        let db = Db::open_memory().await.unwrap();
        seed(&db, "DConfirmed", InvoiceStatus::Confirmed).await;
        let watched = watch_list(&db).await.unwrap();
        assert!(watched.is_empty());
    }
}
