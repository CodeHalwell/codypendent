//! STEP 6.2 — the trusted-publisher key store gives verification real keys.
//!
//! End to end through the store resolver: a signed plugin from a publisher the
//! store does not know **fails closed**; the same plugin verifies once the
//! publisher's key is trusted; the store round-trips through its config file. This
//! is the wiring `plugin verify` (and a future stateful `plugin install`) drives —
//! `store.key_for(&manifest.publisher)` is the resolver threaded into
//! `verify_artifact` / `install_disabled`.

use base64::Engine;
use codypendent_sandbox::{
    checksum_of, parse_manifest, signing_digest, verify_artifact, CapabilitySet, InstalledPlugin,
    PluginManifest, TrustTier, TrustedPublishers, UnsignedPolicy, VerifyError,
};
use ed25519_dalek::{Signer, SigningKey};

/// A signed plugin manifest + its artifact bytes, signed by `signing`.
fn signed_plugin(publisher: &str, signing: &SigningKey) -> (PluginManifest, Vec<u8>) {
    let artifact = b"plugin artifact bytes".to_vec();
    let toml = format!(
        r#"
schema_version = 1
id = "acme.tool"
name = "Acme Tool"
version = "1.0.0"
kind = "native-process"
publisher = "{publisher}"
[runtime]
command = "acme-tool"
[capabilities]
network = ["api.acme.example:443"]
[security]
checksum = "{}"
signature = "unset"
"#,
        checksum_of(&artifact)
    );
    let mut manifest = parse_manifest(&toml).expect("manifest parses");
    let sig = signing.sign(&signing_digest(&manifest));
    manifest.security.signature = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    (manifest, artifact)
}

fn b64_key(signing: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes())
}

#[test]
fn an_unknown_publisher_is_refused_but_a_trusted_one_verifies() {
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let (manifest, artifact) = signed_plugin("acme", &signing);

    // 1) Empty store — the publisher is unknown, so it resolves to NO key and a
    //    signed plugin fails closed.
    let store = TrustedPublishers::new();
    assert!(store.key_for(&manifest.publisher).is_none());
    let err = verify_artifact(
        &manifest,
        &artifact,
        store.key_for(&manifest.publisher).map(|k| k.as_slice()),
        UnsignedPolicy::Deny,
    )
    .unwrap_err();
    assert!(
        matches!(err, VerifyError::InvalidPublisherKey(_)),
        "an unknown publisher must fail closed, got {err:?}"
    );

    // 2) Trust the publisher's real key — now verification succeeds and the plugin
    //    installs at the Trusted tier.
    let mut store = TrustedPublishers::new();
    store.add("acme", &b64_key(&signing)).unwrap();
    let verified = verify_artifact(
        &manifest,
        &artifact,
        store.key_for(&manifest.publisher).map(|k| k.as_slice()),
        UnsignedPolicy::Deny,
    )
    .expect("a trusted publisher's signature verifies");
    assert!(verified.signed);

    let granted = CapabilitySet::from_spec(&manifest.capabilities);
    let installed = InstalledPlugin::install_disabled(
        manifest.clone(),
        &artifact,
        store.key_for(&manifest.publisher).map(|k| k.as_slice()),
        UnsignedPolicy::Deny,
        granted,
    )
    .expect("install verifies against the trusted key");
    assert_eq!(installed.trust, TrustTier::Trusted);
    assert!(installed.signed);
}

#[test]
fn a_publisher_trusting_the_wrong_key_still_fails_closed() {
    // The store trusts `acme`, but with a DIFFERENT key than the one that signed
    // the plugin — verification must be refused (a signature mismatch), never
    // waved through just because the publisher id is present.
    let real = SigningKey::from_bytes(&[42u8; 32]);
    let (manifest, artifact) = signed_plugin("acme", &real);

    let mut store = TrustedPublishers::new();
    let wrong = SigningKey::from_bytes(&[7u8; 32]);
    store.add("acme", &b64_key(&wrong)).unwrap();

    let err = verify_artifact(
        &manifest,
        &artifact,
        store.key_for(&manifest.publisher).map(|k| k.as_slice()),
        UnsignedPolicy::Deny,
    )
    .unwrap_err();
    assert_eq!(err, VerifyError::SignatureMismatch);
}

#[test]
fn the_store_round_trips_through_its_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("codypendent")
        .join("trusted_publishers.toml");

    let signing = SigningKey::from_bytes(&[9u8; 32]);
    let mut store = TrustedPublishers::new();
    store.add("acme", &b64_key(&signing)).unwrap();
    store.save(&path).unwrap();

    // A fresh load resolves the same key (so a persisted trust decision survives).
    let reloaded = TrustedPublishers::load(&path).unwrap();
    assert_eq!(
        reloaded.key_for("acme"),
        Some(signing.verifying_key().as_bytes())
    );

    // Remove + persist, then a load trusts nobody.
    let mut store = reloaded;
    assert!(store.remove("acme"));
    store.save(&path).unwrap();
    assert!(TrustedPublishers::load(&path).unwrap().is_empty());
}
