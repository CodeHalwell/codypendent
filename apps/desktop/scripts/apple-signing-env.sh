#!/usr/bin/env bash
# Shared explicit Developer ID setup. Sourcing this file has no side effects.
prepare_apple_signing() {
  : "${APPLE_SIGNING_IDENTITY:?Set the Developer ID Application identity}"
  [[ "$APPLE_SIGNING_IDENTITY" == "Developer ID Application:"* ]] || {
    echo "A Developer ID Application identity is required for public Apple distribution." >&2
    return 1
  }
  if [[ -n "${APPLE_NOTARY_PROFILE:-}" ]]; then
    notary_args=(--keychain-profile "$APPLE_NOTARY_PROFILE")
  else
    : "${APPLE_ID:?Set Apple ID or APPLE_NOTARY_PROFILE}"
    : "${APPLE_PASSWORD:?Set the app-specific Apple password}"
    : "${APPLE_TEAM_ID:?Set Apple Team ID}"
    notary_args=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
  fi
  staging="$(mktemp -d "${TMPDIR:-/tmp}/codypendent-notary.XXXXXX")"
  temporary_keychain=""
  trap cleanup_apple_signing EXIT
  if [[ -n "${APPLE_CERTIFICATE:-}" ]]; then
    : "${APPLE_CERTIFICATE_PASSWORD:?Set the exported certificate password}"
    temporary_keychain="$staging/signing.keychain-db"
    keychain_password="$(openssl rand -hex 24)"
    printf '%s' "$APPLE_CERTIFICATE" | base64 --decode > "$staging/certificate.p12"
    security create-keychain -p "$keychain_password" "$temporary_keychain"
    security set-keychain-settings -lut 3600 "$temporary_keychain"
    security unlock-keychain -p "$keychain_password" "$temporary_keychain"
    security import "$staging/certificate.p12" -k "$temporary_keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$temporary_keychain" >/dev/null
    APPLE_KEYCHAIN="$temporary_keychain"
  fi
  sign_args=(--force --timestamp --options runtime --sign "$APPLE_SIGNING_IDENTITY")
  if [[ -n "${APPLE_KEYCHAIN:-}" ]]; then sign_args+=(--keychain "$APPLE_KEYCHAIN"); fi
}

cleanup_apple_signing() {
  if [[ -n "$temporary_keychain" ]]; then security delete-keychain "$temporary_keychain" >/dev/null 2>&1 || true; fi
  rm -rf "$staging"
}

notarize_apple_payload() {
  if ! xcrun notarytool submit "$1" "${notary_args[@]}" --wait --output-format json > "$staging/notary-result.json"; then
    cat "$staging/notary-result.json" >&2
    return 1
  fi
  python3 - "$staging/notary-result.json" <<'PY'
import json, sys
result = json.load(open(sys.argv[1]))
if result.get("status") != "Accepted":
    raise SystemExit("Apple notarization was not accepted: " + str(result.get("status")))
print("Apple notarization accepted: " + result["id"])
PY
}
