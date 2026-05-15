# PIVX Merchant Kit

[![CI](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml/badge.svg)](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml)

A self-hosted [PIVX](https://pivx.org) payment processor.

Run a daemon, point your backend at its REST API, get webhooks when invoices are paid. No middlemen, no custody risk, no per-transaction cut. Customers pay the network fee, you pay nothing.

Built on [`pivx-wallet-kit`](https://github.com/PIVX-Labs/pivx-wallet-kit) — the same audited core that powers [`pivx-agent-kit`](https://github.com/PIVX-Labs/pivx-agent-kit).

## What it does

- **Address-per-invoice** for both transparent and shield channels.
- **Watches the chain** for incoming payments and tracks confirmations.
- **Webhook on each event** (invoice confirmed / expired / cancelled), signed with HMAC if you want.
- **Partial payments** automatically extend the invoice expiry so the customer can top up.
- **Refunds** for partial-expired or overpaid invoices, fully optional.
- **One SQLite file** holds all state. One binary, one config, one database.

## Quickstart

```bash
# 1. Build
cargo build --release

# 2. Write a config (see config.toml.example for a fully commented version)
$EDITOR config.toml

# 3. Generate a fresh wallet (24-word mnemonic shown ONCE — back it up)
echo "your-unlock-passphrase" | ./target/release/pivx-merchant-kit init --config config.toml

# 4. Run the daemon
echo "your-unlock-passphrase" | ./target/release/pivx-merchant-kit run --config config.toml
```

That's it. Your daemon is now syncing the chain and accepting invoices on the configured `api.bind` address.

## Creating an invoice

```bash
curl -X POST http://127.0.0.1:7474/v1/invoices \
  -H "authorization: Bearer YOUR_API_TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "channel": "transparent",
    "amount_due_sat": 100000000,
    "external_id": "order-1234",
    "metadata": {"customer": "alice@example.com"}
  }'
```

Response:

```json
{
  "id": "e5b0df69-c071-4e03-bf9f-6cc12cb57830",
  "external_id": "order-1234",
  "channel": "transparent",
  "amount_due_sat": 100000000,
  "amount_paid_sat": 0,
  "address": "DRzLyMstDKKpGVAnxpsG5U1LRZRRfcJSCF",
  "status": "pending",
  "created_at": 1778869053,
  "expires_at": 1778870853,
  "refund_address": null,
  "metadata": {"customer": "alice@example.com"},
  "payments": []
}
```

Show the customer the `address` and `amount_due_sat`. When they pay, the daemon will detect the transaction, wait for the configured number of confirmations, then POST a webhook to your backend.

## Receiving webhooks

The simplest receiver (Python, no crypto needed when `webhooks.secret` is empty):

```python
from flask import Flask, request

app = Flask(__name__)

@app.post("/webhook")
def hook():
    data = request.get_json()
    if data["event_type"] == "invoice.confirmed":
        order_id = data["invoice"]["external_id"]
        ship_goods(order_id)
    return "ok"
```

Three event types are emitted today:
- `invoice.confirmed` — payment fully received and confirmed
- `invoice.expired` — invoice timed out; partial-paid amount may have a refund record
- `invoice.cancelled` — invoice cancelled via the API

The payload always includes the full invoice object (same shape as `GET /v1/invoices/:id`), so you have everything you need without a follow-up call.

### Verifying signatures (optional)

If `webhooks.secret` is set in your config, the daemon adds an `X-Merchant-Signature` header containing the hex HMAC-SHA256 of the body. Verify it like this:

```python
import hmac, hashlib
expected = hmac.new(SECRET.encode(), request.data, hashlib.sha256).hexdigest()
if not hmac.compare_digest(request.headers["X-Merchant-Signature"], expected):
    abort(401)
```

Leave the secret empty for internal-network deployments — the body is just plain JSON, no verification step needed.

## API reference

All routes under `/v1/*` require `Authorization: Bearer <api.auth_token>`. `/healthz` is open.

| Method | Path                              | Purpose                          |
|--------|-----------------------------------|----------------------------------|
| GET    | `/healthz`                        | Liveness probe                   |
| POST   | `/v1/invoices`                    | Create a new invoice             |
| GET    | `/v1/invoices`                    | List invoices (`?status=&limit=`)|
| GET    | `/v1/invoices/:id`                | Get a single invoice + payments  |
| POST   | `/v1/invoices/:id/cancel`         | Cancel (Pending / PartiallyPaid) |
| GET    | `/v1/refunds`                     | List refund records              |
| GET    | `/v1/refunds/:id`                 | Get a single refund              |
| POST   | `/v1/refunds/:id/broadcast`       | Mark a refund as broadcast       |

See [examples/](examples/) for a runnable receiver + curl quickstart.

## Configuration

See [`config.toml.example`](config.toml.example) — every option is documented inline.

Key knobs:

- `payments.confirmations` — depth before fires webhook. Default 3. Set 0 for zero-conf (microtransactions only — a startup warning fires).
- `payments.accept` — `"transparent"`, `"shield"`, or `"both"`.
- `refunds.enabled` — when on, partial-expired and overpaid invoices get auto-refund records. Every invoice must then include `refund_address`.
- `webhooks.secret` — leave empty for unsigned deliveries (fine on a trusted network), set to enable HMAC signing.

## Architecture

```
                 ┌────────────────────────────────────────┐
                 │  pivx-merchant-kit daemon              │
                 │                                        │
   Merchant ───► │  REST API (axum, bearer auth)          │
   backend       │  └─ /v1/invoices, /v1/refunds          │
                 │                                        │
                 │  Sync loop (per poll_interval_secs)    │
                 │  ├─ Blockbook explorer → transparent   │
                 │  │  UTXO discovery + matcher           │
                 │  └─ PIVX Core compact-stream → shield  │
                 │     block sync + note decryption       │
                 │                                        │
                 │  Webhook worker                        │
                 │  └─ Retry queue with HMAC signing      │
                 │                                        │
                 │  Storage                               │
                 │  └─ SQLite (one file)                  │
                 │  └─ Wallet (encrypted JSON on disk)    │
                 └────────────────────────────────────────┘
                              │
                              ▼
                       PIVX network
```

Wallet keys never leave the daemon. The encrypted `wallet.json` is decrypted at startup using the passphrase you provide via stdin or `MERCHANT_KIT_UNLOCK_PASSPHRASE`, kept in memory, and re-encrypted to disk after each successful sync advance.

## Status

**v0.1.0 (in development)** — first 8 of 9 stages complete:

- ✅ Config + invoice state machine
- ✅ SQLite persistence with migrations
- ✅ Wallet bootstrap (init/import/run) with encrypted storage
- ✅ Sync loop (transparent + shield, live-tested on mainnet)
- ✅ Invoice matcher (state transitions, confirmation tracking, expiry)
- ✅ REST API
- ✅ Webhook delivery (HMAC optional)
- ✅ Refund detection
- ✅ Graceful shutdown

**Stage 7b (deferred)**: automatic refund broadcasting. Requires wallet-kit gaining custom-key signing (the current builder signs only with the wallet's default key, but refunds need to spend from the invoice's HD-indexed address). Until then, the operator workflow is: see pending refund via `GET /v1/refunds`, build + broadcast the tx in their wallet, POST the txid back to `/v1/refunds/:id/broadcast`.

135 tests passing. Live end-to-end smoke tested against mainnet.

## License

MIT © JSKitty
