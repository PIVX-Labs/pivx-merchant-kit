#!/usr/bin/env python3
"""Minimal webhook receiver for pivx-merchant-kit.

Run alongside the daemon — point `webhooks.url` in your daemon config
at this server (`http://127.0.0.1:8080/webhook`).

If you set `webhooks.secret` in the daemon config, set the same value
in WEBHOOK_SECRET below and the script will verify the X-Merchant-
Signature header. If both are empty, signatures are skipped and the
body is consumed as plain JSON.

Dependencies: standard library only.
"""
import hashlib
import hmac
import http.server
import json
import os

# Match this with `webhooks.secret` in your daemon config. Leave empty
# (default) to consume unsigned webhooks for internal-network setups.
WEBHOOK_SECRET = os.environ.get("WEBHOOK_SECRET", "")


def verify(body: bytes, signature: str) -> bool:
    if not WEBHOOK_SECRET:
        return True  # Unsigned mode — trust the body.
    if not signature:
        return False
    expected = hmac.new(WEBHOOK_SECRET.encode(), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(n) if n else b""
        sig = self.headers.get("x-merchant-signature", "")
        if not verify(body, sig):
            self.send_response(401)
            self.end_headers()
            return

        try:
            event = json.loads(body)
        except Exception:
            self.send_response(400)
            self.end_headers()
            return

        event_type = event.get("event_type")
        invoice = event.get("invoice", {})
        print(
            f"[{event_type}] invoice={invoice.get('id')} "
            f"external_id={invoice.get('external_id')} "
            f"status={invoice.get('status')} "
            f"paid={invoice.get('amount_paid_sat')}/{invoice.get('amount_due_sat')} sat"
        )

        # Dispatch:
        if event_type == "invoice.confirmed":
            handle_confirmed(invoice)
        elif event_type == "invoice.expired":
            handle_expired(invoice)
        elif event_type == "invoice.cancelled":
            handle_cancelled(invoice)

        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *a, **k):
        pass  # Quiet the default request logger.


def handle_confirmed(invoice):
    """Payment fully received. Ship goods, mark order paid, etc."""
    order = invoice.get("metadata", {}).get("order")
    print(f"  -> ship goods for order {order}")


def handle_expired(invoice):
    """Invoice timed out. If refunds are enabled and the customer paid
    partially, a refund record will exist — check /v1/refunds."""
    print("  -> mark order failed / abandoned")


def handle_cancelled(invoice):
    """Cancelled via the API (operator action). Symmetric to expired."""
    print("  -> mark order cancelled")


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8080"))
    print(f"webhook receiver listening on http://127.0.0.1:{port}/webhook")
    if WEBHOOK_SECRET:
        print("HMAC verification ENABLED")
    else:
        print("HMAC verification DISABLED (unsigned mode)")
    http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
