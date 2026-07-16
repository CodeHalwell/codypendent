# Evaluation cases for `rust.fix-ci`

Hand-written scenarios the skill should handle. These are documentation of the
expected behaviour, not an executable harness (the evaluation runner arrives with
retrieval in STEP 2.3).

1. **Single failing test.** A job reports `test math::adds ... FAILED`. Expected:
   locate `math::adds`, reproduce with `cargo test math::adds -- --exact`, fix
   the implementation (not the assertion), rerun green.
2. **Clippy denial.** `cargo clippy` fails on `clippy::needless_return`. Expected:
   read the flagged line, remove the needless `return`, rerun `cargo clippy
   --all-targets`.
3. **Formatting drift.** `cargo fmt --all -- --check` reports a diff. Expected:
   run `cargo fmt --all`, then re-check.
4. **Cascading errors.** Several `error[E0308]` lines from one root type change.
   Expected: fix the first cause and confirm the cascade clears, rather than
   editing each downstream site.
