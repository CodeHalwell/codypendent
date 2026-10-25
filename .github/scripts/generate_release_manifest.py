#!/usr/bin/env python3
"""Write checksums for final distributable bytes, after signing/notarization."""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import re


def generate(directory: Path) -> Path:
    assets = sorted(path for path in directory.iterdir() if path.name != "SHA256SUMS")
    if not assets:
        raise ValueError("no release assets found")
    rows = []
    for path in assets:
        if (not path.is_file() or path.is_symlink()
                or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", path.name)
                or not path.name.endswith((".tar.gz", ".AppImage", ".vsix", ".dmg"))):
            raise ValueError(f"unexpected release asset: {path.name}")
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        rows.append(f"{digest}  {path.name}\n")
    output = directory / "SHA256SUMS"
    output.write_text("".join(rows), encoding="ascii")
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(generate(args.directory))
