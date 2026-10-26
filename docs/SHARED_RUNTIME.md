# Shared runtime integration with Son of Anton

Codypendent remains the Rust runtime, TUI and Tauri desktop product. Son of Anton remains a separate Code OSS IDE. Beadsmith remains unchanged; its useful timeline, memory provenance and reviewable-increment ideas are implemented through the existing checkpoint, patch, workflow and knowledge machinery.

The Node transport, discovery, session controller and bounded activity projection now live in `sdk/protocol`. The existing Codypendent editor extension and Anton consume that implementation. Rust-generated schemas and golden vectors govern wire contracts; protocol 1.10 adds authenticated knowledge-card reads. Clients cannot provide scope keys: the daemon resolves the owned session's workspace and repository and checks daemon-store access.

Anton vendors a versioned SDK archive and a hash-verified archive of Rust build inputs. The companion repository records both the base commit and whether local changes were included. Ordinary builds require no sibling checkout. Unix installers bundle the CLI/daemon and the confined UI-worker launcher. Windows currently retains the editor's native compatibility runtime.

Sessions, runs, questions, approvals, workflows, repository task cards and execution policy remain daemon-owned. Disconnecting or switching a frontend stops observation. Only explicit cancellation stops a run. Correlated mutations time out and are never automatically replayed; clients reconcile a receipt or ask for an explicit retry using stable idempotency keys.

The SDK displays bounded retained history, preserves interleaved model/tool order and shows tool failures and checkpoint rollbacks. A completed run does not establish test success. Graph pages retain evidence and confidence rather than implying complete impact or test coverage. Missing usage remains unknown.

Skills, Memory, Journey and Docs list views in the TUI and desktop use `ReadKnowledge`. Existing local learning mutation/document replica adapters remain, as do local model/provider configuration and council-definition files. List replies are bounded to 100 cards and 8 MiB; native adapters cap collected pages at 10,000 cards. The underlying stores still need pagination/performance work for unusually large datasets.

## Verification

The Linux validation pass on 2026-10-26 observed 4,055 Rust workspace tests passing (9 ignored), strict workspace Clippy, 492 SDK tests, 307 existing editor tests, 242 desktop frontend tests and 57 desktop Rust transport tests. The complete Tauri bridge, release executable and Debian package also build. Both the release executable and the executable freshly extracted from the Debian package pass the real-window handoff test below.

`npm --prefix apps/desktop run test:native` is a real-window test using WebKitGTK WebDriver, an actual Rust daemon and a deterministic loopback model. It starts a run through the SDK, observes it in Tauri, closes the desktop while the daemon continues, reopens the window, verifies replay, and reads the same curated memory. Build the application with `--features app,tauri/custom-protocol` to embed the frontend. Supply `CODYPENDENT_TEST_BINARY`, `CODYPENDENT_DESKTOP_BINARY` and optionally `CODYPENDENT_SMOKE_OUTPUT`; run under `xvfb-run -a` on headless Linux. CI and release quality gates include this test.

Anton also tests the actual Rust TUI in a PTY with the bundled runtime, alongside its editor/controller and browser checks. These checks use temporary data, no live provider credentials, and no paid model calls. They do not establish macOS notarization, Windows shared-runtime support or stable installer readiness; those require platform-native release evidence.

## Dependency policy

The review CI identified advisories in existing dependencies. Both Rust lockfiles update rustls, webpki, root certificates and Tokio to patched releases; ZIP handling uses version 9. JavaScript development-tool lockfiles are refreshed without lowering audit thresholds. The existing dated-exception policy records `RUSTSEC-2024-0449`: `unsafe-libyaml` is unmaintained, with no reported vulnerability or safe compatible upgrade. Both `serde_yaml` and its compatible `serde_yaml_ng` successor depend on it. A migration to a different YAML parser requires separate round-trip, validation and error-behaviour review; the exception is explicit in `deny.toml` and must be revisited with that migration.
