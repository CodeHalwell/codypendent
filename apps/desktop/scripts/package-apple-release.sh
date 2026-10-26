#!/usr/bin/env bash
# Create a Finder-installable image from an already staged app. Distribution
# signing and notarization are performed by sign-apple-release.sh afterwards.
set -euo pipefail
app="${1:?Usage: package-apple-release.sh Codypendent.app output.dmg}"
output="${2:?Specify the output DMG}"
scripts="$(cd "$(dirname "$0")" && pwd)"
python3 "$scripts/apple-release.py" verify "$app"
[[ ! -e "$output" ]] || { echo "Refusing to replace existing image: $output" >&2; exit 1; }
staging="$(mktemp -d "${TMPDIR:-/tmp}/codypendent-dmg.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
ditto "$app" "$staging/Codypendent.app"
ln -s /Applications "$staging/Applications"
cat > "$staging/Install.txt" <<'TXT'
Drag Codypendent.app into Applications, then open it from Applications.

To update, first let active coding runs finish. Quit Codypendent and its other
connected clients, then stop the idle local daemon from Terminal:

  /Applications/Codypendent.app/Contents/MacOS/codypendent daemon stop

If you installed in your home Applications folder, use this command instead:

  "$HOME/Applications/Codypendent.app/Contents/MacOS/codypendent" daemon stop

Then replace the old app. Your repositories, settings and session history live
outside the app and are retained. The stop command is manual; do not run it
while a coding run is active.

The desktop app includes its CLI, worker launcher and Node runtime. No separate
Node or Rust installation is needed. Git is required to work with repositories.

Unsigned preview builds are for development. A public Apple release requires
Developer ID signing and notarization; keep macOS Gatekeeper enabled.
TXT
mkdir -p "$(dirname "$output")"
hdiutil create -volname Codypendent -srcfolder "$staging" -format UDZO -fs HFS+ "$output"
python3 "$scripts/apple-release.py" verify-dmg "$output"
