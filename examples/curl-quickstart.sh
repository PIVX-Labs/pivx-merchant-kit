#!/usr/bin/env bash
# Quick API tour using curl. Assumes the daemon is running on
# 127.0.0.1:7474 with auth_token = "YOUR_TOKEN". Override with env vars.
set -euo pipefail

API_BASE="${API_BASE:-http://127.0.0.1:7474}"
API_TOKEN="${API_TOKEN:-YOUR_TOKEN}"

auth=(-H "authorization: Bearer ${API_TOKEN}")

echo "== Healthz (no auth) =="
curl -s "${API_BASE}/healthz"
echo

echo "== Create transparent invoice =="
INVOICE_JSON=$(curl -s -X POST "${API_BASE}/v1/invoices" \
  "${auth[@]}" \
  -H "content-type: application/json" \
  -d '{
    "channel": "transparent",
    "amount_due_sat": 50000000,
    "external_id": "demo-order-1",
    "metadata": {"customer": "alice@example.com"}
  }')
echo "$INVOICE_JSON"
INVOICE_ID=$(echo "$INVOICE_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")

echo
echo "== Get invoice by id =="
curl -s "${API_BASE}/v1/invoices/${INVOICE_ID}" "${auth[@]}"
echo

echo
echo "== List pending invoices =="
curl -s "${API_BASE}/v1/invoices?status=pending&limit=10" "${auth[@]}"
echo

echo
echo "== Idempotent re-create (same external_id) =="
curl -s -X POST "${API_BASE}/v1/invoices" \
  "${auth[@]}" \
  -H "content-type: application/json" \
  -d '{
    "channel": "transparent",
    "amount_due_sat": 50000000,
    "external_id": "demo-order-1"
  }' | python3 -c "import sys,json; d=json.load(sys.stdin); print('id matches:', d['id'])"

echo
echo "== Cancel invoice =="
curl -s -X POST "${API_BASE}/v1/invoices/${INVOICE_ID}/cancel" "${auth[@]}"
echo
