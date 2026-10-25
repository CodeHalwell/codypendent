#!/usr/bin/env python3
"""Stage a self-contained Apple app and verify its actual distribution state.

No signing identity is opened and nothing is uploaded by this tool.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SEAL = ".codypendent-runtime-seal.json"
EMBEDDING_MANIFEST = "embedding-runtime.json"
EMBEDDING_RELEASE = json.loads(Path(__file__).with_name(EMBEDDING_MANIFEST).read_text())


def run(*args: str | Path) -> str:
    result = subprocess.run([str(arg) for arg in args], check=False, text=True, capture_output=True, timeout=60)
    if result.returncode:
        raise ValueError(f"{args[0]} failed: {(result.stderr or result.stdout).strip()}")
    return result.stdout + result.stderr


def runtime_entries(root: Path) -> list[dict[str, str]]:
    entries = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if relative == SEAL:
            continue
        if path.is_symlink():
            if not path.resolve().is_relative_to(root.resolve()):
                raise ValueError(f"Node runtime symlink leaves its root: {relative}")
            entries.append({"kind": "link", "path": relative, "digest": os.readlink(path)})
        elif path.is_file():
            entries.append({"kind": "file", "path": relative, "digest": file_checksum(path)})
    return entries


def file_checksum(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def embedding_entries(root: Path) -> list[dict]:
    if any(path.is_symlink() for path in root.rglob("*")):
        raise ValueError("Embedding runtime contains an unexpected symbolic link")
    return [{"path": entry["path"], "size": (root / entry["path"]).stat().st_size, "sha256": entry["digest"]}
            for entry in runtime_entries(root) if entry["path"] != EMBEDDING_MANIFEST and entry["kind"] == "file"]


def check_embedding_model(root: Path, manifest: dict) -> None:
    model = EMBEDDING_RELEASE["model"]
    if manifest.get("modelSha256") != model["sha256"] or file_checksum(root / "model.gguf") != model["sha256"]:
        raise ValueError("Bundled embedding model does not match its pinned checksum")
    if manifest.get("model") != model["name"] or manifest.get("dimensions") != model["dimensions"] or manifest.get("promptFormat") != model["promptFormat"]:
        raise ValueError("Bundled embedding model metadata differs from the pinned release")


def reseal_embeddings(root: Path) -> None:
    manifest = json.loads((root / EMBEDDING_MANIFEST).read_text())
    check_embedding_model(root, manifest)
    manifest["serverSha256"] = file_checksum(root / "llama-server")
    manifest["files"] = embedding_entries(root)
    (root / EMBEDDING_MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n")


def verify_embeddings(root: Path) -> list[Path]:
    manifest = json.loads((root / EMBEDDING_MANIFEST).read_text())
    check_embedding_model(root, manifest)
    if manifest.get("files") != embedding_entries(root) or manifest.get("serverSha256") != file_checksum(root / "llama-server"):
        raise ValueError("Bundled embedding runtime does not match its final byte seal")
    executables = [root / "llama-server", *sorted(root.glob("*.dylib"))]
    for path in executables:
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"Embedding runtime is missing executable: {path}")
    return executables


def reseal(root: Path) -> None:
    if not (root / "bin/node").is_file():
        raise ValueError("Node runtime is missing bin/node")
    (root / SEAL).write_text(json.dumps(runtime_entries(root), separators=(",", ":")) + "\n")


def metadata(app: Path) -> tuple[dict, Path]:
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != "com.codypendent.desktop":
        raise ValueError("Expected the com.codypendent.desktop bundle")
    return info, app / "Contents/MacOS"


def distribution_signature(detail: str) -> bool:
    flags = re.search(r"^CodeDirectory .*\bflags=(0x[0-9a-fA-F]+)\b", detail, re.MULTILINE)
    return bool(
        flags
        and int(flags.group(1), 16) & 0x10000  # CS_RUNTIME
        and re.search(r"^Authority=Developer ID Application:", detail, re.MULTILINE)
        and re.search(r"^Timestamp=.+", detail, re.MULTILINE)
    )


def stage(app: Path, source: Path) -> None:
    info, binaries = metadata(app)
    names = ("codypendent", "codypendent-ui-worker-launcher", "monty")
    embedding_runtime = source / "embedding-runtime"
    required = [source / name for name in names] + [source / "node-runtime/bin/node", *verify_embeddings(embedding_runtime)]
    for path in required:
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"Required executable is absent: {path}")
    expected = info["CFBundleShortVersionString"]
    version = run(source / "codypendent", "--version").strip().split()
    if len(version) < 2 or version[1] != expected:
        raise ValueError(f"CLI and desktop versions differ: {version!r}, app {expected}")
    architecture = set(run("lipo", "-archs", binaries / info["CFBundleExecutable"]).split())
    for path in required:
        if not architecture.issubset(set(run("lipo", "-archs", path).split())):
            raise ValueError(f"Executable does not support app architecture: {path}")
    source_runtime = source / "node-runtime"
    if json.loads((source_runtime / SEAL).read_text()) != runtime_entries(source_runtime):
        raise ValueError("Source Node runtime does not match its release seal")
    # Refuse to silently invalidate an already signed release. --stage belongs
    # immediately after the unsigned Tauri build, before any Developer ID step.
    signature = subprocess.run(["codesign", "-d", "--verbose=4", str(app)], check=False, capture_output=True, text=True)
    if "Authority=Developer ID" in signature.stderr:
        raise ValueError("Refusing to change a Developer ID signed bundle; build an unsigned app first")
    if (binaries / "node-runtime").exists():
        raise ValueError("App already contains a Node runtime; rebuild the unsigned bundle before staging")
    for name in names:
        shutil.copy2(source / name, binaries / name)
    shutil.copytree(source_runtime, binaries / "node-runtime", symlinks=True)
    embedding_destination = app / "Contents/Resources/embedding-runtime"
    if embedding_destination.exists():
        shutil.rmtree(embedding_destination)
    shutil.copytree(embedding_runtime, embedding_destination)
    print(f"Staged matching CLI, worker launcher, Monty, Node and local embedding runtimes in {app}")


def verify(app: Path, distribution: bool) -> None:
    info, binaries = metadata(app)
    runtime = binaries / "node-runtime"
    executables = [binaries / info["CFBundleExecutable"], binaries / "codypendent",
                   binaries / "codypendent-ui-worker-launcher", binaries / "monty", runtime / "bin/node",
                   *verify_embeddings(app / "Contents/Resources/embedding-runtime")]
    for path in executables:
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"Bundle is not self-contained; missing executable: {path}")
    if json.loads((runtime / SEAL).read_text()) != runtime_entries(runtime):
        raise ValueError("Bundled Node runtime seal does not match the final bytes")
    if distribution:
        for path in [*executables, app]:
            run("codesign", "--verify", "--strict", "--verbose=2", path)
            detail = run("codesign", "-d", "--verbose=4", path)
            if not distribution_signature(detail):
                raise ValueError(f"Developer ID, hardened runtime and a secure timestamp are required: {path}")
        run("xcrun", "stapler", "validate", app)
        run("spctl", "--assess", "--type", "execute", "--verbose=4", app)
        print("Apple distribution: Developer ID signatures, hardened runtime, stapled ticket and Gatekeeper accepted")
    else:
        print("Self-contained bundle verified; signing and notarization were not assessed")


def verify_runtime(root: Path, distribution: bool) -> None:
    executables = [root / "codypendent", root / "codypendent-ui-worker-launcher", root / "monty", root / "node-runtime/bin/node",
                   *verify_embeddings(root / "embedding-runtime")]
    if (root / "codypendentd").exists():
        executables.append(root / "codypendentd")
    for path in executables:
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"Runtime is missing executable: {path}")
    architecture = set(run("lipo", "-archs", executables[0]).split())
    verify_architectures(architecture, executables[1:])
    runtime = root / "node-runtime"
    if json.loads((runtime / SEAL).read_text()) != runtime_entries(runtime):
        raise ValueError("Node runtime seal does not match the CLI archive payload")
    if distribution:
        for path in executables:
            run("codesign", "--verify", "--strict", "--verbose=2", path)
            if not distribution_signature(run("codesign", "-d", "--verbose=4", path)):
                raise ValueError(f"CLI distribution needs Developer ID, hardened runtime and secure timestamp: {path}")
            run("codesign", "--verify", "--strict", "--check-notarization", "-R=notarized", path)
        print("CLI distribution: every executable has Developer ID, hardened runtime and verified notarization")
    else:
        print("Matching CLI runtime verified; signing and notarization were not assessed")


def verify_architectures(architecture: set[str], executables: list[Path]) -> None:
    for path in executables:
        if not architecture.issubset(set(run("lipo", "-archs", path).split())):
            raise ValueError(f"Runtime executable architecture differs: {path}")



def verify_dmg(dmg: Path, distribution: bool) -> None:
    run("hdiutil", "verify", dmg)
    if distribution:
        run("codesign", "--verify", "--strict", dmg)
        detail = run("codesign", "-d", "--verbose=4", dmg)
        if not re.search(r"^Authority=Developer ID Application:", detail, re.MULTILINE) or not re.search(r"^Timestamp=.+", detail, re.MULTILINE):
            raise ValueError("DMG needs a Developer ID signature and secure timestamp")
        run("xcrun", "stapler", "validate", dmg)
        run("spctl", "--assess", "--type", "open", "--context", "context:primary-signature", dmg)
    with tempfile.TemporaryDirectory(prefix="codypendent-dmg-check-") as temporary:
        mount = Path(temporary) / "image"
        mount.mkdir()
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, dmg)
        try:
            shortcut = mount / "Applications"
            if not shortcut.is_symlink() or os.readlink(shortcut) != "/Applications":
                raise ValueError("DMG is missing the Applications install shortcut")
            verify(mount / "Codypendent.app", distribution)
        finally:
            run("hdiutil", "detach", mount)
    print("DMG mounted read-only and its complete app verified")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("stage")
    prepare.add_argument("app", type=Path)
    prepare.add_argument("runtime_directory", type=Path, help="Extracted matching codypendent Apple release archive")
    check = commands.add_parser("verify")
    check.add_argument("app", type=Path)
    check.add_argument("--distribution", action="store_true", help="Require Developer ID, hardened runtime, notarization and Gatekeeper")
    disk = commands.add_parser("verify-dmg")
    disk.add_argument("dmg", type=Path)
    disk.add_argument("--distribution", action="store_true")
    runtime = commands.add_parser("verify-runtime")
    runtime.add_argument("runtime_directory", type=Path)
    runtime.add_argument("--distribution", action="store_true")
    seal = commands.add_parser("seal-runtime")
    seal.add_argument("runtime_directory", type=Path)
    embeddings = commands.add_parser("seal-embeddings")
    embeddings.add_argument("runtime_directory", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "stage":
            stage(args.app.resolve(), args.runtime_directory.resolve())
        elif args.command == "verify-dmg":
            verify_dmg(args.dmg.resolve(), args.distribution)
        elif args.command == "seal-runtime":
            reseal(args.runtime_directory.resolve())
        elif args.command == "seal-embeddings":
            reseal_embeddings(args.runtime_directory.resolve())
        elif args.command == "verify-runtime":
            verify_runtime(args.runtime_directory.resolve(), args.distribution)
        else:
            verify(args.app.resolve(), args.distribution)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Apple release check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
