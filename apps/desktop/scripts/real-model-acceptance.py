#!/usr/bin/env python3
"""Exercise installed HTTP models through the production daemon and desktop tools.

This is headless runtime evidence. The shared acceptance host executes the exact
requested helper source; packaged WebKit rendering is tested separately.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import tomllib

ROOT = Path(__file__).resolve().parents[3]
USER_DATA = Path.home() / "Library/Application Support/codypendent"


def harness():
    path = Path(__file__).with_name("real-agent-acceptance.py")
    spec = importlib.util.spec_from_file_location("real_agent_acceptance", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.OBJECTIVE = """This is a real desktop integration acceptance check. Use only the supplied desktop tools, not shell, file or network tools. Keep the output small. First call generateSandboxedUi to make a compact HTML card with a button and counter (initialHeight 200, placeholderMessages []). Then call desktop.defineTool: name desktop.custom.realWorkspace, description Read the current acceptance workspace, parameters {"type":"object","properties":{},"additionalProperties":false}, capabilities ["desktop.getWorkspace"], functionBody exactly "return await host.call('desktop.getWorkspace', {});". The newly defined tool is advertised on the next model request: call it. Finally report the actual returned title and repository in two plain prose sentences. Do not return raw JSON or source code to the user. If a tool is unavailable, explain its actual error. Never claim a tool ran unless its real result arrived."""
    return module


def fixture(root: Path, source: Path) -> tuple[Path, Path]:
    root.mkdir(parents=True, exist_ok=True)
    data, repository = root / "data", root / "repository"
    data.mkdir(mode=0o700, exist_ok=True)
    data.chmod(0o700)
    if not repository.exists():
        git_env = {**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null"}
        subprocess.run(["/usr/bin/git", "clone", "--shared", "--no-checkout", "--quiet", str(source), str(repository)], check=True, timeout=60, env=git_env)
        subprocess.run(["/usr/bin/git", "-C", str(repository), "sparse-checkout", "set", "--no-cone", "/README.md"], check=True, timeout=30, env=git_env)
        subprocess.run(["/usr/bin/git", "-C", str(repository), "checkout", "--quiet", "--detach", "HEAD"], check=True, timeout=30, env=git_env)
    # These are references, not copies. This harness invokes no configuration
    # mutations or discovery commands, and its tool host refuses file access.
    for name in ("models.toml", "auth.json"):
        target, origin = data / name, USER_DATA / name
        if target.is_symlink():
            if target.resolve() != origin.resolve():
                raise ValueError(f"Unexpected {name} reference")
        elif target.exists():
            raise ValueError(f"Refusing to replace existing {name}")
        elif origin.exists():
            target.symlink_to(origin)
    return data, repository


def outcome(summary: dict, evidence: Path) -> dict:
    calls = summary.get("tools", [])
    successful = [call["tool"] for call in calls if call.get("outcome", {}).get("type") == "Succeeded"]
    host_path = evidence / "host-results.jsonl"
    host_results = [json.loads(line) for line in host_path.read_text().splitlines()] if host_path.exists() else []
    custom = [call for call in host_results if call["name"] == "desktop.custom.realWorkspace" and call.get("error") is None]
    return {
        "model": summary["model"], "sessionId": summary["sessionId"], "runId": summary["runId"],
        "outcome": summary["outcome"], "seconds": summary["seconds"],
        "successfulTools": successful, "generatedUi": "generateSandboxedUi" in successful,
        "definedTool": "desktop.defineTool" in successful,
        "customRoundtrip": "desktop.custom.realWorkspace" in successful and bool(custom),
        "hostAdapter": "permission-restricted Node; exact requested source; real isolated workspace context",
        "packagedWebKit": False,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--source-repository", type=Path, default=ROOT)
    parser.add_argument("--model", action="append")
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--parallel", type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 180:
        parser.error("--seconds must be between 1 and 180")
    rows = tomllib.loads((USER_DATA / "models.toml").read_text()).get("model", [])
    available = {row["id"] for row in rows if row.get("provider_id") in ("ollama", "cerebras") and row.get("provider") == "openai-compatible"}
    models = args.model or sorted(available)
    if set(models) - available:
        parser.error("A requested model is not a configured Ollama/Cerebras HTTP profile")
    root = args.root.resolve()
    data, repository = fixture(root, args.source_repository.resolve())
    evidence = root / "evidence"
    evidence.mkdir(exist_ok=True)
    cli = ROOT / "target/debug/codypendent"
    module = harness()
    socket_path = Path(f"/tmp/cpd-http-{os.getpid()}.sock")
    env = {**os.environ, "CODYPENDENT_DATA_DIR": str(data), "CODYPENDENT_CONFIG_DIR": str(data / "config"), "CODYPENDENT_SOCKET": str(socket_path)}
    module.atomic(root / "fixture.json", {"models": models, "secondsPerModel": args.seconds, "parallel": args.parallel, "repository": str(repository), "cli": str(cli), "cliSha256": hashlib.sha256(cli.read_bytes()).hexdigest(), "credentialHandling": "Existing auth referenced through the production runtime; no key read, printed or copied by this script", "acceptance": "headless production daemon and native desktop tools; not packaged WebKit"})
    log = (root / "daemon.log").open("a")
    daemon = subprocess.Popen([str(cli), "__daemon"], cwd=repository, env=env, stdout=log, stderr=log, start_new_session=True)
    summaries = []

    def run(model: str) -> dict:
        connection = module.Connection(socket_path)
        try:
            summary = module.run_model(connection, model, repository, data, evidence, args.seconds)
            safe = model.replace("/", "_").replace("#", "_").replace(":", "_")
            return outcome(summary, evidence / safe)
        finally:
            connection.socket.close()

    try:
        deadline = time.monotonic() + 30
        while not socket_path.exists() and time.monotonic() < deadline:
            if daemon.poll() is not None:
                raise RuntimeError("Isolated daemon exited; inspect its log")
            time.sleep(0.1)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel) as executor:
            jobs = {executor.submit(run, model): model for model in models}
            for job in concurrent.futures.as_completed(jobs):
                try:
                    result = job.result()
                except Exception as error:
                    result = {"model": jobs[job], "outcome": "harness-error", "reason": str(error)}
                summaries.append(result)
                module.atomic(root / "summary.json", summaries)
                print(json.dumps(result), flush=True)
    finally:
        if daemon.poll() is None:
            os.killpg(daemon.pid, signal.SIGTERM)
            daemon.wait(timeout=10)
        log.close()
        socket_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
