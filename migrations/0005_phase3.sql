-- Phase 3 — GitHub and IDE awareness.
--
-- Webhook delivery idempotency (STEP 3.3). GitHub retries webhook deliveries;
-- every delivery carries a unique `X-GitHub-Delivery` GUID. Recording that GUID
-- before producing any internal event makes ingestion replay-safe: a redelivered
-- payload (same GUID) is acknowledged but never normalized a second time
-- (exit criterion 4). The GUID is the primary key, so the INSERT itself is the
-- idempotency authority — a duplicate loses the insert and is skipped.
CREATE TABLE webhook_deliveries (
    delivery_id TEXT PRIMARY KEY,     -- X-GitHub-Delivery GUID
    event_type TEXT NOT NULL,         -- X-GitHub-Event (e.g. pull_request, check_run)
    received_at TEXT NOT NULL
);
