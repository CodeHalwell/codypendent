# Reference: reproducing Rust CI failures locally

The narrowest commands that recreate each class of GitHub Actions failure.

| Failure in CI | Reproduce locally |
| --- | --- |
| One test failed | `cargo test <test_name> -- --exact` |
| A crate's tests failed | `cargo test -p <crate>` |
| Whole workspace tests | `cargo test` |
| Clippy denied | `cargo clippy --all-targets` |
| Formatting drift | `cargo fmt --all -- --check` |
| Build broke | `cargo build --all-targets` |

Notes:

- `-- --exact` after a test name avoids running every test whose name is a
  prefix match.
- Clippy and the test gate can disagree; run both before declaring success.
- `cargo fmt --all -- --check` only reports drift — run `cargo fmt --all` to
  actually apply it, then re-check.
