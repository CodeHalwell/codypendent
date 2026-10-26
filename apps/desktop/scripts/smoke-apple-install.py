#!/usr/bin/env python3
"""Exercise the real CLI installer using a local release and disposable paths.

The GitHub download is replaced by an offline fixture. The app, archive,
checksum verification, complete-bundle validation and replacement are real.
No system Applications folder or user data is modified.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path)
    parser.add_argument("app", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    cli, app = args.cli.resolve(), args.app.resolve()
    with tempfile.TemporaryDirectory(prefix="cody-install-smoke-", dir="/tmp") as temporary:
        root = Path(temporary)
        for directory in ("release", "source", "bin", "home", "data", "config", "Applications"):
            (root / directory).mkdir()
        source = root / "source/Codypendent.app"
        subprocess.run(["/usr/bin/ditto", str(app), str(source)], check=True)
        quarantine = b"0081;00000000;CodypendentInstallFixture;"
        subprocess.run(["/usr/bin/xattr", "-w", "com.apple.quarantine", quarantine.decode(), str(source)], check=True)
        asset = root / "release/codypendent-desktop-aarch64-apple-darwin.tar.gz"
        # BSD tar preserves macOS attributes; use fast compression for this local test.
        archive_env = dict(os.environ)
        archive_env.pop("COPYFILE_DISABLE", None)
        subprocess.run(["/usr/bin/tar", "-czf", str(asset), "--options", "gzip:compression-level=1",
                        "-C", str(source.parent), source.name], env=archive_env, check=True)

        def manifest():
            with asset.open("rb") as data:
                digest = hashlib.file_digest(data, "sha256").hexdigest()
            (asset.parent / "SHA256SUMS").write_text(f"{digest}  {asset.name}\n")

        manifest()
        helper = root / "bin/gh"
        helper.write_text(f"#!{sys.executable}\n" + '''import fnmatch, os, pathlib, shutil, sys
args = sys.argv[1:]
if args[:2] != ["release", "download"]: raise SystemExit("Unexpected gh operation")
destination = pathlib.Path(args[args.index("-D") + 1])
patterns = [args[index + 1] for index, item in enumerate(args) if item == "-p"]
for pattern in patterns:
    matches = [path for path in pathlib.Path(os.environ["CODYPENDENT_FIXTURE_RELEASE"]).iterdir() if fnmatch.fnmatchcase(path.name, pattern)]
    if not matches: raise SystemExit("Missing fixture release asset")
    for source in matches: shutil.copy2(source, destination / source.name)
''')
        helper.chmod(0o755)
        env = {"HOME": str(root / "home"), "PATH": f"{root / 'bin'}:/usr/bin:/bin:/usr/sbin:/sbin",
               "CODYPENDENT_DATA_DIR": str(root / "data"), "CODYPENDENT_CONFIG_DIR": str(root / "config"),
               "CODYPENDENT_SOCKET": str(root / "daemon.sock"),
               "CODYPENDENT_APPLICATIONS_DIR": str(root / "Applications"),
               "CODYPENDENT_FIXTURE_RELEASE": str(asset.parent)}
        retained = root / "data/history-preserved.txt"
        retained.write_text("existing session data remains outside the app\n")

        def install(expected=None):
            result = subprocess.run([str(cli), "install", "desktop", "offline-install-fixture"],
                                    cwd=root, env=env, text=True, capture_output=True, timeout=240)
            combined = result.stdout + result.stderr
            if expected is None:
                if result.returncode: raise RuntimeError(combined)
            elif result.returncode == 0 or expected not in combined:
                raise RuntimeError(f"Expected refusal containing {expected!r}: {combined}")

        def check_quarantine(path):
            # BSD tar retains the quarantine flag but normalizes its agent metadata.
            value = subprocess.check_output(["/usr/bin/xattr", "-p", "com.apple.quarantine", str(path)]).strip()
            assert value and int(value.split(b";", 1)[0], 16) & 1, value

        install()
        installed = root / "Applications/Codypendent.app"
        check_quarantine(installed)
        sentinel = installed / ".installation-smoke-sentinel"
        sentinel.write_text("previous app")
        with (installed / "Contents/MacOS/codypendent-desktop").open("rb"):
            install("is in use")
        assert sentinel.read_text() == "previous app"
        size = asset.stat().st_size
        with asset.open("ab") as data: data.write(b"damaged download")
        install("checksum mismatch")
        assert sentinel.read_text() == "previous app"
        with asset.open("r+b") as data: data.truncate(size)

        complete = asset.with_suffix(".complete")
        asset.rename(complete)
        try:
            with tarfile.open(asset, "w:gz", compresslevel=1) as archive:
                archive.add(source / "Contents/Info.plist", arcname="Codypendent.app/Contents/Info.plist")
            manifest()
            install("desktop bundle is incomplete")
            assert sentinel.read_text() == "previous app"
        finally:
            asset.unlink(missing_ok=True)
            complete.rename(asset)
            manifest()
        install()
        assert not sentinel.exists()
        assert retained.read_text() == "existing session data remains outside the app\n"
        check_quarantine(installed)
        assert not list((root / "Applications").glob(".codypendent-install-*"))
        report = {"cli": str(cli), "app": str(app), "offline_download_fixture": True,
                  "first_install": "passed", "open_bundle_refused": "passed",
                  "corrupt_download_preserves_previous_app": "passed",
                  "incomplete_bundle_preserves_previous_app": "passed", "replacement": "passed",
                  "external_data_preserved": "passed", "quarantine_preserved": "passed",
                  "temporary_staging_cleaned": "passed"}
    encoded = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
