-- Initial schema for pivx-merchant-kit.
--
-- All tables defined up front so later stages don't need migrations just to
-- add empty tables. Stages 6 (webhooks) and 7 (refunds) fill in their tables
-- without further schema changes.
--
-- IDs are stored as TEXT (UUID as ASCII) for human-readable logs and trivial
-- copy/paste in support flows. SQLite has no fixed-width UUID type and the
-- size difference vs BLOB is negligible at the scales this daemon targets.
--
-- Amounts are stored as INTEGER satoshis. SQLite's INTEGER is 64-bit signed;
-- u64 satoshi values up to 2^63 fit (~92 billion PIV — far above the supply
-- cap). The repo layer casts via u64::try_from on read.

PRAGMA foreign_keys = ON;

CREATE TABLE invoices (
    id              TEXT    PRIMARY KEY,
    external_id     TEXT    UNIQUE,        -- nullable; merchant idempotency key
    channel         TEXT    NOT NULL,      -- 'transparent' | 'shield'
    amount_due_sat  INTEGER NOT NULL,
    address         TEXT    NOT NULL UNIQUE,
    -- HD index used to derive this invoice's address. Stored so we can
    -- re-derive the spending key when refunding without scanning the
    -- chain or maintaining a separate index of address -> key.
    hd_index        INTEGER NOT NULL,
    status          TEXT    NOT NULL,      -- InvoiceStatus serialized
    refund_address  TEXT,                  -- required when refunds.enabled
    metadata        TEXT    NOT NULL DEFAULT '{}',  -- arbitrary JSON
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL       -- mutable; reset on partial payment
);

CREATE INDEX idx_invoices_status     ON invoices(status);
CREATE INDEX idx_invoices_expires_at ON invoices(expires_at);

CREATE TABLE payments (
    id             TEXT    PRIMARY KEY,
    invoice_id     TEXT    NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    txid           TEXT    NOT NULL,
    vout           INTEGER NOT NULL,
    amount_sat     INTEGER NOT NULL,
    confirmations  INTEGER NOT NULL DEFAULT 0,
    seen_at        INTEGER NOT NULL,
    confirmed_at   INTEGER,
    -- A single tx output can only fund one invoice. Without this,
    -- double-counting an output across invoices would skew totals.
    UNIQUE(txid, vout)
);

CREATE INDEX idx_payments_invoice ON payments(invoice_id);

-- Single-row state table for the next HD index to derive. Separate columns
-- for transparent and shield branches since they derive from different
-- subtrees of the wallet HD chain.
CREATE TABLE hd_cursor (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    transparent_next  INTEGER NOT NULL DEFAULT 0,
    shield_next       INTEGER NOT NULL DEFAULT 0
);

INSERT INTO hd_cursor (id) VALUES (1);

-- Outbound webhook delivery queue. Stage 6 reads/writes these rows.
CREATE TABLE webhook_deliveries (
    id                TEXT    PRIMARY KEY,
    invoice_id        TEXT    NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    event_type        TEXT    NOT NULL,    -- 'invoice.confirmed' etc
    payload           TEXT    NOT NULL,    -- JSON body to POST
    attempts          INTEGER NOT NULL DEFAULT 0,
    next_attempt_at   INTEGER NOT NULL,
    status            TEXT    NOT NULL DEFAULT 'pending',  -- pending | delivered | dead
    last_error        TEXT,
    last_status_code  INTEGER,
    created_at        INTEGER NOT NULL,
    delivered_at      INTEGER
);

CREATE INDEX idx_webhook_status_next ON webhook_deliveries(status, next_attempt_at);

-- Refund records. Stage 7 reads/writes these rows.
CREATE TABLE refunds (
    id             TEXT    PRIMARY KEY,
    invoice_id     TEXT    NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    reason         TEXT    NOT NULL,        -- 'partial_expired' | 'overpayment'
    to_address     TEXT    NOT NULL,
    amount_sat     INTEGER NOT NULL,        -- amount being refunded (post-fee)
    fee_sat        INTEGER NOT NULL,        -- network fee deducted from refund
    txid           TEXT,                    -- null until broadcast
    status         TEXT    NOT NULL DEFAULT 'pending',  -- pending | broadcast | confirmed | failed
    created_at     INTEGER NOT NULL,
    broadcast_at   INTEGER,
    confirmed_at   INTEGER
);

CREATE INDEX idx_refunds_invoice ON refunds(invoice_id);
CREATE INDEX idx_refunds_status  ON refunds(status);
