//! Per-session event fan-out to subscribed clients (STEP 1.11).
//!
//! Populated by STEP 1.11: a `tokio::sync::broadcast` per session, filtered by
//! each client's subscriptions; slow clients fall back to re-attach.
