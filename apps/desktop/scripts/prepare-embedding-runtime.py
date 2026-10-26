#!/usr/bin/env python3
"""Prepare checksum-pinned local embeddings for developer and desktop builds."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[2]
RELEASE = json.loads((HERE / "embedding-runtime.json").read_text())
TAURI_RUNTIME = REPOSITORY / "apps/desktop/src-tauri/resources/embedding-runtime"
MANIFEST = "embedding-runtime.json"


def checksum(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def matches(path: Path, asset: dict) -> bool:
    return path.is_file() and path.stat().st_size == asset["size"] and checksum(path) == asset["sha256"]


def download(asset: dict, cache: Path) -> Path:
    destination = cache / asset["filename"]
    if matches(destination, asset):
        return destination
    fd, temporary = tempfile.mkstemp(prefix=".download-", dir=cache)
    try:
        request = urllib.request.Request(asset["url"], headers={"User-Agent": "Codypendent-build"})
        with os.fdopen(fd, "wb") as handle, urllib.request.urlopen(request, timeout=60) as response:
            remaining = asset["size"] + 1
            while remaining:
                chunk = response.read(min(1024 * 1024, remaining))
                if not chunk:
                    break
                handle.write(chunk)
                remaining -= len(chunk)
        if not matches(Path(temporary), asset):
            raise ValueError(f"Embedding asset failed its pinned checksum: {asset['filename']}")
        os.replace(temporary, destination)
    finally:
        Path(temporary).unlink(missing_ok=True)
    return destination


def runtime_file(name: str) -> bool:
    return name in ("llama-server", "llama-server.exe") or bool(re.fullmatch(r"[^/]+(?:\.dylib|\.so(?:\.\d+)*|\.dll)", name))


def copy_member(name: str, source, output: Path) -> None:
    if not runtime_file(name):
        return
    destination = output / name
    if destination.exists():
        raise ValueError(f"Runtime archive contains duplicate filename: {name}")
    with destination.open("wb") as handle:
        shutil.copyfileobj(source, handle)
    destination.chmod(0o755)


def unpack(archive_path: Path, output: Path) -> None:
    if archive_path.suffix == ".zip":
        with zipfile.ZipFile(archive_path) as archive:
            for member in archive.infolist():
                with archive.open(member) as source:
                    copy_member(PurePosixPath(member.filename).name, source, output)
        return
    with tarfile.open(archive_path) as archive:
        for member in archive.getmembers():
            if member.isdir() or not runtime_file(PurePosixPath(member.name).name):
                continue
            if member.issym() and PurePosixPath(member.linkname).name != member.linkname:
                raise ValueError(f"Runtime library link leaves its directory: {member.name}")
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(f"Runtime archive member is not a file: {member.name}")
            with source:
                copy_member(PurePosixPath(member.name).name, source, output)


def manifest(root: Path, target: str, asset: dict) -> dict:
    model = RELEASE["model"]
    server = "llama-server.exe" if "windows" in target else "llama-server"
    if not (root / server).is_file():
        raise ValueError("Pinned runtime archive is missing llama-server")
    return {
        "schemaVersion": 1, "model": model["name"], "modelSha256": model["sha256"],
        "modelRevision": model["revision"], "dimensions": model["dimensions"],
        "promptFormat": model["promptFormat"], "quantization": model["quantization"],
        "serverVersion": RELEASE["runtime"]["version"], "runtimeRevision": RELEASE["runtime"]["revision"],
        "runtimeArchiveSha256": asset["sha256"], "serverSha256": checksum(root / server), "target": target,
        "files": [{"path": path.relative_to(root).as_posix(), "size": path.stat().st_size, "sha256": checksum(path)}
                  for path in sorted(root.rglob("*")) if path.is_file() and path.name != MANIFEST],
    }


def replace_directory(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".embedding-stage-", dir=destination.parent) as temporary:
        staged, previous = Path(temporary) / "next", Path(temporary) / "previous"
        shutil.copytree(source, staged)
        if destination.exists():
            destination.rename(previous)
        try:
            staged.rename(destination)
        except OSError:
            if previous.exists():
                previous.rename(destination)
            raise


def prepare(target: str, output: Path, tauri: bool = True) -> Path:
    asset = RELEASE["runtime"]["assets"].get(target)
    if asset is None:
        raise ValueError(f"No pinned embedding runtime is available for {target}")
    cache = REPOSITORY / "target/embedding-runtime-cache"
    cache.mkdir(parents=True, exist_ok=True)
    runtime, model = download(asset, cache), download(RELEASE["model"], cache)
    with tempfile.TemporaryDirectory(prefix=".embedding-", dir=cache) as temporary:
        root = Path(temporary)
        unpack(runtime, root)
        shutil.copy2(model, root / "model.gguf")
        licenses = root / "licenses"
        licenses.mkdir()
        for item in RELEASE["licenses"]:
            shutil.copy2(download(item, cache), licenses / item["filename"])
        (root / MANIFEST).write_text(json.dumps(manifest(root, target, asset), indent=2) + "\n")
        destination = output / "embedding-runtime"
        replace_directory(root, destination)
        if tauri:
            replace_directory(root, TAURI_RUNTIME)
    return destination


def host_target() -> str:
    system = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu", "Windows": "pc-windows-msvc"}.get(platform.system())
    machine = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}.get(platform.machine())
    return f"{machine}-{system}"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=os.environ.get("TAURI_ENV_TARGET_TRIPLE") or os.environ.get("CARGO_BUILD_TARGET") or host_target())
    parser.add_argument("--output", type=Path, default=REPOSITORY / "target/debug")
    parser.add_argument("--no-tauri", action="store_true")
    args = parser.parse_args()
    destination = prepare(args.target, args.output.resolve(), not args.no_tauri)
    print(f"Prepared {RELEASE['model']['name']} and llama.cpp {RELEASE['runtime']['version']} for {args.target}: {destination}")


if __name__ == "__main__":
    main()
