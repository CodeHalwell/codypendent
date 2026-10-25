#!/usr/bin/env python3
"""Prepare the pinned standalone Monty worker for developer and desktop builds."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import tempfile
import urllib.request
import zipfile

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[2]
RELEASE = json.loads((HERE / "monty-runtime.json").read_text())
TARGETS = {
    "aarch64-apple-darwin": "darwin-arm64",
    "x86_64-apple-darwin": "darwin-x64",
    "aarch64-unknown-linux-gnu": "linux-arm64",
    "x86_64-unknown-linux-gnu": "linux-x64",
    "x86_64-pc-windows-msvc": "win32-x64",
}


def prepare(target: str, output: Path) -> Path:
    key = TARGETS.get(target)
    if key is None:
        raise ValueError(f"No pinned Monty worker is available for {target}")
    asset = RELEASE["assets"][key]
    cache = REPOSITORY / "target/monty-runtime-cache"
    cache.mkdir(parents=True, exist_ok=True)
    wheel = cache / asset["filename"]
    if not wheel.exists() or hashlib.sha256(wheel.read_bytes()).hexdigest() != asset["sha256"]:
        request = urllib.request.Request(asset["url"], headers={"User-Agent": "Codypendent-build"})
        with urllib.request.urlopen(request, timeout=60) as response:
            data = response.read(asset["size"] + 1)
        if len(data) != asset["size"] or hashlib.sha256(data).hexdigest() != asset["sha256"]:
            raise ValueError("The Monty release download did not match its pinned checksum")
        temporary = wheel.with_suffix(".download")
        temporary.write_bytes(data)
        temporary.replace(wheel)
    suffix = ".exe" if key.startswith("win32") else ""
    member = f"pydantic_monty_runtime-{RELEASE['version']}.data/scripts/monty{suffix}"
    with zipfile.ZipFile(wheel) as archive:
        binary = archive.read(member)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"monty{suffix}"
    fd, temporary = tempfile.mkstemp(prefix=".monty-", dir=output)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(binary)
        os.chmod(temporary, 0o755)
        os.replace(temporary, destination)
    finally:
        Path(temporary).unlink(missing_ok=True)
    sidecar = REPOSITORY / "apps/desktop/src-tauri/binaries" / f"monty-{target}{suffix}"
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(destination, sidecar)
    (output / "monty-runtime.json").write_text(json.dumps({
        "version": RELEASE["version"], "target": target,
        "wheelSha256": asset["sha256"], "binarySha256": hashlib.sha256(binary).hexdigest(),
    }, indent=2) + "\n")
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--output", type=Path, default=REPOSITORY / "target/debug")
    args = parser.parse_args()
    target = args.target or os.environ.get("TAURI_ENV_TARGET_TRIPLE") or os.environ.get("CARGO_BUILD_TARGET")
    if target is None:
        machine = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "x64"
        system = {"Darwin": "darwin", "Linux": "linux", "Windows": "win32"}.get(platform.system())
        target = next((target for target, key in TARGETS.items() if key == f"{system}-{machine}"), None)
        if target is None:
            parser.error("This platform does not have a pinned Monty worker")
    binary = prepare(target, args.output.resolve())
    print(f"Prepared Monty {RELEASE['version']} for {target}: {binary}")


if __name__ == "__main__":
    main()
