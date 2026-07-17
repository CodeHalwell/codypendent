-- Phase 2 follow-up — code-graph per-file symbol identity (issue #6, items 4 & 5).
--
-- The syntax-layer graph originally keyed a node purely by its
-- `(repository, symbol_key)`, where `symbol_key` folds only name + kind +
-- signature. Two files with a same-named, same-signature top-level symbol
-- (e.g. a `fn init` in both `src/foo.rs` and `src/bar.rs`) therefore collapsed
-- onto one row, and the second file's edge-replacement could delete the first
-- file's edges — silently dropping one module's APIs from the repository map
-- (item 5, an active full-scan bug).
--
-- `source_path` records the repo-relative file a node was parsed from. It is
-- folded into `symbol_key` (so cross-file symbols never collide) and lets a
-- single-file reparse retire exactly the symbols that file no longer defines
-- (item 4, the incremental-watcher path). Existing rows predate the code graph
-- in practice — the startup scan wipes and rebuilds the graph — so a nullable
-- add-column is sufficient; new inserts always carry the path.
ALTER TABLE code_nodes ADD COLUMN source_path TEXT;

-- Retirement (item 4) and edge-replacement scope a file's own nodes; index that
-- lookup alongside the existing per-repository index.
CREATE INDEX idx_code_nodes_source ON code_nodes(repository, source_path);
