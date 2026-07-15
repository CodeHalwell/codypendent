# Master Acceptance Checklist

The final gate. Run this after Phase 7 (or, for an interim release, after any completed phase — sections are cumulative and marked by phase). Everything here restates exit criteria and release gates already defined in the [Roadmap](../15-roadmap.md), [Testing Strategy](../16-testing-strategy.md), and phase chapters; nothing new is introduced. A release candidate ships only when every applicable box is checked.

## 1. Hygiene (every release, any phase)

- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --workspace` green.
- [ ] `cargo deny check` and `cargo audit` clean or with documented, dated exceptions.
- [ ] CI green on the release commit; working tree clean; every migration file unchanged since its first commit.

## 2. Phase exit criteria (cumulative)

- [ ] **Phase 0:** `daemon start` / `status --json` / `stop`; restart preserves `instance_id` and increments `boot_count`; fixture event log replays deterministically.
- [ ] **Phase 1:** disconnect-survival; duplicate-command idempotency; kill-9 recovery ("recovers or cleanly marks the run"); reviewable + attributable patches; unmerged-work-protecting worktree cleanup; Explore-cannot-write; status line; JSONL parity; chronicle v0; safe-point steering.
- [ ] **Phase 2:** retrieval eval gate (recall@8 ≥ 0.8, forbidden exclusion 100%, within budget); skill permissions visible; memory-opens-its-source; `index rebuild` restores derived state; cross-repository memory isolation.
- [ ] **Phase 3:** TUI+IDE same-run parity; unsaved-buffer provenance labeled; idempotent approval-gated PR writes; webhook replay safety + forged-signature rejection.
- [ ] **Phase 4:** concurrent edits merge; reproducible (byte-identical) document snapshots; symbol change flags affected docs; every graph edge shows evidence + revision; CRDT decision ADR recorded with numbers.
- [ ] **Phase 5:** no shared writable worktrees between agents; workflow resumes after daemon restart with no duplicate effects; node-level cost/provenance visible; single-agent baseline selectable; orchestration reasons declared.
- [ ] **Phase 6:** undeclared path/network access fails with audit; permission-expansion updates blocked pending approval; original audio/image artifacts linked; setup assistant structurally cannot make sensitive changes; themes carry no execution permissions.
- [ ] **Phase 7:** router beats static-strongest on cost at threshold quality (report attached); self-promotion structurally impossible; regression suite covers historical failures; promoted versions attributable and reversible.

## 3. Recovery matrix ([Chapter 16](../16-testing-strategy.md))

Failure injection at each point, with the documented restart state verified:

- [ ] After command persistence. — [ ] Before external effect. — [ ] After effect, before outcome persistence. — [ ] During model stream. — [ ] During shell execution. — [ ] During worktree creation. — [ ] During artifact write. — [ ] During checkpoint. — [ ] During client catch-up.

## 4. Security regression suite ([Chapter 16](../16-testing-strategy.md) / [Chapter 11](../11-security-and-governance.md))

- [ ] Path traversal. — [ ] Symlink escape. — [ ] Command injection (structured commands; interpreter requires approval). — [ ] Environment leakage. — [ ] Unauthorized network. — [ ] Malicious MCP output labeled/sanitized. — [ ] Skill prompt injection cannot grant permissions. — [ ] Plugin permission escalation blocked. — [ ] Forged GitHub webhook rejected. — [ ] Replayed approval rejected. — [ ] Cross-repository memory leakage impossible. — [ ] Secrets never in model context, events, or logs (scan). — [ ] Deny-wins and no-lower-scope-widening property tests. — [ ] Data classification gates provider eligibility.

## 5. Protocol and persistence compatibility

- [ ] Previous-version protocol fixture corpora replay (handshake, unknown fields, unknown enum variants).
- [ ] Persisted event fixtures from every prior release deserialize.
- [ ] Snapshot migration path tested; incompatible major version rejected with structured error.
- [ ] Migration replay: fresh DB from `0001` to head equals upgraded DB schema.

## 6. Worktree suite ([Chapter 16](../16-testing-strategy.md))

- [ ] Nested path rejection. — [ ] Stale record reconciliation. — [ ] Unmerged commit protection. — [ ] Dirty file preservation. — [ ] Owned process cleanup. — [ ] Simultaneous creation. — [ ] Symlink boundary. — [ ] Branch collision.

## 7. Interaction-model acceptance ([Chapter 16](../16-testing-strategy.md))

- [ ] Explore cannot write. — [ ] Plan emits a versioned plan. — [ ] Build stays within its worktree. — [ ] Plan changes trigger reapproval. — [ ] Steering applies at a safe point. — [ ] Forks isolate mutable state. — [ ] Model switching preserves artifacts. — [ ] Selective apply is correct. — [ ] JSONL and TUI observe equivalent events. — [ ] Chronicles let a fresh agent resume work. — [ ] (When remote attach ships) revocation works.

## 8. Evaluation gates

- [ ] Core benchmark suite (50–100 tasks) at or above the phase's agreed success threshold.
- [ ] Retrieval metrics (recall@k, precision@k, MRR, unsafe exclusion) at threshold.
- [ ] Routing comparison report generated and attached.
- [ ] Regression suite green.

## 9. Product release gates ([Chapter 16](../16-testing-strategy.md) / [TIMELINE](../../TIMELINE.md))

- [ ] Clean install and uninstall on each supported OS (data dir removal documented and complete).
- [ ] Artifact + config backup/restore drill passes.
- [ ] TUI: keyboard/mouse equivalence table verified; capability fallbacks (256/16-color, no-mouse) render; no blocking calls on the render thread.
- [ ] Docs: README quick-start works on a fresh machine without private guidance; roadmap and known limitations published; terminology frozen (comedy vocabulary cosmetic only; no `cody` binary anywhere).
- [ ] Permission-deny precedence re-tested at this release (TIMELINE risk track).
- [ ] SECURITY.md contact path verified; supported-versions table updated.

## 10. Sign-off

- [ ] Every ADR affected by this release is updated (CONTRIBUTING list: daemon/client authority, event ordering, persistent data, protocol compatibility, security boundary, plugin execution, framework ownership, model data policy).
- [ ] Release notes include: routing report, eval summary, promoted/rolled-back versions, known limitations.
- [ ] A human owner has read this checklist and signed the release commit.
