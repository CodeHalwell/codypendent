#!/usr/bin/env bash
# Sign/notarize a CLI archive payload before its final tarball is produced.
set -euo pipefail
runtime="${1:?Usage: sign-apple-runtime.sh path/to/codypendent-apple-target}"
scripts="$(cd "$(dirname "$0")" && pwd)"
python3 "$scripts/apple-release.py" verify-runtime "$runtime"
source "$scripts/apple-signing-env.sh"
prepare_apple_signing
codesign "${sign_args[@]}" --entitlements "$scripts/../src-tauri/NodeEntitlements.plist" "$runtime/node-runtime/bin/node"
python3 "$scripts/apple-release.py" seal-runtime "$runtime/node-runtime"
for library in "$runtime/embedding-runtime/"*.dylib; do
  codesign "${sign_args[@]}" "$library"
done
codesign "${sign_args[@]}" "$runtime/embedding-runtime/llama-server"
python3 "$scripts/apple-release.py" seal-embeddings "$runtime/embedding-runtime"
for executable in codypendent codypendent-ui-worker-launcher codypendentd monty; do
  if [[ -f "$runtime/$executable" ]]; then codesign "${sign_args[@]}" "$runtime/$executable"; fi
done
ditto -c -k --keepParent "$runtime" "$staging/Codypendent-CLI.zip"
notarize_apple_payload "$staging/Codypendent-CLI.zip"
# Bare executables cannot carry stapled tickets. Check their online notarized
# requirement, not spctl's top-level .app policy. The desktop DMG supports offline
# stapled distribution; this archive's first Gatekeeper check may need a network.
python3 "$scripts/apple-release.py" verify-runtime "$runtime" --distribution
