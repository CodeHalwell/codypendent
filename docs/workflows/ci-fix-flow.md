# CI Fix Flow (`/fix-ci`)

`/fix-ci` repairs a failed GitHub check on a pull request. Since Phase 5
(STEP 5.1.4) it is **not** a hard-coded objective prompted into a single agent —
it starts the declarative `repair-github-check` workflow and runs it through the
workflow engine, so the sequence is a supervised multi-agent flow with structural
guarantees rather than instructions in a prompt.

## Invocation

```
codypendent fix-ci --pr <N> [--repo <PATH>]
```

The client sends `StartWorkflow` naming the workflow by id (`repair-github-check`)
with the PR number as its typed input and the repository the run operates on
(each writing node gets its own isolated worktree). The daemon resolves the
workflow from its sources and drives it in the background; `codypendent workflow
watch <run>` streams its progress.

## The workflow (`docs/specs/workflow.yaml`)

| Step | Kind | What it does |
|------|------|--------------|
| `inspect` | agent (investigator) | Reads the failed check + logs, posts a `finding`. |
| `patch` | agent (implementer) | Proposes a fix in an **isolated worktree**; the daemon captures the diff as `proposed_patch`. Writes are approval-gated. |
| `verify` | tool (`repository.test`) | Applies the proposed patch into its own worktree and re-runs the tests, posting `test_result`. Running an applied (untrusted) patch parks for approval. |
| `review` | agent (**independent** reviewer) | Judges the change and posts a `decision`. The reviewer is a separate agent so its judgement is structurally independent of the implementer (ADR-008). |
| `publish` | tool (`github.update-pull-request`) | Updates the PR with the outcome. **Always** approval-gated. |

Agents never exchange raw transcripts; they communicate only through typed
blackboard artifacts (`finding`, `proposed_patch`, `test_result`, `decision`) and
declared node outputs.

## Where the definition comes from

`repair-github-check` ships as an **embedded built-in**, so a fresh install runs
`/fix-ci` with no repository file. A project can override it by placing a manifest
at `<repo>/.codypendent/workflows/*.yaml` (repository scope), or a user can add
one under `<data_dir>/workflows` (user scope); precedence is repository > user >
built-in. A published `(id, version)` is immutable: declaring the same id and
version with different content in two sources is a load error — bump the
workflow's `version` to change it (the clean way to shadow the built-in).

## Approval gates

Every externally visible write parks for durable approval before it happens: the
implementer's worktree write, running the applied patch in `verify`, and the PR
update in `publish`. A rejected or denied write fails the run and **never** calls
GitHub. Without a GitHub token configured, `/fix-ci` fails with
`github is not configured (no token available)` — the same legible error the
earlier prompt flow gave.

## Divergence from the Phase-3 flow (intentional)

The old prompt flow posted a separate `github.create_check_run_summary` in
addition to updating the PR. The declarative workflow does **not**: its single
`publish` step (the approval-gated PR update) carries the outcome onto the pull
request. This is a deliberate simplification recorded in the workflow's
`description`; a plain single-agent run can still post a check-run summary if one
is specifically wanted.
