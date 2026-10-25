#!/usr/bin/env bash
# Run only for an explicitly authorised Developer ID release. This uses an
# existing identity, or imports the CI certificate into a temporary keychain.
# Publishing is separate. Never invoke this script for a local preview build.
set -euo pipefail
app="${1:?Usage: sign-apple-release.sh path/to/Codypendent.app [output.dmg]}"
scripts="$(cd "$(dirname "$0")" && pwd)"
python3 "$scripts/apple-release.py" verify "$app"
source "$scripts/apple-signing-env.sh"
prepare_apple_signing
bin="$app/Contents/MacOS"
codesign "${sign_args[@]}" --entitlements "$scripts/../src-tauri/NodeEntitlements.plist" "$bin/node-runtime/bin/node"
# Code signing changes Node's bytes, so seal the final signed runtime before
# sealing the enclosing app. No file in the app changes after its signature.
python3 "$scripts/apple-release.py" seal-runtime "$bin/node-runtime"
embeddings="$app/Contents/Resources/embedding-runtime"
for library in "$embeddings/"*.dylib; do
  codesign "${sign_args[@]}" "$library"
done
codesign "${sign_args[@]}" "$embeddings/llama-server"
python3 "$scripts/apple-release.py" seal-embeddings "$embeddings"
codesign "${sign_args[@]}" "$bin/codypendent-ui-worker-launcher"
codesign "${sign_args[@]}" "$bin/monty"
codesign "${sign_args[@]}" "$bin/codypendent"
codesign "${sign_args[@]}" "$app"
ditto -c -k --keepParent "$app" "$staging/Codypendent.zip"
notarize_apple_payload "$staging/Codypendent.zip"
xcrun stapler staple "$app"
python3 "$scripts/apple-release.py" verify "$app" --distribution

# The app ticket is already stapled before it enters its final distribution
# container. Sign and notarize the DMG itself so offline Finder installation
# can validate both container and contained app.
if [[ -n "${2:-}" ]]; then
  dmg="$2"
  bash "$scripts/package-apple-release.sh" "$app" "$dmg"
  dmg_sign_args=(--force --timestamp --sign "$APPLE_SIGNING_IDENTITY")
  if [[ -n "${APPLE_KEYCHAIN:-}" ]]; then dmg_sign_args+=(--keychain "$APPLE_KEYCHAIN"); fi
  codesign "${dmg_sign_args[@]}" "$dmg"
  notarize_apple_payload "$dmg"
  xcrun stapler staple "$dmg"
  python3 "$scripts/apple-release.py" verify-dmg "$dmg" --distribution
fi
