# Integration guide

This guide walks through wiring `pivx-merchant-kit` into three real-world shapes of backend. Pick the one closest to your stack — the API is the same regardless.

Before any of these you need a running daemon. See [README.md](README.md)'s Quickstart for that — `init`, `run`, done. Each example below assumes:

- Daemon listening on `http://127.0.0.1:7474`
- `api.auth_token = "REPLACE_ME_TOKEN"` in your config
- (Optional) `webhooks.secret = "REPLACE_ME_SECRET"` if you want signed deliveries

Replace those placeholder strings with real values from your config.

---

## Example 1: Minimal Python webhook receiver

The simplest possible integration. Customer goes to checkout, your backend calls the daemon to mint an invoice, your `/webhook` endpoint waits for the daemon to call back when payment lands.

### Backend: `app.py`

```python
import os
import requests
from flask import Flask, request, jsonify, abort

app = Flask(__name__)

DAEMON = "http://127.0.0.1:7474"
TOKEN = "REPLACE_ME_TOKEN"
HEADERS = {"authorization": f"Bearer {TOKEN}", "content-type": "application/json"}

# In-memory order store. Replace with your DB.
ORDERS = {}


@app.post("/checkout")
def checkout():
    """Customer clicks 'pay with PIVX' — we mint an invoice."""
    order_id = request.json["order_id"]
    amount_piv = request.json["amount_piv"]
    email = request.json["email"]

    resp = requests.post(
        f"{DAEMON}/v1/invoices",
        headers=HEADERS,
        json={
            "channel": "transparent",
            "amount_due_sat": int(amount_piv * 100_000_000),
            "external_id": order_id,
            "metadata": {"email": email, "amount_piv": amount_piv},
        },
    )
    resp.raise_for_status()
    invoice = resp.json()

    ORDERS[order_id] = {"status": "pending", "address": invoice["address"]}
    return jsonify({
        "address": invoice["address"],
        "amount_piv": amount_piv,
        "expires_at": invoice["expires_at"],
    })


@app.post("/webhook")
def webhook():
    """Daemon calls us when invoice state changes."""
    event = request.get_json()
    inv = event["invoice"]
    order_id = inv["external_id"]

    if event["event_type"] == "invoice.confirmed":
        ORDERS[order_id]["status"] = "paid"
        send_receipt(inv["metadata"]["email"], order_id, inv["payments"])
        ship_order(order_id)
    elif event["event_type"] == "invoice.expired":
        ORDERS[order_id]["status"] = "expired"
    elif event["event_type"] == "invoice.cancelled":
        ORDERS[order_id]["status"] = "cancelled"

    return "ok"


@app.get("/order/<order_id>")
def order_status(order_id):
    """Customer's order-status page polls this for UI updates."""
    return jsonify(ORDERS.get(order_id, {"status": "unknown"}))


def send_receipt(email, order_id, payments):
    txids = [p["txid"] for p in payments]
    print(f"[email] {email}: order {order_id} paid (txids: {txids})")


def ship_order(order_id):
    print(f"[shipping] order {order_id}")


if __name__ == "__main__":
    app.run(port=8080)
```

### Verifying webhook signatures (recommended for internet-exposed receivers)

If `webhooks.secret` is set in the daemon config, every delivery includes an `X-Merchant-Signature` header. Verify before trusting:

```python
import hmac, hashlib

WEBHOOK_SECRET = b"REPLACE_ME_SECRET"

@app.post("/webhook")
def webhook():
    sig = request.headers.get("X-Merchant-Signature", "")
    expected = hmac.new(WEBHOOK_SECRET, request.data, hashlib.sha256).hexdigest()
    if not hmac.compare_digest(sig, expected):
        abort(401)
    # ...rest of the handler
```

### Idempotent processing

The daemon retries failed webhooks with exponential backoff. Your receiver should be idempotent: store the `X-Merchant-Delivery-Id` (a UUID) and skip re-processing if you've seen it before.

```python
DELIVERED_IDS = set()

@app.post("/webhook")
def webhook():
    delivery_id = request.headers.get("X-Merchant-Delivery-Id")
    if delivery_id in DELIVERED_IDS:
        return "ok"  # idempotent — already handled
    DELIVERED_IDS.add(delivery_id)
    # ...rest of the handler
```

---

## Example 2: Node.js / Express with polling

If you can't expose a publicly-reachable URL (development environments, behind-corporate-NAT, etc.), poll the API instead.

### Backend: `server.js`

```js
import express from 'express';
import fetch from 'node-fetch';

const app = express();
app.use(express.json());

const DAEMON = 'http://127.0.0.1:7474';
const TOKEN = 'REPLACE_ME_TOKEN';
const headers = {
  authorization: `Bearer ${TOKEN}`,
  'content-type': 'application/json',
};

// In-memory order store. Replace with your DB.
const orders = new Map();

app.post('/checkout', async (req, res) => {
  const { order_id, amount_piv, email } = req.body;

  const r = await fetch(`${DAEMON}/v1/invoices`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      channel: 'transparent',
      amount_due_sat: Math.round(amount_piv * 100_000_000),
      external_id: order_id,
      metadata: { email, amount_piv },
    }),
  });
  const invoice = await r.json();

  orders.set(order_id, {
    invoice_id: invoice.id,
    status: 'pending',
    address: invoice.address,
  });

  res.json({
    address: invoice.address,
    amount_piv,
    expires_at: invoice.expires_at,
  });
});

// Client polls this every 3 seconds while waiting for payment.
app.get('/order/:order_id', async (req, res) => {
  const order = orders.get(req.params.order_id);
  if (!order) return res.status(404).json({ status: 'unknown' });

  // Refresh from daemon.
  const r = await fetch(`${DAEMON}/v1/invoices/${order.invoice_id}`, { headers });
  const inv = await r.json();

  // Detect state transitions ourselves since we're not using webhooks.
  if (inv.status === 'confirmed' && order.status !== 'paid') {
    order.status = 'paid';
    await shipOrder(req.params.order_id, inv.metadata.email);
  } else if (inv.status === 'expired') {
    order.status = 'expired';
  } else if (inv.status === 'cancelled') {
    order.status = 'cancelled';
  }

  res.json(order);
});

async function shipOrder(orderId, email) {
  console.log(`[shipping] ${orderId} → ${email}`);
}

app.listen(8080);
```

### Client polling pattern

```html
<script>
async function poll(orderId) {
  while (true) {
    const r = await fetch(`/order/${orderId}`);
    const order = await r.json();
    if (order.status === 'paid') {
      showSuccess(order);
      return;
    } else if (order.status === 'expired' || order.status === 'cancelled') {
      showFailure(order.status);
      return;
    }
    await new Promise(r => setTimeout(r, 3000));
  }
}
</script>
```

---

## Example 3: WooCommerce-shape adapter (conceptual)

If you're integrating with an existing e-commerce platform that has its own payment gateway interface, the merchant-kit daemon sits behind a thin adapter that translates between the platform's events and the daemon's API.

### Adapter responsibilities

1. **On checkout**: convert the platform's order into a `POST /v1/invoices` call.
2. **On webhook**: convert merchant-kit's `invoice.confirmed` event into the platform's "payment complete" call.
3. **On admin actions** (refunds, cancellations): translate to merchant-kit API calls.

### Sketch

```python
# wp-content/plugins/pivx-payments/gateway.py (or PHP equivalent)

class PivxPaymentGateway:
    def __init__(self, daemon_url, token, webhook_secret=None):
        self.daemon = daemon_url
        self.headers = {
            "authorization": f"Bearer {token}",
            "content-type": "application/json",
        }
        self.webhook_secret = webhook_secret

    def process_payment(self, order):
        """Called when a WooCommerce checkout selects PIVX."""
        resp = requests.post(
            f"{self.daemon}/v1/invoices",
            headers=self.headers,
            json={
                "channel": "transparent",
                "amount_due_sat": int(order.total_piv * 100_000_000),
                "external_id": f"wc-order-{order.id}",
                "refund_address": order.customer_refund_address,  # optional
                "metadata": {
                    "wc_order_id": order.id,
                    "customer_id": order.customer_id,
                    "items": [{"sku": i.sku, "qty": i.qty} for i in order.items],
                },
            },
        )
        invoice = resp.json()

        # Save invoice ID on the order so the webhook handler can find it.
        order.set_meta("pivx_invoice_id", invoice["id"])
        order.set_meta("pivx_address", invoice["address"])
        order.update_status("pending-payment")

        # Redirect to a "send X PIV to this address" page.
        return {
            "result": "success",
            "redirect": f"/checkout/pivx-pay/{invoice['id']}",
        }

    def handle_webhook(self, body, signature_header):
        """POST /wc-api/pivx → here."""
        if self.webhook_secret:
            expected = hmac.new(
                self.webhook_secret.encode(), body, hashlib.sha256
            ).hexdigest()
            if not hmac.compare_digest(expected, signature_header):
                abort(401)

        event = json.loads(body)
        inv = event["invoice"]
        wc_order_id = inv["metadata"]["wc_order_id"]
        order = WooOrder.get(wc_order_id)

        if event["event_type"] == "invoice.confirmed":
            order.payment_complete(transaction_id=inv["payments"][0]["txid"])
            order.update_status("processing")
        elif event["event_type"] == "invoice.expired":
            order.update_status("failed", note="PIVX invoice expired")
        elif event["event_type"] == "invoice.cancelled":
            order.update_status("cancelled")

    def refund(self, order, amount):
        """WooCommerce admin clicks 'Refund'. Not yet automated — the
        merchant-kit refund flow only handles partial-expired and
        overpayment automatically. For arbitrary refunds, build the
        tx in your wallet and record the txid here."""
        ...  # operator workflow
```

### What the adapter ships

- A settings page in the platform admin (daemon URL, auth token, webhook secret).
- A `pivx-pay/<invoice_id>` template that shows the address + a QR code + a polling JS snippet for live status.
- The webhook endpoint mounted on the platform's REST router.

The adapter is thin because all the hard work (chain watching, confirmations, refunds) lives in the daemon. The adapter is just translation.

---

## Operational concerns

### Choosing a confirmation threshold

| `confirmations` | When to use | Risk |
|-----------------|-------------|------|
| `0` (zero-conf) | Microtransactions where rollback wouldn't hurt | Mempool tx can be double-spent until mined |
| `1` | Low-value goods (~under $5) | Rare 1-block reorgs |
| `3` (default) | Most flows | Negligible reorg risk |
| `10+` | High-value goods, exchange-scale withdrawals | Slower UX |

The daemon logs a loud warning at startup if you set `confirmations = 0`.

### Choosing webhook signatures

| Situation | Set `webhooks.secret`? |
|-----------|------------------------|
| Webhook receiver on same host as daemon | No need |
| Receiver on the same private network | No need |
| Receiver on the public internet | Yes — anyone could forge unsigned calls |
| Behind a reverse proxy you trust | Up to you |

### Storing customer context

Two places to stash data you'll need on the webhook:

1. **`external_id`** — your order ID. Use it as the lookup key into your own DB.
2. **`metadata`** — arbitrary JSON. Use it to carry the data you'd otherwise have to look up.

The webhook payload echoes both back, so you can do everything off the webhook body alone if you want. Or use `external_id` to look up your own DB. Either pattern works.

### Refund flow gotchas

- **Refunds are off by default.** Set `refunds.enabled = true` and every invoice must include `refund_address`.
- **Small shield partials produce dust refunds.** Sapling fees are ~2.4M sat per tx. A 0.01 PIV partial-expired refund would be net negative — the daemon skips those (`refund skipped — net amount would be dust` in logs).
- **Refunds run on a 30-second worker tick.** If you need faster, drop `poll_interval_secs` in the config or kick the daemon — restart picks up pending refunds immediately.
- **Manual broadcasts.** If the auto-broadcast fails (CDN down, RPC unreachable, weird edge case), `GET /v1/refunds` shows pending rows and `POST /v1/refunds/:id/broadcast` lets you record the txid after broadcasting via another wallet.

### Production deployment checklist

- [ ] Generated a non-default `api.auth_token` (the daemon refuses to start otherwise)
- [ ] Backed up the wallet mnemonic from `init` offline
- [ ] Set `payments.confirmations` appropriately for your average ticket size
- [ ] If using webhooks across the public internet, set `webhooks.secret` and verify on the receiver
- [ ] `wallet.data_dir` on a persistent volume (the encrypted wallet file lives there)
- [ ] Reverse proxy (nginx, caddy, traefik) handles TLS — don't expose the daemon's `127.0.0.1:7474` directly
- [ ] systemd or supervisord restarts the daemon on crash
- [ ] Monitoring on `GET /healthz`
- [ ] Backup strategy for the SQLite file (state) and `wallet.json` (encrypted keys)

---

## Next steps

- Run [`examples/curl-quickstart.sh`](examples/curl-quickstart.sh) end-to-end against your daemon to verify everything works
- Read [`config.toml.example`](config.toml.example) — every option is documented inline
- File an issue if your stack needs an integration shape not covered above
