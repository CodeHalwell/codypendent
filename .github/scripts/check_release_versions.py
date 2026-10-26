#!/usr/bin/env python3
"""Keep first-party app and current user-guide version markers aligned."""
from pathlib import Path
import json
import re
import sys
import tomllib

root = Path(__file__).resolve().parents[2]
expected = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
versions = {
    "apps/desktop/src-tauri/Cargo.toml": tomllib.loads((root / "apps/desktop/src-tauri/Cargo.toml").read_text())["package"]["version"],
    **{name: json.loads((root / name).read_text())["version"] for name in [
        "apps/desktop/package.json", "apps/desktop/src-tauri/tauri.conf.json", "docs/MANIFEST.json",
    ]},
}
guide = re.search(r"^> \*\*Version:\*\* (\S+)", (root / "docs/cli-and-tui-user-guide.md").read_text(), re.M)
versions["docs/cli-and-tui-user-guide.md"] = guide[1] if guide else "missing"
errors = [f"{name}: {version} (expected {expected})" for name, version in versions.items() if version != expected]
if errors:
    print("release versions: FAILED\n" + "\n".join(errors), file=sys.stderr)
    sys.exit(1)
print(f"release versions: OK — app and current guide target {expected}")
