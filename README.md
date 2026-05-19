# PIVX Merchant Kit

[![CI](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml/badge.svg)](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A self-hosted [PIVX](https://pivx.org) payment processor.

Run a daemon, point your backend at its REST API, get webhooks when invoices are paid. No middlemen, no custody risk, no per-transaction cut. Customers pay the network fee, you pay nothing.

Built on [`pivx-wallet-kit`](https://github.com/PIVX-Labs/pivx-wallet-kit) — the same audited core that powers [`pivx-agent-kit`](https://github.com/PIVX-Labs/pivx-agent-kit).

## What it does

- **Address-per-invoice** for both transparent (`D...`) and shield (`ps1...`) channels.
- **Watches the chain** for incoming payments and tracks confirmations.
- **Webhook on each event** (`invoice.confirmed`, `invoice.expired`, `invoice.cancelled`), with optional HMAC signing.
- **Partial payments** automatically reset the invoice expiry so customers can top up.
- **Automatic refunds** for partial-paid-but-expired invoices and overpayments — builds, signs, and broadcasts a refund tx with zero operator action. Works on both channels.
- **One SQLite file** holds all state. One binary, one config, one database.

## Use cases

- E-commerce checkout (custom backend, WooCommerce/Shopify via thin adapter)
- Donation / tipping pages (one-shot invoice per visitor)
- SaaS / API billing (monthly invoice → webhook unlocks the next billing period)
- Pay-per-API-call gating (insufficient balance → invoice address in response)
- Crowdfunding with auto-refund-on-cancellation
- Pretty much any flow that says "give me a unique address, tell me when it's paid"

## Quickstart

```bash
# 1. Build
cargo build --release

# 2. Copy and edit the example config
cp config.toml.example config.toml
$EDITOR config.toml          # set api.auth_token to something secret

# 3. Generate a fresh wallet (24-word mnemonic shown once — back it up offline)
echo "your-unlock-passphrase" | ./target/release/pivx-merchant-kit init --config config.toml

# 4. Run the daemon
echo "your-unlock-passphrase" | ./target/release/pivx-merchant-kit run --config config.toml
```

That's it. The daemon is now syncing the chain and accepting invoices on the configured `api.bind` address.

> **First run downloads Sapling params (~50MB)** if you've configured `payments.accept = "shield"` or `"both"`. One-time, then cached in `data_dir/params/`. Skipped entirely for transparent-only deployments.

To restore an existing wallet from a mnemonic instead of generating a fresh one, use `import` and set the passphrase via env var:

```bash
echo "your 24-word mnemonic phrase..." | \
  MERCHANT_KIT_UNLOCK_PASSPHRASE="your-unlock-passphrase" \
  ./target/release/pivx-merchant-kit import --config config.toml
```

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

Show the customer the `address` and `amount_due_sat`. When they pay, the daemon detects the tx, waits for the configured number of confirmations, and either fires a webhook or surfaces the change via the API — your choice.

### Idempotency

Including an `external_id` makes invoice creation idempotent: re-POSTing with the same `external_id` returns the existing invoice (same `id`, same `address`) rather than creating a duplicate. Use your order ID and your backend can safely retry on any HTTP error.

## Receiving payments

Two patterns, pick whichever fits your stack:

### Pattern A: webhooks (push)

Lowest latency, no polling overhead. Daemon POSTs to the URL in your config when state changes.

```python
from flask import Flask, request

app = Flask(__name__)

@app.post("/webhook")
def hook():
    e = request.get_json()
    if e["event_type"] == "invoice.confirmed":
        inv = e["invoice"]
        ship_goods(
            order_id=inv["external_id"],         # your idempotency key
            email=inv["metadata"]["customer"],   # your stashed context
        )
    return "ok"
```

Event types:

| Event | When |
|-------|------|
| `invoice.confirmed` | Payment received and confirmed past the threshold |
| `invoice.expired` | Invoice timed out (partial-paid amount may have a refund record) |
| `invoice.cancelled` | Cancelled via the API |

The webhook body includes the **full invoice object** — same shape as `GET /v1/invoices/:id` — so you never need a follow-up call. Idempotent processing: the `X-Merchant-Delivery-Id` header (and `event_id` field) is a UUID the daemon retries with on failures; store the IDs you've handled and dedupe.

If you set `webhooks.secret` in config, the daemon adds an `X-Merchant-Signature` header (hex HMAC-SHA256 of the body) so the receiver can verify the call genuinely came from this daemon — useful when the receiver is on the public internet:

```python
import hmac, hashlib
expected = hmac.new(SECRET.encode(), request.data, hashlib.sha256).hexdigest()
if not hmac.compare_digest(request.headers["X-Merchant-Signature"], expected):
    abort(401)
```

Leave the secret empty for trusted-network deployments — the body is then plain JSON and there's no verification step.

### Pattern B: polling (pull)

No publicly-reachable endpoint required. Backend polls `GET /v1/invoices/:id` and inspects the `status` field.

```js
const inv = await fetch('http://daemon:7474/v1/invoices', {
  method: 'POST',
  headers: { authorization: 'Bearer ...', 'content-type': 'application/json' },
  body: JSON.stringify({
    channel: 'transparent',
    amount_due_sat: 5_000_000,
    external_id: 'ORD-1',
  }),
}).then(r => r.json());

// Then on the order status page, every few seconds:
const status = await fetch(`http://daemon:7474/v1/invoices/${inv.id}`, {
  headers: { authorization: 'Bearer ...' },
}).then(r => r.json());

if (status.status === 'confirmed') showSuccess(status);
```

Polling overhead is small — even at 1Hz for hundreds of in-flight checkouts, it's well under SQLite's read-only throughput.

### Pattern C: both

Use webhooks for the fast path and polling as a safety net for missed deliveries. The state is authoritative either way.

## API reference

All `/v1/*` routes require `Authorization: Bearer <api.auth_token>`. `/healthz` is unauthenticated for load-balancer probes.

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/healthz` | Liveness probe |
| `POST` | `/v1/invoices` | Create a new invoice |
| `GET` | `/v1/invoices` | List invoices (`?status=&limit=`) |
| `GET` | `/v1/invoices/:id` | Get a single invoice + its payments |
| `POST` | `/v1/invoices/:id/cancel` | Cancel (`pending` or `partially_paid` only) |
| `GET` | `/v1/refunds` | List refund records |
| `GET` | `/v1/refunds/:id` | Get a single refund |
| `POST` | `/v1/refunds/:id/broadcast` | Manually mark a refund broadcast (operator workflow) |

See [`examples/curl-quickstart.sh`](examples/curl-quickstart.sh) for a runnable tour of every endpoint.

## Configuration

See [`config.toml.example`](config.toml.example) — every option is documented inline. The knobs you'll actually touch:

| Knob | What |
|------|------|
| `payments.accept` | `"transparent"`, `"shield"`, or `"both"` |
| `payments.confirmations` | Depth before `confirmed` fires. Default 3. Set 0 for zero-conf (microtx only — startup logs a loud warning) |
| `payments.default_expiry_secs` | How long invoices live by default. Default 1800 |
| `payments.partial_reset_secs` | When a partial lands, reset expiry to `now + this`. Default 1800 |
| `refunds.enabled` | When on, partial-expired and overpaid invoices get auto-refund txes built + broadcast. Off by default — overpays become donations |
| `webhooks.url` | Where to POST events |
| `webhooks.secret` | HMAC-SHA256 signing key. Empty = unsigned (fine on internal networks) |
| `api.auth_token` | Bearer token your backend presents. Refuses to start if empty / placeholder |
| `sync.rpc_url` | PIVX Core RPC with compact-stream support |
| `sync.explorer_url` | Blockbook v2 explorer |

## Architecture

```
                 ┌──────────────────────────────────────────┐
                 │  pivx-merchant-kit daemon                │
                 │                                          │
   Merchant ───► │  REST API (axum, bearer auth)            │
   backend       │  └─ /v1/invoices, /v1/refunds            │
                 │                                          │
                 │  Sync loop (every poll_interval_secs)    │
                 │  ├─ Blockbook → transparent UTXO match   │
                 │  └─ Compact-stream → shield note match   │
                 │                                          │
                 │  Webhook worker (HMAC, retry, dead-ltr)  │
                 │  └─ POST to merchant URL                 │
                 │                                          │
                 │  Refund worker (when refunds.enabled)    │
                 │  ├─ Builds + signs + broadcasts          │
                 │  └─ Updates row with on-chain txid       │
                 │                                          │
                 │  Storage                                 │
                 │  ├─ SQLite (one file)                    │
                 │  └─ Wallet (encrypted JSON on disk)      │
                 └──────────────────────────────────────────┘
                                  │
                                  ▼
                            PIVX network
```

Wallet keys never leave the daemon. The encrypted `wallet.json` is decrypted at startup using the passphrase you provide via stdin or `MERCHANT_KIT_UNLOCK_PASSPHRASE`, kept in memory, and re-encrypted to disk after each sync advance.

## Common pitfalls

- **Forgetting to back up the mnemonic at `init`.** The 24 words shown on first run are the *only* way to recover the wallet. The daemon never shows them again.
- **Setting `confirmations = 0` for non-microtx use.** Zero-conf means the daemon fires the webhook the moment the tx hits the mempool — fine for tiny amounts, dangerous for anything where a rollback would actually hurt. The startup log warns loudly if you do this.
- **Enabling `refunds.enabled` mid-flight.** Once enabled, every new invoice *must* include `refund_address` — the API rejects requests that don't. Flip the flag during a maintenance window and update your invoice-creation code first.
- **Small shield refunds eaten by fees.** Sapling refund txes carry a ~2.4M sat (~0.024 PIV) fee. Partial payments smaller than that produce dust refunds that get skipped. Consider using transparent invoices for small-ticket flows.
- **Behind-NAT webhook receivers.** Either expose your receiver via a reverse proxy or use the polling pattern instead.
- **Multiple daemons on the same data directory.** SQLite will technically allow it but state will desync. Run one daemon per data directory.

## Status

**v0.1.0 — feature complete, mainnet-verified end-to-end.**

What's shipped:

- ✅ Config + invoice state machine (`pending → partially_paid → confirming → confirmed`, plus terminal `expired` / `cancelled`)
- ✅ SQLite persistence with migrations + atomic HD cursor
- ✅ Wallet bootstrap (`init` / `import` / `run`) with encrypted-at-rest storage
- ✅ Sync loop — Blockbook transparent UTXO discovery + PIVX Core compact-stream shield block sync
- ✅ Invoice matcher with confirmation tracking and expiry sweeper
- ✅ REST API with bearer auth, idempotency, full validation
- ✅ Webhook delivery with optional HMAC, exponential-backoff retry, dead-letter queue
- ✅ Automatic refund broadcasting for both transparent and shield (Sapling params eager-loaded at startup)
- ✅ Graceful SIGINT/SIGTERM shutdown with wallet persistence

136 tests passing. Live end-to-end tested on mainnet — both channels, both refund paths, all verified on-chain.

## Roadmap

Open ideas for v0.2.0 — file an issue if any matter to you:

- Docker image + `docker-compose.yml` for one-line deployment
- `systemd` unit file template
- `pivx-merchant-kit doctor` — sanity-check config, RPC reachability, DB permissions
- Native testnet support (currently mainnet-only)
- WooCommerce / Shopify thin-adapter plugin
- `payment.received` webhook event before `confirmed` (early-notification for cautious flows)

## See also

- [`pivx-wallet-kit`](https://github.com/PIVX-Labs/pivx-wallet-kit) — the shared crypto core (BIP39, HD derivation, transparent + shield tx builders)
- [`pivx-agent-kit`](https://github.com/PIVX-Labs/pivx-agent-kit) — AI-agent / CLI / MCP-server wallet built on the same core

## License

MIT © JSKitty
