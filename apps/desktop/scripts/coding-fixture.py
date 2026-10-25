#!/usr/bin/env python3
"""Create an isolated, real-provider coding fixture without contacting a provider.

The committed standard-library Python project starts with four failing tests.
The selected model must make real edits and run those tests; this script supplies
no model responses, credentials, daemon database, or recorded session history.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit


FILES = {
    "words.py": '''def word_counts(text):
    counts = {}
    for word in text.split():
        counts[word] = counts.get(word, 0) + 1
    return counts
''',
    "cli.py": '''import argparse
import json
from words import word_counts

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("text")
    args = parser.parse_args()
    print(json.dumps(word_counts(args.text)))

if __name__ == "__main__":
    main()
''',
    "tests/test_words.py": '''import json
import subprocess
import sys
import unittest
from words import word_counts

class WordTests(unittest.TestCase):
    def test_case_and_punctuation(self):
        self.assertEqual(word_counts("Hello, HELLO! world."), {"hello": 2, "world": 1})
    def test_empty(self):
        self.assertEqual(word_counts("...  "), {})
    def test_unicode(self):
        self.assertEqual(word_counts("CAFÉ café"), {"café": 2})
    def test_top(self):
        p = subprocess.run([sys.executable, "cli.py", "Pear apple pear BANANA apple", "--top", "2"], text=True, capture_output=True)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout), [["apple", 2], ["pear", 2]])
    def test_negative_top(self):
        p = subprocess.run([sys.executable, "cli.py", "apple", "--top", "-1"], text=True, capture_output=True)
        self.assertNotEqual(p.returncode, 0)

if __name__ == "__main__":
    unittest.main()
''',
    "README.md": "# Word report\n\nSmall standard-library Python CLI. Run `python3 -m unittest discover -s tests -v`.\n",
    ".gitignore": "__pycache__/\n*.pyc\n",
}

OBJECTIVE = (
    "Fix this word-report CLI using only the Python standard library. "
    "Make word_counts case-insensitive, preserve Unicode letters, and ignore punctuation, "
    "including returning an empty mapping for punctuation-only input. Add --top N to cli.py: "
    "return a JSON list of the top N [word, count] pairs, ordered by descending count then "
    "alphabetically for ties. Reject negative N and preserve the existing JSON mapping output "
    "when --top is absent. Keep the existing tests intact, update the README, and run "
    "python3 -m unittest discover -s tests -v. Review the resulting changes and report the "
    "actual test result."
)


def endpoint(value: str) -> str:
    parsed = urlsplit(value)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise argparse.ArgumentTypeError("use an http(s) OpenAI-compatible base URL")
    if parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
        raise argparse.ArgumentTypeError("base URL must not contain credentials, query parameters, or fragments")
    if any(ord(char) < 32 for char in value):
        raise argparse.ArgumentTypeError("base URL must be one line")
    return value.rstrip("/")


def environment_name(value: str) -> str:
    if value and not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
        raise argparse.ArgumentTypeError("supply an environment variable NAME, not its value")
    return value


def positive_integer(value: str) -> int:
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def verify_baseline(repository: Path) -> None:
    result = subprocess.run(
        [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
        cwd=repository, text=True, capture_output=True, timeout=30, check=False,
    )
    expected = ("case_and_punctuation", "empty", "unicode", "top")
    if (result.returncode != 1 or "Ran 5 tests" not in result.stderr
            or "FAILED (failures=4)" not in result.stderr
            or any(not re.search(rf"test_{name} \([^\n]+\) \.\.\. FAIL", result.stderr) for name in expected)
            or not re.search(r"test_negative_top \([^\n]+\) \.\.\. ok", result.stderr)):
        raise RuntimeError(f"Unexpected fixture baseline:\n{result.stdout}{result.stderr}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, help="Real provider's model identifier")
    parser.add_argument("--base-url", required=True, type=endpoint, help="OpenAI-compatible endpoint, including /v1 when required")
    parser.add_argument("--model-id", default="real-code", help="Local Codypendent model id (default: real-code)")
    parser.add_argument("--api-key-env", default="", type=environment_name, help="Optional API key environment variable NAME; its value is never read or saved")
    parser.add_argument("--context-tokens", default=32768, type=positive_integer, help="Configured context limit (default: 32768)")
    parser.add_argument("--parent", type=Path, default=Path("/tmp"), help="Parent for a fresh disposable directory")
    args = parser.parse_args()
    if any(not value.strip() or any(ord(char) < 32 for char in value) for value in (args.model, args.model_id)):
        parser.error("model and model-id must be nonempty single-line identifiers")
    if not args.parent.is_dir():
        parser.error("parent must be an existing directory")

    root = Path(tempfile.mkdtemp(prefix="cody-coding-", dir=args.parent)).resolve()
    repository = root / "repository"
    data = root / "data"
    config = data / "config"
    config.mkdir(parents=True)
    repository.mkdir()
    for name, contents in FILES.items():
        path = repository / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "--quiet", str(repository)], check=True)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True)
    subprocess.run([
        "git", "-C", str(repository), "-c", "user.name=Codypendent Fixture",
        "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
        "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "Initial coding fixture",
    ], check=True)
    model = {
        "id": args.model_id, "provider": "openai-compatible", "base_url": args.base_url,
        "model": args.model, "api_key_env": args.api_key_env, "context_tokens": args.context_tokens,
    }
    (data / "models.toml").write_text(
        "[[model]]\n" + "".join(f"{key} = {json.dumps(value, ensure_ascii=False)}\n" for key, value in model.items()),
        encoding="utf-8",
    )
    (config / "desktop.json").write_text(json.dumps({"repository": str(repository)}, indent=2) + "\n", encoding="utf-8")
    (config / "policy.toml").write_text('[shell]\nallowed_programs = ["python3"]\n', encoding="utf-8")
    verify_baseline(repository)
    print("Baseline verified: 4 expected failures, 1 pass. No provider was contacted.")
    print(f"Disposable fixture: {root}")
    print("Fixture-only policy adds python3 to the shell allowlist; per-command approvals remain required.")
    print("Run these exports before launching Codypendent:")
    for name, value in (
        ("CODYPENDENT_DATA_DIR", data), ("CODYPENDENT_CONFIG_DIR", config),
        ("CODYPENDENT_SOCKET", root / "daemon.sock"),
    ):
        print(f"export {name}={shlex.quote(str(value))}")
    print(f"Repository: {repository}")
    print(f"Select model: {args.model_id}")
    if args.api_key_env:
        print(f"Supply {args.api_key_env} in the launch environment; this fixture stores only its name.")
    print("Objective:")
    print(OBJECTIVE)


if __name__ == "__main__":
    main()
