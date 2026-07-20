//! Artifact verification (STEP 6.1): checksum and publisher signature.
//!
//! Before a plugin is ever installed (even disabled), its artifact bytes are
//! matched against the manifest's `sha256:` checksum, and — when the manifest is
//! signed — the checksum is verified against the publisher's ed25519 key. An
//! *unsigned* plugin follows policy `[plugins].unsigned`, which defaults to
//! **deny**: a plugin with no signature is refused unless the operator has
//! explicitly opted into allowing unsigned plugins.
//!
//! The checksum binds the manifest to a specific artifact. The signature binds a
//! **canonical digest of the entire manifest** (every field except the signature
//! itself) to a publisher (see [`signing_digest`]). Signing the whole manifest
//! means a valid publisher signature cannot be replayed against a manifest whose
//! `[capabilities]`, runtime command, resource caps, scopes, or any other field
//! has been altered: change any signed field and the digest — and so the required
//! signature — changes with it.

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::manifest::PluginManifest;

/// Policy for plugins whose manifest carries no signature (`[plugins].unsigned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnsignedPolicy {
    /// Refuse unsigned plugins. The default posture (STEP 6.1).
    #[default]
    Deny,
    /// Allow unsigned plugins (operator opt-in; development installs).
    Allow,
}

/// Why verification failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    /// The manifest's checksum is not the `sha256:<hex>` shape.
    #[error("malformed checksum (expected `sha256:<hex>`): {0}")]
    MalformedChecksum(String),
    /// The artifact bytes do not hash to the manifest's checksum.
    #[error("checksum mismatch: manifest {expected}, artifact {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    /// The plugin is unsigned and policy denies unsigned plugins.
    #[error("plugin is unsigned and policy denies unsigned plugins")]
    UnsignedDenied,
    /// The publisher key could not be decoded.
    #[error("invalid publisher key: {0}")]
    InvalidPublisherKey(String),
    /// The signature could not be decoded.
    #[error("invalid signature encoding: {0}")]
    InvalidSignature(String),
    /// The signature did not verify against the publisher key.
    #[error("signature does not verify against the publisher key")]
    SignatureMismatch,
}

/// The outcome of verifying an artifact — the trust facts a lifecycle records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// Whether a real signature was present and verified.
    pub signed: bool,
}

/// Compute the `sha256:<hex>` checksum of some artifact bytes, in the manifest's
/// canonical form.
#[must_use]
pub fn checksum_of(artifact: &[u8]) -> String {
    let digest = Sha256::digest(artifact);
    format!("sha256:{}", hex::encode(digest))
}

/// The 32-byte digest a publisher signs: a SHA-256 over the **entire manifest** —
/// every field except the signature itself — canonically serialized. Signing the
/// whole manifest, rather than an enumerated subset of fields, binds *every*
/// security-relevant field at once: the requested capabilities, the runtime
/// execution identity (command/protocol/working directory), the resource-isolation
/// caps, the installable scopes, the sandbox profile, the update policy, and the
/// artifact checksum. A valid signature can therefore never be replayed against a
/// manifest that was altered in *any* way — there is no unsigned field left to
/// tamper with, which forecloses the whole class of "field X wasn't signed" replays.
///
/// The serialization is deterministic (the manifest is all named struct fields,
/// enums, scalars, and ordered `Vec`s — no maps or floats), so signer and verifier
/// reproduce identical bytes; it is unambiguous (JSON escapes every value), so no
/// field value can forge a delimiter; and its length is committed under the hash.
/// The signature field is blanked before serializing, since a signature cannot sign
/// itself. This is the signing contract a packaging tool implements.
///
/// The `codypendent-plugin-signature-v1` domain-separation tag **versions the
/// signature scheme**: a signature only verifies against the scheme it was produced
/// under, and a future scheme change bumps the tag so the two never collide. This
/// crate deliberately accepts **only** the current scheme — there is no fallback to
/// verifying a bare-checksum signature. Accepting that weaker form would reopen
/// exactly the field-swap replays this digest closes, so it is refused by design
/// rather than kept for backward compatibility (no signed plugins ship against an
/// older contract — signed packaging targets this digest from the outset).
#[must_use]
pub fn signing_digest(manifest: &PluginManifest) -> [u8; 32] {
    // Sign the whole manifest minus the signature field (which cannot sign itself),
    // canonically serialized — so no field is left unbound and there is no
    // enumerate-every-security-field footgun.
    let mut signable = manifest.clone();
    signable.security.signature = String::new();
    let canonical = serde_json::to_vec(&signable).expect("plugin manifest serializes");
    let mut hasher = Sha256::new();
    hasher.update(b"codypendent-plugin-signature-v1");
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(&canonical);
    hasher.finalize().into()
}

/// Verify a plugin artifact against its manifest under the given unsigned policy
/// and optional publisher key.
///
/// * The artifact must hash to the manifest's checksum.
/// * If the manifest is signed, `publisher_key` (32 raw ed25519 public-key bytes)
///   must verify the signature over the [`signing_digest`] — the digest of the
///   whole manifest, so a signature cannot be replayed against *any* altered field
///   (permissions, runtime command, resource caps, scopes, …).
/// * If the manifest is unsigned, [`UnsignedPolicy`] decides.
///
/// The publisher key is supplied by the caller (the daemon resolves it from the
/// trusted-publisher store keyed by `manifest.publisher`); passing `None` for a
/// signed manifest is treated as an invalid key.
pub fn verify_artifact(
    manifest: &PluginManifest,
    artifact: &[u8],
    publisher_key: Option<&[u8]>,
    unsigned: UnsignedPolicy,
) -> Result<Verified, VerifyError> {
    // 1. Checksum shape + match. The checksum binds manifest → artifact and is
    //    checked first: a signature over a checksum means nothing if the checksum
    //    does not describe the bytes in hand.
    let expected = manifest.security.checksum.trim();
    let expected_hex = expected
        .strip_prefix("sha256:")
        .ok_or_else(|| VerifyError::MalformedChecksum(expected.to_string()))?;
    if expected_hex.is_empty() || hex::decode(expected_hex).is_err() {
        return Err(VerifyError::MalformedChecksum(expected.to_string()));
    }
    let actual = checksum_of(artifact);
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(VerifyError::ChecksumMismatch {
            expected: expected.to_string(),
            actual,
        });
    }

    // 2. Signature (or unsigned policy).
    if !manifest.security.is_signed() {
        return match unsigned {
            UnsignedPolicy::Allow => Ok(Verified { signed: false }),
            UnsignedPolicy::Deny => Err(VerifyError::UnsignedDenied),
        };
    }

    let key_bytes = publisher_key.ok_or_else(|| {
        VerifyError::InvalidPublisherKey("no publisher key supplied for a signed plugin".into())
    })?;
    let key_arr: [u8; 32] = key_bytes.try_into().map_err(|_| {
        VerifyError::InvalidPublisherKey(format!("expected 32 bytes, got {}", key_bytes.len()))
    })?;
    let key = VerifyingKey::from_bytes(&key_arr)
        .map_err(|e| VerifyError::InvalidPublisherKey(e.to_string()))?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest.security.signature.trim())
        .map_err(|e| VerifyError::InvalidSignature(e.to_string()))?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().map_err(|_| {
        VerifyError::InvalidSignature(format!("expected 64 bytes, got {}", sig_bytes.len()))
    })?;
    let signature = Signature::from_bytes(&sig_arr);

    // The signed message is the canonical digest binding the artifact identity AND
    // the security-relevant manifest fields (id/version/kind/capabilities), so a
    // valid signature can't be replayed against a manifest whose permissions,
    // runtime kind, or identity were swapped.
    key.verify_strict(&signing_digest(manifest), &signature)
        .map_err(|_| VerifyError::SignatureMismatch)?;
    Ok(Verified { signed: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};

    fn manifest_with(checksum: &str, signature: &str) -> PluginManifest {
        let toml = format!(
            r#"
schema_version = 1
id = "wc"
name = "Word Count"
version = "0.1.0"
kind = "wasm-component"
publisher = "me"
[runtime]
command = "word_count.wasm"
[security]
checksum = "{checksum}"
signature = "{signature}"
"#
        );
        parse_manifest(&toml).expect("manifest parses")
    }

    /// A manifest with a `[capabilities]` table, so a signature binds real
    /// permissions. `network` lists the requested hosts.
    fn manifest_with_caps(checksum: &str, network: &[&str]) -> PluginManifest {
        let net = network
            .iter()
            .map(|h| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            r#"
schema_version = 1
id = "wc"
name = "Word Count"
version = "0.1.0"
kind = "wasm-component"
publisher = "me"
[runtime]
command = "word_count.wasm"
[capabilities]
network = [{net}]
[security]
checksum = "{checksum}"
signature = "unset"
"#
        );
        parse_manifest(&toml).expect("manifest parses")
    }

    /// Sign a manifest's [`signing_digest`] with `key` and set its signature field
    /// — the packaging step, so the signature covers the manifest's real
    /// checksum + identity + capabilities.
    fn sign(manifest: &mut PluginManifest, key: &SigningKey) {
        let sig = key.sign(&signing_digest(manifest));
        manifest.security.signature =
            base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    }

    #[test]
    fn checksum_matches_are_accepted_when_unsigned_allowed() {
        let artifact = b"plugin bytes";
        let m = manifest_with(&checksum_of(artifact), "set-during-packaging");
        let v = verify_artifact(&m, artifact, None, UnsignedPolicy::Allow).expect("verifies");
        assert!(!v.signed);
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let m = manifest_with(&checksum_of(b"the real bytes"), "set-during-packaging");
        let err = verify_artifact(&m, b"tampered bytes", None, UnsignedPolicy::Allow).unwrap_err();
        assert!(matches!(err, VerifyError::ChecksumMismatch { .. }));
    }

    #[test]
    fn malformed_checksum_is_rejected() {
        let m = manifest_with("md5:whatever", "set-during-packaging");
        let err = verify_artifact(&m, b"x", None, UnsignedPolicy::Allow).unwrap_err();
        assert!(matches!(err, VerifyError::MalformedChecksum(_)));
    }

    #[test]
    fn unsigned_is_denied_by_default_policy() {
        let artifact = b"plugin bytes";
        let m = manifest_with(&checksum_of(artifact), "set-during-packaging");
        let err = verify_artifact(&m, artifact, None, UnsignedPolicy::Deny).unwrap_err();
        assert_eq!(err, VerifyError::UnsignedDenied);
    }

    #[test]
    fn valid_signature_verifies() {
        let artifact = b"signed plugin bytes";
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut m = manifest_with_caps(&checksum_of(artifact), &["api.github.com:443"]);
        sign(&mut m, &signing);
        let key = signing.verifying_key();
        let v = verify_artifact(&m, artifact, Some(key.as_bytes()), UnsignedPolicy::Deny)
            .expect("signature verifies");
        assert!(v.signed);
    }

    #[test]
    fn signature_from_the_wrong_key_is_rejected() {
        let artifact = b"signed plugin bytes";
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut m = manifest_with_caps(&checksum_of(artifact), &["api.github.com:443"]);
        sign(&mut m, &signing);
        // A different key must not verify this signature.
        let attacker = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let err = verify_artifact(
            &m,
            artifact,
            Some(attacker.as_bytes()),
            UnsignedPolicy::Deny,
        )
        .unwrap_err();
        assert_eq!(err, VerifyError::SignatureMismatch);
    }

    #[test]
    fn signature_over_a_tampered_artifact_is_rejected_at_checksum() {
        // A signed manifest for the real bytes, but the artifact shipped is
        // different: checksum fails before signature is even considered.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut m = manifest_with_caps(&checksum_of(b"the real bytes"), &["api.github.com:443"]);
        sign(&mut m, &signing);
        let key = signing.verifying_key();
        let err = verify_artifact(&m, b"tampered", Some(key.as_bytes()), UnsignedPolicy::Deny)
            .unwrap_err();
        assert!(matches!(err, VerifyError::ChecksumMismatch { .. }));
    }

    #[test]
    fn swapping_capabilities_after_signing_breaks_the_signature() {
        // The exit-criterion-2 attack the review flagged: a publisher signs a
        // narrow manifest; an attacker keeps the valid checksum + signature but
        // widens `[capabilities]`. The signature must no longer verify, because it
        // binds the capabilities, not just the checksum.
        let artifact = b"signed plugin bytes";
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut m = manifest_with_caps(&checksum_of(artifact), &["api.github.com:443"]);
        sign(&mut m, &signing);
        let key = signing.verifying_key();
        // The honest manifest verifies.
        assert!(verify_artifact(&m, artifact, Some(key.as_bytes()), UnsignedPolicy::Deny).is_ok());

        // Attacker widens the network allowlist, keeping the same signature.
        m.capabilities
            .network
            .push("exfiltrate.example.com:443".to_string());
        let err =
            verify_artifact(&m, artifact, Some(key.as_bytes()), UnsignedPolicy::Deny).unwrap_err();
        assert_eq!(
            err,
            VerifyError::SignatureMismatch,
            "widened capabilities break the signature"
        );
    }

    #[test]
    fn signing_digest_changes_with_the_security_relevant_fields() {
        // Sanity: the digest binds checksum + id/version/kind + capabilities.
        let base = manifest_with_caps(&checksum_of(b"x"), &["a:1"]);
        let d0 = signing_digest(&base);

        let mut widened = base.clone();
        widened.capabilities.network.push("b:2".into());
        assert_ne!(signing_digest(&widened), d0, "capabilities are bound");

        let mut different_version = base.clone();
        different_version.version = "9.9.9".into();
        assert_ne!(signing_digest(&different_version), d0, "version is bound");

        let mut different_command = base.clone();
        different_command.runtime.command = "totally-different".into();
        assert_ne!(
            signing_digest(&different_command),
            d0,
            "runtime command is bound"
        );

        let mut weaker_limits = base.clone();
        weaker_limits.resources.memory_mb += 100_000;
        assert_ne!(
            signing_digest(&weaker_limits),
            d0,
            "resource caps are bound"
        );
    }

    #[test]
    fn capability_value_injection_does_not_collide_with_a_split_capability() {
        // The review's attack: manifest A declares ONE filesystem_read whose value
        // embeds delimiter-looking bytes; manifest B declares that same safe read
        // PLUS a real network capability. Under a naive newline-joined encoding
        // these two could hash equal, letting A's signature authorize B's added
        // network access. The length-prefixed encoding must keep them distinct.
        let checksum = checksum_of(b"x");
        let mut a = manifest_with_caps(&checksum, &[]);
        a.capabilities.filesystem_read = vec!["safe\ncap-value=8\nevil:443".into()];
        let mut b = manifest_with_caps(&checksum, &["evil:443"]);
        b.capabilities.filesystem_read = vec!["safe".into()];
        assert_ne!(
            signing_digest(&a),
            signing_digest(&b),
            "an injected capability value must not collide with a split capability set"
        );

        // Concretely: a valid signature over A must not verify B (which gained
        // network access), even though both share the checksum + publisher key.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let key = signing.verifying_key();
        sign(&mut a, &signing);
        b.security.signature = a.security.signature.clone();
        let err =
            verify_artifact(&b, b"x", Some(key.as_bytes()), UnsignedPolicy::Deny).unwrap_err();
        assert_eq!(
            err,
            VerifyError::SignatureMismatch,
            "A's signature must not authorize B"
        );
    }
}
