//! codypendent-sandbox — the plugin **security boundary** (Phase 6).
//!
//! This crate exists because it draws a trust boundary (the manual's crate rule):
//! everything a plugin declares is *untrusted input* until this crate has parsed,
//! verified, and gated it. It carries no daemon or agent-framework code, so the
//! security decisions are exercised in isolation.
//!
//! The pieces, in lifecycle order:
//!
//! * [`manifest`] — parse `plugin.toml` (the [`docs/specs/plugin.toml`] shape):
//!   identity, runtime, capabilities, resources, security record, update policy.
//! * [`verify`] — checksum (sha256) + publisher signature (ed25519) verification,
//!   with the default-**deny** unsigned policy.
//! * [`permission`] — the [`CapabilitySet`](permission::CapabilitySet) and the
//!   **permission diff** that blocks a capability-expanding update until it is
//!   re-approved (exit criterion 2).
//! * [`profile`] — lowering a granted capability set into a **closed**
//!   [`SandboxProfile`](profile::SandboxProfile): env allowlist, pre-opened paths,
//!   network allowlist, resource caps. An executor that honours it cannot grant
//!   an undeclared path or host (exit criterion 1).
//! * [`lifecycle`] — the discover → verify → install-disabled → smoke-test →
//!   enable → update → revoke state machine, carrying each plugin's trust record.
//! * [`sanitize`] — neutralize untrusted plugin/MCP output (label by origin,
//!   size-cap, strip control sequences) before it enters context.
//!
//! What this crate contains as of STEP 6.2: the [`executor`] — the OS-level
//! enforcement seam (`sandbox-exec` on macOS, `bwrap` arg-generation on Linux, a
//! fail-closed refusal elsewhere) that *consumes* a [`SandboxProfile`](profile::SandboxProfile)
//! and actually confines a process — and the [`trust_store`], the data-only
//! trusted-publisher key store that gives [`verify_artifact`](verify::verify_artifact)
//! real keys to verify against. What it still defers (named, not faked): the
//! `wasmtime` WASM component runtime and the brokered-secrets daemon.
//!
//! [`docs/specs/plugin.toml`]: ../../docs/specs/plugin.toml

pub mod executor;
pub mod lifecycle;
pub mod manifest;
pub mod permission;
pub mod profile;
pub mod sanitize;
pub mod trust_store;
pub mod verify;

pub use executor::{
    bwrap_argv, enforcing_executor, seatbelt_profile, CapabilityReport, RefusingSandbox,
    SandboxBackend, SandboxCommand, SandboxError, SandboxExecutor, SandboxOutcome,
};
pub use lifecycle::{InstalledPlugin, LifecycleError, LifecycleState, TrustTier};
pub use manifest::{
    parse_manifest, CapabilitiesSpec, ManifestError, PluginKind, PluginManifest, ResourcesSpec,
    RuntimeSpec, SecuritySpec, UpdateSpec, SUPPORTED_PLUGIN_SCHEMA_VERSION,
};
pub use permission::{Capability, CapabilitySet, PermissionDiff};
pub use profile::{SandboxProfile, ENV_ALLOWLIST};
pub use sanitize::{sanitize_untrusted, Sanitized};
pub use trust_store::{TrustStoreError, TrustedPublishers};
pub use verify::{
    checksum_of, signing_digest, verify_artifact, UnsignedPolicy, Verified, VerifyError,
};
