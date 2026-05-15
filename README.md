# PIVX Merchant Kit

[![CI](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml/badge.svg)](https://github.com/PIVX-Labs/pivx-merchant-kit/actions/workflows/ci.yml)

A self-hosted [PIVX](https://pivx.org) payment processor. Accept transparent
and shield payments without middlemen, custodians, or per-transaction fees.

> **Status:** under active development. Public API will firm up at `v0.1.0`.

## What it is

Merchant Kit is a small Rust daemon that turns a PIVX wallet into a payment
processor:

- Your backend asks Merchant Kit to create an invoice (amount, expiry).
- Merchant Kit derives a fresh address, watches the chain, and tracks the
  payment through its lifecycle.
- When the payment reaches the confirmation depth you've configured, Merchant
  Kit fires an HMAC-signed webhook at your backend so it can deliver the goods.

Built on [`pivx-wallet-kit`](https://github.com/PIVX-Labs/pivx-wallet-kit) for
all crypto operations — same audited core that powers
[`pivx-agent-kit`](https://github.com/PIVX-Labs/pivx-agent-kit).

## Design goals

- **Self-hosted.** You run it. No SaaS, no rent-seeking, no custody risk.
- **Fee-free.** Merchant Kit takes nothing. Customers pay the network fee, you
  pay nothing. That's the whole pitch.
- **Lightweight.** A single statically-linked binary plus a TOML config file.
- **Configurable.** Transparent, shield, or both. Zero-conf for microtx, or 10
  confirmations for high-value orders. Partial payments, refunds, expiry
  windows — every knob is documented and tuneable.
- **Webhook-first.** Drive Shopify, WooCommerce, your own backend, anything
  that can accept an HTTPS POST.

## Features (planned)

- [ ] Address-per-invoice for transparent **and** shield
- [ ] Configurable confirmation depth (incl. zero-conf with safety warnings)
- [ ] Partial payments with automatic timeout extension
- [ ] Optional automatic refunds (partial-on-expire, overpay excess)
- [ ] HMAC-signed webhook delivery with retry queue + dead letter
- [ ] REST control plane (bearer-auth) for backend integration
- [ ] SQLite persistence — single-file deployment

## Building

```bash
cargo build --release
```

The binary lands at `target/release/pivx-merchant-kit`.

## License

MIT © JSKitty
