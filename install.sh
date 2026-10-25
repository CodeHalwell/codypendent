#!/usr/bin/env bash
# Codypendent installer — downloads the latest release tarball for this machine
# and installs `codypendent` onto your PATH (self-sufficient — it runs the
# daemon from itself; the mandatory UI worker resource launcher, pinned Node
# runtime, and optional standalone `codypendentd` are installed with it).
#
# One-liner (uses your existing `gh` auth, so it works for a private repo):
#
#   gh api repos/CodeHalwell/codypendent/contents/install.sh \
#     -H 'Accept: application/vnd.github.raw' | bash
#
# Install a specific release instead of the latest:
#
#   … | bash -s -- v0.1.0-build.17
#
# Override the install dir (default /usr/local/bin):
#
#   … | CODYPENDENT_BIN="$HOME/.local/bin" bash
set -euo pipefail

REPO="CodeHalwell/codypendent"
BINDIR="${CODYPENDENT_BIN:-/usr/local/bin}"
LIBDIR="${CODYPENDENT_LIB:-$(dirname "$BINDIR")/lib/codypendent}"

command -v gh  >/dev/null || { echo "error: GitHub CLI (gh) is required — https://cli.github.com" >&2; exit 1; }
command -v tar >/dev/null || { echo "error: tar is required" >&2; exit 1; }

# 1. Detect this machine's release target.
os="$(uname -s)"; arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)   target="aarch64-apple-darwin" ;;
  Darwin/x86_64)  target="x86_64-apple-darwin" ;;
  Linux/x86_64)   target="x86_64-unknown-linux-gnu" ;;
  *) echo "error: no prebuilt binary for $os/$arch (Windows is unsupported)." >&2; exit 1 ;;
esac

# 2. Resolve the release tag: first arg wins; otherwise the newest release
#    (rolling builds are prereleases, so we ask for the latest of ALL releases).
tag="${1:-}"
if [ -z "$tag" ]; then
  tag="$(gh release list -R "$REPO" -L 1 --json tagName --jq '.[0].tagName' 2>/dev/null || true)"
fi
[ -n "$tag" ] || { echo "error: no releases found on $REPO" >&2; exit 1; }

asset="codypendent-$target.tar.gz"
echo "codypendent: installing $tag ($target) -> $BINDIR"

# 3. Download + extract into a temp dir that is always cleaned up.
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
gh release download "$tag" -R "$REPO" -p "$asset" -p SHA256SUMS -D "$tmp" --clobber
# Verify before extraction or any privileged install. Old releases without a
# published manifest fail closed; a digest of the download alone proves nothing.
expected="$(awk -v name="$asset" '$2 == name { digest=$1; count++ } END { if (count != 1) exit 1; print digest }' "$tmp/SHA256SUMS")" || {
  echo "error: release manifest has no unique checksum for $asset" >&2; exit 1;
}
[[ "$expected" =~ ^[[:xdigit:]]{64}$ ]] || { echo "error: malformed release checksum" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
  actual="$(sha256sum "$tmp/$asset" | cut -d ' ' -f 1)"
else
  command -v shasum >/dev/null || { echo "error: sha256sum or shasum is required" >&2; exit 1; }
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d ' ' -f 1)"
fi
[ "$actual" = "$expected" ] || { echo "error: release checksum mismatch; nothing installed" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/codypendent-$target"
[ -x "$src/codypendent" ] || { echo "error: codypendent binary missing in $asset" >&2; exit 1; }
[ -x "$src/codypendent-ui-worker-launcher" ] || { echo "error: UI worker launcher missing in $asset" >&2; exit 1; }
[ -x "$src/node-runtime/bin/node" ] || { echo "error: pinned Remote UI Node runtime missing in $asset" >&2; exit 1; }
[ -x "$src/embedding-runtime/llama-server" ] && [ -f "$src/embedding-runtime/model.gguf" ] && [ -f "$src/embedding-runtime/embedding-runtime.json" ] || {
  echo "error: pinned local embedding runtime missing in $asset" >&2; exit 1;
}

# macOS releases are signed/notarized in CI; do not clear Gatekeeper quarantine.

# 5. Install `codypendent` (self-sufficient — it runs the daemon from itself
#    via `codypendent __daemon`). Also install the OPTIONAL standalone
#    `codypendentd` if the tarball carried it. Use sudo only if the target dir
#    is not writable.
bins=("$src/codypendent" "$src/codypendent-ui-worker-launcher")
[ -x "$src/codypendentd" ] && bins+=("$src/codypendentd")
mkdir -p "$BINDIR" 2>/dev/null || true
mkdir -p "$LIBDIR" 2>/dev/null || true
privilege=(command)
if [ ! -w "$BINDIR" ] || [ ! -w "$LIBDIR" ]; then
  echo "codypendent: $BINDIR is not writable — using sudo"
  privilege=(sudo)
fi
"${privilege[@]}" install -m 0755 "${bins[@]}" "$BINDIR"/
"${privilege[@]}" mkdir -p "$LIBDIR"
for runtime in node-runtime embedding-runtime; do
  runtime_stage="$LIBDIR/$runtime.new.$$"
  runtime_old="$LIBDIR/$runtime.old.$$"
  "${privilege[@]}" cp -R "$src/$runtime" "$runtime_stage"
  [ ! -e "$LIBDIR/$runtime" ] || "${privilege[@]}" mv "$LIBDIR/$runtime" "$runtime_old"
  "${privilege[@]}" mv "$runtime_stage" "$LIBDIR/$runtime"
  [ ! -e "$runtime_old" ] || "${privilege[@]}" rm -rf "$runtime_old"
done

if [ -x "$src/codypendentd" ]; then
  echo "codypendent: installed codypendent, codypendent-ui-worker-launcher, pinned UI and embedding runtimes, and codypendentd"
else
  echo "codypendent: installed codypendent, codypendent-ui-worker-launcher, and pinned UI and embedding runtimes"
fi
case ":$PATH:" in
  *":$BINDIR:"*) echo "codypendent: run  codypendent" ;;
  *) echo "codypendent: add $BINDIR to your PATH, then run  codypendent" ;;
esac
