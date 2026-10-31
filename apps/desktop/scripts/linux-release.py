#!/usr/bin/env python3
"""Stage and verify the complete Linux desktop runtime; never publish or sign."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
TAURI_CONFIG = HERE.parent / "src-tauri/tauri.conf.json"
PAYLOAD_SEAL = ".codypendent-desktop-runtime.json"
TARGET = "x86_64-unknown-linux-gnu"
spec = importlib.util.spec_from_file_location("codypendent_apple_release", HERE / "apple-release.py")
apple = importlib.util.module_from_spec(spec)
spec.loader.exec_module(apple)


def configuration(path: Path) -> dict:
    config = json.loads(path.read_text())
    product = config.get("productName")
    if not isinstance(product, str) or not product or Path(product).name != product or product in (".", ".."):
        raise ValueError("The desktop product name must be one resource directory component")
    if not isinstance(config.get("version"), str):
        raise TypeError("The desktop version is missing")
    return config


def elf_x64(path: Path) -> None:
    if not path.is_file() or path.is_symlink() or not os.access(path, os.X_OK):
        raise ValueError(f"Required Linux executable is absent: {path}")
    with path.open("rb") as handle:
        header = handle.read(20)
    if header[:6] != b"\x7fELF\x02\x01" or len(header) < 20 or int.from_bytes(header[18:20], "little") != 62:
        raise ValueError(f"Expected an x86-64 Linux ELF executable: {path}")


def entries(root: Path) -> list[dict[str, str]]:
    # Share the release verifier's exact hashing and internal-link rules.
    return [item for item in apple.runtime_entries(root) if item["path"] != PAYLOAD_SEAL]


def verify_payload(root: Path, config: dict, *, sealed: bool = False) -> None:
    for path in root.rglob("*"):
        if path.is_symlink() and not path.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"Runtime symlink leaves its root: {path.relative_to(root)}")
    required = [root / name for name in ("codypendent", "codypendent-ui-worker-launcher", "monty")]
    required.extend([root / "node-runtime/bin/node", *apple.verify_embeddings(root / "embedding-runtime")])
    for path in required:
        elf_x64(path)
    runtime = root / "node-runtime"
    if json.loads((runtime / apple.SEAL).read_text()) != apple.runtime_entries(runtime):
        raise ValueError("Node runtime does not match its release seal")
    version = apple.run(root / "codypendent", "--version").strip().split()
    if len(version) < 2 or version[1] != config["version"]:
        raise ValueError(f"CLI and desktop versions differ: {version!r}, app {config['version']}")
    if sealed:
        expected = {
            "schemaVersion": 1, "target": TARGET, "version": config["version"],
            "productName": config["productName"], "entries": entries(root),
        }
        if json.loads((root / PAYLOAD_SEAL).read_text()) != expected:
            raise ValueError("Desktop runtime does not match its staged byte seal")


def stage(source: Path, destination: Path, overlay: Path, config: dict) -> None:
    # Validate the actual release payload before changing any build inputs.
    verify_payload(source, config)
    if destination.exists() or destination.is_symlink():
        raise ValueError("Desktop runtime destination already exists; rebuild the generated staging directory")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".linux-runtime-", dir=destination.parent) as temporary:
        staged = Path(temporary) / "runtime"
        shutil.copytree(source, staged, symlinks=True)
        (staged / PAYLOAD_SEAL).write_text(json.dumps({
            "schemaVersion": 1, "target": TARGET, "version": config["version"],
            "productName": config["productName"], "entries": entries(staged),
        }, indent=2) + "\n")
        verify_payload(staged, config, sealed=True)
        staged.rename(destination)
    overlay.parent.mkdir(parents=True, exist_ok=True)
    # Generic Tauri resources copy one file at a time and dereference links.
    # Linux's directory mapping preserves the complete sealed Node tree,
    # outside linuxdeploy's recursive ELF rewriting under usr/lib.
    # Neither distribution bundling nor the pinned resources are disabled.
    files = {f"usr/share/{config['productName']}/runtime": str(destination)}
    overlay.write_text(json.dumps({"bundle": {"linux": {
        "appimage": {"files": files}, "deb": {"files": files},
    }}}, indent=2) + "\n")
    print(f"Staged matching Linux CLI, worker, Monty, Node and embedding runtimes in {destination}")


def verify_appdir(appdir: Path, config: dict) -> None:
    elf_x64(appdir / "usr/bin/codypendent-desktop")
    verify_payload(appdir / "usr/share" / config["productName"] / "runtime", config, sealed=True)
    print("Self-contained Linux AppDir verified; distribution signing was not assessed")


def verify_image(image: Path, config: dict) -> None:
    elf_x64(image)
    # Extraction needs neither FUSE nor a display, and never starts the GUI.
    with tempfile.TemporaryDirectory(prefix="codypendent-appimage-check-") as temporary:
        result = subprocess.run([str(image), "--appimage-extract"], check=False, cwd=temporary,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, timeout=120)
        if result.returncode:
            raise ValueError(f"AppImage extraction failed: {result.stderr[-2000:]}")
        verify_appdir(Path(temporary) / "squashfs-root", config)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tauri-config", type=Path, default=TAURI_CONFIG)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("stage")
    prepare.add_argument("runtime_directory", type=Path, help="Extracted matching Linux CLI release artifact")
    prepare.add_argument("--output", type=Path, default=HERE.parent / "src-tauri/resources/runtime")
    prepare.add_argument("--config", type=Path, required=True, help="Generated Tauri resource overlay")
    directory = commands.add_parser("verify-appdir")
    directory.add_argument("appdir", type=Path)
    image = commands.add_parser("verify-image")
    image.add_argument("image", type=Path)
    args = parser.parse_args()
    try:
        config = configuration(args.tauri_config.resolve())
        if args.command == "stage":
            stage(args.runtime_directory.resolve(), args.output.resolve(), args.config.resolve(), config)
        elif args.command == "verify-appdir":
            verify_appdir(args.appdir.resolve(), config)
        else:
            verify_image(args.image.resolve(), config)
    except (OSError, TypeError, ValueError, subprocess.SubprocessError) as error:
        print(f"Linux desktop release check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
