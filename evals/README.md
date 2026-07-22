# Codypendent evaluation corpus

The benchmark task set for `codypendent eval run` (Phase 7 STEP 7.1, [Chapter
16](../docs/docs/16-testing-strategy.md)). Every case is a Chapter 16
`EvalCase` — a pinned `repository_revision`, a `prompt`, a `policy`, a list of
objective `Assertion`s, and cost/duration budgets — run headlessly over the
JSONL client and scored against what actually happened, never against a
model's own account of what it did.

## Layout

```
evals/
  tasks/
    core/            # the runnable core suite — see below
      001-....json
      ...
  fixtures/
    tiny-crate.bundle  # a vendored git repository, one pinned commit
```

- **`evals/tasks/<suite>/*.json`** — one `EvalCase` per file. `codypendent
  eval run --suite <suite>` loads every `*.json` file directly under that
  directory (non-recursive), in filename order — hence the numeric prefixes.
- **`evals/fixtures/<name>.bundle`** — a fixture repository vendored as a `git
  bundle`, not a plain checkout. A plain checkout would need its own nested
  `.git` directory, which the *parent* repository (this one) would then treat
  as a submodule gitlink rather than tracked file content — a bundle is an
  ordinary blob to the parent repo, and `git clone` accepts a bundle file
  directly as a clone source, exactly like a live remote. `codypendent eval
  run` clones the suite's bundle into a fresh scratch directory per case
  (never mutating the vendored bundle) and checks out that case's pinned
  `repository_revision`.
- A suite's fixture is resolved by **name convention**: `evals/tasks/<suite>/`
  runs against `evals/fixtures/<name>.bundle`, where `<name>` is currently
  hardcoded to `tiny-crate` in `codypendent-cli`'s `commands::eval_run`.
  `EvalCase` itself carries only a `repository_revision`, not a repository
  path — see "Growing the corpus" below for how a multi-fixture suite would
  extend this.

## The core suite (`evals/tasks/core/`)

11 cases (this task's brief asked for a real, runnable 8–12; the full 50–100
the roadmap eventually wants is a separate, later content-authoring effort —
see below). Every case runs against the **same single pinned commit**
(`8e7644ddbbe0dd04052b47f0e2bfefd45b535ee6`) of the vendored
`codypendent-eval-fixture` crate — a tiny, dependency-free Rust crate with:

- one deliberate bug (`math::add_one` is off by one — `math::tests::
  add_one_increments` fails against the pinned commit);
- one undocumented function (`greet::loud_greet`);
- one broken CI config (`.github/workflows/ci.yml` never checks out the
  repository before running `cargo test`).

Task classes covered (the six the brief named): failing-test-diagnosis
(`002`), small-bug-fix (`001`, `009`), regression-test-addition (`003`),
doc-update (`004`, `011`), ci-diagnosis (`005`), safe-refactor (`006`, `008`,
`010`). Also covered: an architecture-explanation-style read-only case
(`007`) and a PR-feedback-response case (`009`) from the broader Chapter 16
list. The three assertion kinds this task's brief specifically required at
least one of each: `no-forbidden-network` (`001`, `006`, `007`),
`approval-requested` (`001`, `008`), `command-not-executed` (every case).

**A whole-suite caveat, by design:** `RunObservation::tests_passed` is a
single pass/fail for the *entire* fixture's `cargo test` run, not per-test
(Chapter 16's `EvalCase` shape doesn't carry a test filter). Concretely, this
means a case can only honestly assert `tests-pass` if resolving it *also*
fixes `math::add_one`'s pre-existing failure — cases `001` and `009` do;
every other case that changes the repository deliberately leaves that bug
alone and so does **not** assert `tests-pass`. Growing the corpus with a
multi-fixture-revision suite (see below) removes this constraint.

### How a case is run and scored

`codypendent eval run` (`crates/cli/src/eval.rs`) builds the objective
`RunObservation` two ways:

1. **From the run's own event stream** — `approval_requested`,
   `executed_commands`, `network_hosts`, `cost_usd` come from
   `ApprovalRequested`/`ApprovalResolved`/`BudgetWarning` events as the run
   streams by. Only an **approved** action counts as executed/contacted; a
   rejected proposal never ran. An action that somehow executes *without*
   going through the approval flow is invisible to this — every
   allow-listed shell command in this codebase's default policy requires
   approval (`crates/daemon/src/policy/mod.rs`), so this is a narrow,
   documented gap, not a silent one.
2. **From the checked-out working tree, after the run completes** —
   `changed_files` (tracked + untracked diff against the pinned revision),
   `existing_symbols` (a literal `git grep`, checked only when a case
   actually asserts `symbol-exists`), and `tests_passed` (a real `cargo test`
   in the checkout, checked only when a case asserts `tests-pass`). These
   facts live in the repository, not on the wire.

`correct_citations` has **no signal yet** — no event carries a claim/source
pair — so it is always empty and a `citation-correct` assertion would always
fail. No case in this suite uses it; see "Deferred" below.

## Growing the corpus to 50–100

1. **More cases against the same fixture.** The cheapest growth path: add
   more `evals/tasks/core/NNN-*.json` files against the existing pinned
   commit. Keep the "does this assertion set need `tests-pass`" rule above in
   mind, or extend the fixture with a second commit (see next point).
2. **A second pinned commit in the same fixture.** `git bundle create` again
   after adding more commits to the same working tree (`git bundle create
   evals/fixtures/tiny-crate.bundle --all` captures every ref/commit, so old
   pinned revisions keep resolving). A later commit that fixes `add_one` lets
   new cases assert `tests-pass` freely without touching that history.
3. **A second fixture.** Vendor another tiny crate the same way (build it as
   its own git repo, `git bundle create evals/fixtures/<new-name>.bundle
   --all`), add `evals/tasks/<new-suite>/`, and update
   `commands::eval_run`'s hardcoded fixture name to read a per-suite manifest
   instead (e.g. a `evals/tasks/<suite>/suite.toml` naming its fixture) —
   today it is a single hardcoded string because there is only one suite.
4. **Vendoring this repository itself at a fixed revision** (the brief's
   other suggested option) works the same way: `git bundle create
   evals/fixtures/codypendent-self.bundle <sha>` from a shallow or full clone
   of this repository, pinned to a specific commit. Prefer a small, purpose-
   built fixture like `tiny-crate` for most cases — `cargo test` on the real
   workspace is far slower per case, and a case designer rarely needs the
   whole codebase's surface area.
5. **Task classes still uncovered here** that the roadmap's full corpus wants:
   architecture explanation (partially covered, `007`), PR-feedback response
   (partially covered, `009`). Add cases as the fixture(s) grow.

## CI smoke

`.github/workflows/ci.yml`'s `eval-smoke` job runs:

- `cargo test -p codypendent-eval` — the harness's own scoring/promotion unit
  and integration tests, including `corpus_it.rs`, which loads the *real*
  `evals/tasks/core/` suite shipped here and checks its shape (parses, ids
  unique, required task classes present, the three mandated assertion kinds
  each appear, a fixed-revision consistency check).
- `cargo test -p codypendent-cli --test eval_it` — a deterministic,
  hand-rolled mock daemon (no `codypendentd` subprocess, no live model) drives
  the exact same runner code path (`eval::run_case`) end to end, including
  real `git`/`cargo test` repository inspection against a real throwaway git
  repo it builds on the fly. It proves a known-pass case passes and a
  known-fail case fails — the "mock model" here is the mock daemon's scripted
  behaviour, which is deterministic by construction (see the test file's own
  doc comment for why this, rather than faking the model-provider wire
  protocol, is the appropriately-scoped mock for this task).

Running the *real* corpus against a live daemon and a real (or local) model
is not part of CI (no API key / local model is available there) — do it by
hand: `codypendent eval run --suite core --report out.json` after `codypendent
daemon start` and a configured `models.toml`.

## Deferred (named, not faked)

- **The full 50–100 case corpus.** This task ships a real, runnable 11-case
  core suite per its brief's explicit scope; growing it further is a
  separate, large content-authoring effort (see above).
- **Citation checking.** `correct_citations` has no wire signal; wiring one
  (an event or artifact carrying a claim → source mapping) is future work.
- **Cost accounting fidelity.** `cost_usd` is read from the last
  `BudgetWarning { dimension: Cost }` event, if any; a run that never emits
  one reports `0.0` — real, not fabricated, but not necessarily the model
  provider's actual invoice.
- **Routing-policy enforcement.** A case's `policy` field is recorded in the
  CLI's stdout summary but does not yet select a model — Phase 7's router is
  not wired into `StartRun` (see the roadmap's "routing⇄eval composition"
  note); every case runs under whatever the daemon's own `models.toml`
  resolves for `AgentMode::Build`.
