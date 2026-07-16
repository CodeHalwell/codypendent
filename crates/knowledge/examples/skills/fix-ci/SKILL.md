# Fix Rust CI

Diagnose and repair a failing Rust GitHub Actions run with the smallest change
that turns the pipeline green again. Work one failure at a time — a CI run that
fails for several reasons is several passes of this procedure, not one.

## When to use

- A GitHub Actions check on a Rust project is red (`cargo test`, `cargo clippy`,
  or `cargo fmt --check`).
- A reviewer reports "the CI test is failing" and points at a job or a test name.

## Procedure

1. **Read the failing output.** Pull the failing job's log and find the first
   real error — the compiler's first `error[...]`, the first `FAILED` test line,
   or the first clippy `warning`/`error`. Later lines are usually fallout from
   the first; fix the cause, not the cascade.
2. **Locate the test or symbol.** Search the workspace for the failing test name
   or the symbol named in the error (`workspace.search`), then open the file
   around it (`workspace.read_file`) to read the code and its assertions in
   context.
3. **Reproduce locally.** Run the narrowest command that recreates the failure:
   - a single test: `cargo test <test_name> -- --exact`
   - a package's tests: `cargo test -p <crate>`
   - lint failures: `cargo clippy --all-targets`
   - format failures: `cargo fmt --all -- --check`
   Confirm you see the *same* error the CI saw before changing anything.
4. **Make a minimal fix.** Change as little as possible to address the root
   cause. Prefer correcting the code under test over editing the assertion,
   unless the test itself encodes the wrong expectation — in which case fix the
   test and say so. Never silence a failure by deleting or `#[ignore]`-ing a test.
5. **Rerun to green.** Re-run the exact command from step 3, then widen to the
   whole gate the CI runs (`cargo test`, then `cargo clippy --all-targets`, then
   `cargo fmt --all -- --check`) so a local fix does not shift the failure
   elsewhere.
6. **Summarize.** Report what failed, the root cause, the change you made, and
   the commands that now pass — so the reviewer can verify without re-deriving
   the diagnosis.

## Guardrails

- Stay inside the granted worktree; this skill never touches paths outside
  `$REPOSITORY` / `$WORKTREE`.
- Only `cargo` and `git` are permitted commands.
- Scripts under `scripts/` are references only in this phase — they are recorded
  and displayed but not executed by the runtime yet.
