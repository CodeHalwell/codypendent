#!/usr/bin/env python3
"""Enforce the release workflow's least-privilege and supply-chain invariants."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release.yml"
CI_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci.yml"
REMOTE_ACTION_RE = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")
USES_RE = re.compile(r"^\s*(?:-\s+)?uses:\s+([^\s#]+)")
ROOT_MANIFEST = "./Cargo.toml"
DESKTOP_MANIFEST = "apps/desktop/src-tauri/Cargo.toml"
DESKTOP_ADVISORY_CONFIG = "apps/desktop/src-tauri/deny.toml"
POSTGRES_IMAGE_RE = re.compile(r"^\s+image:\s+postgres:[^@\s]+@sha256:[0-9a-f]{64}$")


def section(lines: list[str], header: str, indent: int) -> list[str]:
    prefix = " " * indent + header
    try:
        start = next(index for index, line in enumerate(lines) if line == prefix)
    except StopIteration as error:
        raise ValueError(f"missing {header.rstrip(':')!r} section") from error

    body: list[str] = []
    for line in lines[start + 1 :]:
        if line and len(line) - len(line.lstrip()) <= indent:
            break
        body.append(line)
    return body


def action_steps(lines: list[str]) -> list[list[str]]:
    starts = [
        index
        for index, line in enumerate(lines)
        if line.startswith("      - ")
    ]
    return [
        lines[start : starts[position + 1] if position + 1 < len(starts) else len(lines)]
        for position, start in enumerate(starts)
    ]


def input_value(step: list[str], name: str) -> str | None:
    pattern = re.compile(rf"^\s+{re.escape(name)}:\s*(\S.*)$")
    for line in step:
        if match := pattern.match(line):
            return match.group(1).split(" #", 1)[0].strip()
    return None


def main() -> int:
    try:
        lines = RELEASE_WORKFLOW.read_text(encoding="utf-8").splitlines()
        ci_lines = CI_WORKFLOW.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        print(f"release workflow security: could not read workflow: {error}", file=sys.stderr)
        return 2

    failures: list[str] = []

    try:
        default_permissions = section(lines, "permissions:", 0)
        if "  contents: read" not in default_permissions:
            failures.append("top-level permissions must set contents: read")
    except ValueError as error:
        failures.append(str(error))

    try:
        publish = section(lines, "publish:", 2)
        publish_condition = " ".join(section(publish, "if: >-", 4))
        if "github.event_name == 'push' &&" not in publish_condition:
            failures.append("publish must require a push event; manual dispatch must only build artifacts")
        publish_permissions = section(publish, "permissions:", 4)
        if "      contents: write" not in publish_permissions:
            failures.append("publish job must explicitly set contents: write")
        if sum(line.strip() == "contents: write" for line in lines) != 1:
            failures.append("publish must be the only job with contents: write")
        write_permissions = [
            line.strip()
            for line in lines
            if re.fullmatch(r"[a-z-]+:\s*write(?:-all)?", line.strip())
        ]
        if write_permissions != ["contents: write"]:
            failures.append(
                "publish contents: write must be the workflow's only write permission; "
                f"found {write_permissions}"
            )
        if any(line.strip() == "permissions: write-all" for line in lines):
            failures.append("permissions: write-all is forbidden")
    except ValueError as error:
        failures.append(str(error))

    for job, signer in (("build:", "sign-apple-runtime.sh"), ("desktop-bundle:", "sign-apple-release.sh")):
        try:
            body = section(lines, job, 2)
            signing = next(step for step in action_steps(body) if any(signer in line for line in step))
            required = input_value(signing, "REQUIRE_APPLE_SIGNING") or ""
            if "startsWith(github.ref, 'refs/tags/')" not in required:
                failures.append(f"{job[:-1]} must require Apple signing for every tag")
            if not any('elif [ "$REQUIRE_APPLE_SIGNING" = "true" ]' in line for line in signing) or not any(line.strip() == "exit 1" for line in signing):
                failures.append(f"{job[:-1]} must fail when required Apple credentials are absent")
        except (ValueError, StopIteration):
            failures.append(f"{job[:-1]} is missing its Apple distribution signing gate")

    for workflow_name, workflow_lines in (("release", lines), ("CI", ci_lines)):
        for step in action_steps(workflow_lines):
            uses_match = next(
                (USES_RE.match(line) for line in step if USES_RE.match(line)), None
            )
            if uses_match is None:
                continue
            action = uses_match.group(1)
            if not action.startswith("./") and REMOTE_ACTION_RE.fullmatch(action) is None:
                failures.append(
                    f"{workflow_name} remote action is not pinned to a full commit SHA: {action}"
                )
            if (
                action.startswith("actions/checkout@")
                and input_value(step, "persist-credentials") != "false"
            ):
                failures.append(
                    f"{workflow_name} checkout must set persist-credentials: false: {action}"
                )

    # Migration immutability compares the checked-in manifest with HEAD^. A
    # depth-1 checkout removes that trust anchor and would turn every run into a
    # false bootstrap, so both jobs that invoke the checker must fetch a parent.
    for workflow_name, workflow_lines, job_name in (
        ("release", lines, "rust-lint:"),
        ("CI", ci_lines, "lint:"),
    ):
        try:
            job = section(workflow_lines, job_name, 2)
        except ValueError as error:
            failures.append(str(error))
            continue
        checkout_steps = [
            step
            for step in action_steps(job)
            if any("uses: actions/checkout@" in line for line in step)
        ]
        if len(checkout_steps) != 1 or input_value(checkout_steps[0], "fetch-depth") != "2":
            failures.append(
                f"{workflow_name} {job_name.rstrip(':')} checkout must set fetch-depth: 2"
            )

    expected_profiles = {
        (ROOT_MANIFEST, None),
        (DESKTOP_MANIFEST, "licenses bans sources"),
        (DESKTOP_MANIFEST, "advisories"),
    }
    for workflow_name, workflow_lines in (("release", lines), ("CI", ci_lines)):
        workflow_text = "\n".join(workflow_lines)
        postgres_images = [
            line.strip() for line in workflow_lines if "image: postgres:" in line
        ]
        if len(postgres_images) != 1 or not any(
            POSTGRES_IMAGE_RE.fullmatch(line) for line in workflow_lines
        ):
            failures.append(
                f"{workflow_name} PostgreSQL service image must be pinned to one sha256 digest"
            )
        if "POSTGRES_PASSWORD:" in workflow_text:
            failures.append(
                f"{workflow_name} ephemeral PostgreSQL service must not contain a password"
            )
        if workflow_text.count("POSTGRES_HOST_AUTH_METHOD: trust") != 1:
            failures.append(
                f"{workflow_name} ephemeral PostgreSQL service must use local trust auth exactly once"
            )
        if workflow_text.count(
            "DATABASE_URL: postgres://postgres@127.0.0.1:5432/codypendent_test"
        ) != 1:
            failures.append(
                f"{workflow_name} tests must use the passwordless loopback PostgreSQL URL exactly once"
            )
        deny_steps = [
            step
            for step in action_steps(workflow_lines)
            if any("uses: EmbarkStudios/cargo-deny-action@" in line for line in step)
        ]
        profiles = {
            (
                input_value(step, "manifest-path"),
                input_value(step, "command-arguments"),
            )
            for step in deny_steps
        }
        if profiles != expected_profiles or len(deny_steps) != len(expected_profiles):
            failures.append(
                f"{workflow_name} cargo-deny must run the root policy plus desktop "
                "license/source and advisory profiles exactly once; found "
                f"{sorted(str(value) for value in profiles)}"
            )
        for step in deny_steps:
            manifest = input_value(step, "manifest-path") or "<missing manifest>"
            if input_value(step, "command") != "check":
                failures.append(
                    f"{workflow_name} cargo-deny command for {manifest} must be check"
                )
            arguments = (input_value(step, "arguments") or "").split()
            for required in ("--all-features", "--locked", "--config"):
                if required not in arguments:
                    failures.append(
                        f"{workflow_name} cargo-deny arguments for {manifest} "
                        f"must include {required}"
                    )
            expected_config = (
                DESKTOP_ADVISORY_CONFIG
                if input_value(step, "command-arguments") == "advisories"
                else "deny.toml"
            )
            try:
                config = arguments[arguments.index("--config") + 1]
            except (ValueError, IndexError):
                config = None
            if config != expected_config:
                failures.append(
                    f"{workflow_name} cargo-deny profile for {manifest} must use "
                    f"{expected_config}, found {config}"
                )

    try:
        root_policy = tomllib.loads((REPO_ROOT / "deny.toml").read_text(encoding="utf-8"))
        desktop_policy = tomllib.loads(
            (REPO_ROOT / DESKTOP_ADVISORY_CONFIG).read_text(encoding="utf-8")
        )
        root_ignores = {
            entry["id"] for entry in root_policy["advisories"].get("ignore", [])
        }
        desktop_ignores = {
            entry["id"] for entry in desktop_policy["advisories"].get("ignore", [])
        }
        missing_ignores = root_ignores - desktop_ignores
        if missing_ignores:
            failures.append(
                "desktop advisory policy must retain root advisory exceptions; missing "
                + ", ".join(sorted(missing_ignores))
            )
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as error:
        failures.append(f"could not validate cargo-deny policy inheritance: {error}")

    if failures:
        print(
            f"release workflow security: FAILED — {len(failures)} problem(s):",
            file=sys.stderr,
        )
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        "release workflow security: OK — read-only defaults, isolated publish "
        "permission, immutable actions/service images, credential-safe checkouts, "
        "passwordless ephemeral PostgreSQL, and both Rust lockfiles are gated in CI "
        "and release"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
