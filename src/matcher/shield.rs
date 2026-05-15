//! Shield matcher: turns newly-decrypted shield notes into `Payment` rows.
//!
//! Each note carries its `PaymentAddress` (via the embedded Sapling `Note`
//! struct). We re-encode the address and look up the matching invoice;
//! notes that don't match any open invoice (e.g. memos sent to our wallet
//! by friends, change outputs from our own spends) are silently skipped.
//!
//! Stage 4 doesn't reach into the note's memo. The matcher's job is just
//! "wire this on-chain payment to its invoice record." Higher layers can
//! surface memos through the API or webhooks later.

use crate::config::PaymentsConfig;
use crate::error::{Error, Result};
use crate::payment::Payment;
use crate::storage::{invoices, payments, Db};
use pivx_wallet_kit::keys::encode_payment_address;
use pivx_wallet_kit::wallet::SerializedNote;
use sapling::Note;

/// Apply a batch of newly-discovered shield notes. Returns the count of
/// new Payment rows actually inserted (notes that don't match an open
/// invoice are skipped without being counted).
pub async fn apply(
    db: &Db,
    config: &PaymentsConfig,
    notes: Vec<SerializedNote>,
    now: i64,
) -> Result<usize> {
    let mut inserted = 0usize;
    for serialized in notes {
        let Some((invoice_id, amount_sat, txid_proxy)) = match_note(&serialized, db).await? else {
            // Note doesn't correspond to any known invoice address.
            // That's expected for, e.g., shield change outputs from our
            // own refund spends.
            continue;
        };

        // Shield notes don't have a transparent-style (txid, vout). They
        // have a nullifier — globally unique once spent, but until then
        // we use the nullifier itself as the (txid, vout) key so the
        // `UNIQUE(txid, vout)` constraint dedupes repeat observations
        // across sync ticks. vout = 0 since the constraint expects a
        // tuple; the nullifier alone guarantees uniqueness.
        let payment = Payment::new(invoice_id, txid_proxy, 0, amount_sat, now);
        match payments::insert(db, &payment).await {
            Ok(()) => {
                inserted += 1;
                tracing::info!(
                    invoice_id = %invoice_id,
                    nullifier = %payment.txid,
                    amount_sat = amount_sat,
                    "new shield payment matched to invoice"
                );
                if let Some(invoice) = invoices::get(db, invoice_id).await? {
                    super::apply_for(db, &invoice, config, now).await?;
                }
            }
            Err(Error::Sqlx(sqlx::Error::Database(e)))
                if e.message().to_lowercase().contains("unique") =>
            {
                // Already seen.
            }
            Err(other) => return Err(other),
        }
    }
    Ok(inserted)
}

/// Resolve a single SerializedNote to its invoice. Returns
/// `Some((invoice_id, amount_sat, dedupe_key))` if the note's recipient
/// matches a known invoice, else `None`.
async fn match_note(
    serialized: &SerializedNote,
    db: &Db,
) -> Result<Option<(uuid::Uuid, u64, String)>> {
    let note: Note = serde_json::from_value(serialized.note.clone())
        .map_err(|e| Error::Invoice(format!("note deserialize: {}", e)))?;
    let encoded_addr = encode_payment_address(&note.recipient());
    let Some(invoice) = invoices::get_by_address(db, &encoded_addr).await? else {
        return Ok(None);
    };
    let value: u64 = note.value().inner();
    // Nullifier serves as the per-output dedupe key for shield. Stored
    // in payments.txid since the schema doesn't carry a separate column
    // (and the nullifier is the equivalent "this output, that input"
    // identifier for the shield model).
    let dedupe = serialized.nullifier.clone();
    Ok(Some((invoice.id, value, dedupe)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcceptPolicy;
    use crate::invoice::{Invoice, InvoiceStatus, PaymentChannel};
    use crate::storage::invoices;
    use uuid::Uuid;

    fn cfg() -> PaymentsConfig {
        PaymentsConfig {
            accept: AcceptPolicy::Both,
            confirmations: 3,
            default_expiry_secs: 1800,
            partial_reset_secs: 1800,
        }
    }

    #[tokio::test]
    async fn apply_empty_notes_is_noop() {
        // Trivial sanity case — the loop is empty, no DB writes, returns 0.
        // Real note round-tripping is integration-tested when we point the
        // daemon at a wallet that's received a shield payment (Stage 9).
        let db = Db::open_memory().await.unwrap();
        let count = apply(&db, &cfg(), Vec::new(), 1).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn no_invoice_for_address_returns_none() {
        // match_note against a DB with no matching invoice returns None
        // without erroring. We hand-craft a SerializedNote with a recipient
        // we don't have an invoice for.
        let db = Db::open_memory().await.unwrap();
        // Insert an unrelated invoice so the DB isn't empty.
        let inv = Invoice {
            id: Uuid::new_v4(),
            external_id: None,
            channel: PaymentChannel::Shield,
            amount_due_sat: 1000,
            address: "ps1qreal_invoice_address".into(),
            hd_index: 0,
            status: InvoiceStatus::Pending,
            created_at: 0,
            expires_at: 100,
            refund_address: None,
            metadata: serde_json::json!({}),
        };
        invoices::insert(&db, &inv).await.unwrap();

        // We don't have the test-fixture machinery for synthesising a
        // valid SerializedNote here (would require a real wallet +
        // chain). Confirm via direct DB query that an arbitrary address
        // not in the table returns None.
        let res = invoices::get_by_address(&db, "ps1qsomething_else").await.unwrap();
        assert!(res.is_none());
    }
}
