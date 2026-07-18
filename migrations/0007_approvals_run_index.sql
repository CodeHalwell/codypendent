-- The auto-approval check (`ApprovalBroker::run_scoped_match`) runs
--   WHERE run_id = ? AND scope = 'run' AND state = 'approved'
-- on EVERY approval request, against a table that only ever grows (rows are
-- never purged). Without an index that is a full-table scan per request.
CREATE INDEX idx_approvals_run_scope_state ON approvals (run_id, scope, state);
