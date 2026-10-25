#!/usr/bin/env python3
"""Run installed agents through an isolated daemon and its desktop MCP bridge."""
from __future__ import annotations

import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import threading
import time
import tomllib
import uuid

VERSION = {"major": 1, "minor": 11}
ROOT = Path(__file__).resolve().parents[3]
USER_DATA = Path.home() / "Library/Application Support/codypendent"
HOST = {"name": "desktop.getWorkspace", "description": "Read the actual isolated acceptance workspace context.", "parameters": {"type": "object", "properties": {}, "additionalProperties": False}, "capabilities": [], "version": 0}
OBJECTIVE = """This is a real desktop integration acceptance check. Do not inspect files, run shell commands, access the network with tools, or modify the repository. Use only the codypendent-desktop MCP server. First generateSandboxedUi: make a compact card with a button and a counter using html, css, jsFunctions and jsExpressions (initialHeight 200, placeholderMessages []). Then desktop.defineTool: name desktop.custom.realWorkspace, description Read the current acceptance workspace, parameters {\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}, capabilities [\"desktop.getWorkspace\"], functionBody exactly \"return await host.call('desktop.getWorkspace', {});\". Refresh your MCP tool list if needed, then call the newly defined realWorkspace tool. If the client keeps the original tool list, use desktop.executeTool with name desktop.custom.realWorkspace and args {}. Finally report the actual returned title and repository in two plain prose sentences. Do not return raw JSON or source code to the user. If tools are unavailable, state the actual error. Never claim a tool ran unless its real result arrived."""


def uid() -> str:
    return str(uuid.uuid4())


def atomic(path: Path, value: object) -> None:
    temporary = path.with_suffix(f".{uid()}.tmp")
    temporary.write_text(json.dumps(value))
    temporary.chmod(0o600)
    temporary.replace(path)


class Connection:
    def __init__(self, path: Path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.connect(str(path))
        self.socket.settimeout(0.5)
        self.client = uid()
        self.buffer = b""
        self.pending: list[dict] = []
        self.request({"type": "ClientHello", "client_name": "real-agent-acceptance", "client_version": "0.14.0", "supported_protocols": [VERSION], "capabilities": {key: False for key in ("rich_text", "image_display", "audio_capture", "editor_mutations", "diff_view", "mouse", "unicode", "true_color")}})

    def send(self, payload: dict) -> str:
        message = uid()
        body = json.dumps({"protocol_version": VERSION, "message_id": message, "client_id": self.client, "payload": payload}).encode()
        self.socket.sendall(struct.pack(">I", len(body)) + body)
        return message

    def read(self) -> dict | None:
        if self.pending:
            return self.pending.pop(0)
        while True:
            if len(self.buffer) >= 4:
                size = struct.unpack(">I", self.buffer[:4])[0]
                if size > 16 * 1024 * 1024:
                    raise RuntimeError("Daemon frame exceeds limit")
                if len(self.buffer) >= 4 + size:
                    body, self.buffer = self.buffer[4:4 + size], self.buffer[4 + size:]
                    value = json.loads(body)
                    if value["payload"]["type"] == "Ping":
                        self.send({"type": "Pong"})
                        continue
                    return value
            try:
                data = self.socket.recv(65536)
            except socket.timeout:
                return None
            if not data:
                raise RuntimeError("Daemon closed connection")
            self.buffer += data

    def request(self, payload: dict) -> dict:
        message = self.send(payload)
        held = []
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            reply = self.read()
            if reply is None:
                continue
            if reply.get("correlation_id") == message:
                self.pending = held + self.pending
                if reply["payload"]["type"] in ("Error", "CommandRejected"):
                    raise RuntimeError(json.dumps(reply["payload"]))
                return reply
            held.append(reply)
        raise TimeoutError("Daemon command did not reply; received " + json.dumps(held)[:4000])

    def command(self, name: str, **fields) -> dict:
        command = uid()
        return self.request({"type": "Command", "command_id": command, "idempotency_key": command, "body": {"type": name, **fields}})


def serve_host(directory: Path, context: dict, stopped: threading.Event, output: Path) -> None:
    handled: set[str] = set()
    published = 0.0
    while not stopped.wait(0.1):
        if time.monotonic() - published >= 10:
            atomic(directory / "context.json", {"updatedAt": int(time.time() * 1000), "context": context, "tools": [HOST]})
            published = time.monotonic()
        for path in directory.glob("request-*.json"):
            call = json.loads(path.read_text())
            if call["callId"] in handled:
                continue
            handled.add(call["callId"])
            result, error = None, None
            if call["name"] == HOST["name"]:
                result = context
            elif call["definition"].get("functionBody") and call["definition"].get("capabilities") == [HOST["name"]]:
                # Evaluate the actual generated source with a fixture host. This
                # is separate from packaged WebKit acceptance, which runs in-app.
                program = "const vm=require('node:vm');const payload=JSON.parse(process.argv[1]);const context=vm.createContext({host:Object.freeze({call:async(name,args)=>{if(name!=='desktop.getWorkspace'||Object.keys(args).length)throw Error('Capability denied');return payload.context;}})});vm.runInContext('(async()=>{'+payload.body+'})()',context,{timeout:1000}).then(value=>process.stdout.write(JSON.stringify(value)),error=>{console.error(String(error));process.exitCode=1;});"
                executed = subprocess.run(["node", "--permission", "--no-addons", "-e", program, json.dumps({"body": call["definition"]["functionBody"], "context": context})], capture_output=True, text=True, timeout=5, env={"PATH": os.environ.get("PATH", ""), "NO_COLOR": "1"})
                if executed.returncode == 0:
                    result = json.loads(executed.stdout)
                else:
                    error = executed.stderr[:2000]
            else:
                error = "Acceptance host refuses unexpected source or capability"
            atomic(directory / f"result-{call['callId']}.json", {"callId": call["callId"], "result": result, "error": error})
            with output.open("a") as stream:
                stream.write(json.dumps({"callId": call["callId"], "name": call["name"], "result": result, "error": error}) + "\n")


def run_model(connection: Connection, model: str, repository: Path, data: Path, evidence: Path, seconds: int) -> dict:
    safe = model.replace("/", "_").replace("#", "_").replace(":", "_")
    out = evidence / safe
    out.mkdir(parents=True, exist_ok=True)
    session = connection.command("CreateSession", workspace=uid(), title=f"Real agent acceptance: {model}", repository=str(repository))["session_id"]
    connection.command("AttachSession", session_id=session, subscriptions=[{"type": "SessionSummary"}, {"type": "AgentActivity"}], requested_role={"type": "Controller"}, repository=str(repository))
    directory = data / "desktop-tools" / session
    directory.mkdir(parents=True, mode=0o700)
    context = {"view": "sessions", "repository": str(repository), "sessionId": session, "selectedModel": model, "mode": "Build", "title": f"Real acceptance for {model}"}
    atomic(directory / "context.json", {"updatedAt": int(time.time() * 1000), "context": context, "tools": [HOST]})
    stopped = threading.Event()
    host = threading.Thread(target=serve_host, args=(directory, context, stopped, out / "host-results.jsonl"), daemon=True)
    host.start()
    start = time.monotonic()
    run = connection.command("StartRun", session_id=session, objective=OBJECTIVE, mode={"type": "Build"}, repository=str(repository), model=model)["payload"].get("created_run")
    context["runId"] = run
    summary = {"model": model, "sessionId": session, "runId": run, "startedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(), "outcome": "timeout", "tools": [], "approvals": [], "assistant": ""}
    try:
        with (out / "events.jsonl").open("a") as stream:
            while time.monotonic() - start < seconds:
                frame = connection.read()
                if frame is None:
                    continue
                if frame.get("session_id") != session:
                    continue
                stream.write(json.dumps(frame) + "\n")
                stream.flush()
                body = frame.get("payload", {}).get("body", {})
                kind = body.get("type")
                if kind == "ApprovalRequested":
                    action = body["action"]
                    allowed = action.get("type") == "McpToolCall" and action.get("server") == "desktop"
                    if action.get("type") == "AcpToolCall":
                        detail = json.dumps(action)
                        allowed = any(name in detail for name in ("codypendent-desktop", "generateSandboxedUi", "realWorkspace", "desktop.defineTool", "cpd_desktop_"))
                    summary["approvals"].append({"action": action, "approved": allowed})
                    connection.command("ResolveApproval", approval_id=body["approval_id"], decision={"type": "Approve" if allowed else "Reject"}, scope={"type": "Once"})
                elif kind == "ToolCompleted":
                    summary["tools"].append({"tool": body.get("tool"), "outcome": body.get("outcome"), "artifact": body.get("artifact")})
                elif kind == "ModelStreamDelta":
                    summary["assistant"] += body.get("text", "")
                elif kind == "RunCompleted":
                    summary["outcome"] = body.get("disposition")
                    break
            else:
                cancelled = time.monotonic()
                connection.command("CancelRun", session_id=session, run_id=run)
                summary["timedOut"] = True
                while time.monotonic() - cancelled < 10:
                    frame = connection.read()
                    if frame is None or frame.get("session_id") != session:
                        continue
                    stream.write(json.dumps(frame) + "\n")
                    body = frame.get("payload", {}).get("body", {})
                    if body.get("type") == "RunCompleted":
                        summary["outcome"] = body.get("disposition")
                        summary["cancellationSeconds"] = round(time.monotonic() - cancelled, 2)
                        break
    finally:
        stopped.set()
        host.join(timeout=6)
    summary["seconds"] = round(time.monotonic() - start, 2)
    required = {"generateSandboxedUi", "desktop.defineTool", "desktop.custom.realWorkspace"}
    completed = {tool["tool"] for tool in summary["tools"] if tool.get("outcome", {}).get("type") == "Succeeded"}
    summary["desktopRoundtripPassed"] = required.issubset(completed) and (out / "host-results.jsonl").exists()
    atomic(out / "summary.json", summary)
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--model", action="append", required=True)
    parser.add_argument("--coordinate", action="append", default=[], help="Override MODEL=ACP_COORDINATE in isolated test configuration only")
    parser.add_argument("--seconds", type=int, default=150)
    args = parser.parse_args()
    root = args.root.resolve()
    data, repository = root / "data", root / "repository"
    data.mkdir(parents=True, exist_ok=True)
    evidence = root / "evidence"
    evidence.mkdir(exist_ok=True)
    if not repository.exists():
        repository.mkdir()
        subprocess.run(["git", "init", "--quiet", str(repository)], check=True)
        (repository / "README.md").write_text("# Isolated real-agent acceptance\nNo application source or credentials are stored here.\n")
        subprocess.run(["git", "-C", str(repository), "add", "README.md"], check=True)
        subprocess.run(["git", "-C", str(repository), "-c", "user.name=Acceptance", "-c", "user.email=acceptance@localhost", "commit", "--quiet", "-m", "Isolated acceptance repository"], check=True)
    rows = tomllib.loads((USER_DATA / "models.toml").read_text()).get("model", [])
    selected = []
    overrides = dict(value.split("=", 1) for value in args.coordinate)
    for model in args.model:
        row = next((row for row in rows if row["id"] == model), None)
        if row is None and model.startswith("acp/"):
            row = {"id": model, "provider": "acp", "model": model[4:]}
        if row is None or row["provider"] != "acp":
            raise ValueError(f"No configured ACP model: {model}")
        row = dict(row)
        if model in overrides:
            row["model"] = overrides[model]
        selected.append(row)
    (data / "models.toml").write_text("\n\n".join("[[model]]\n" + "\n".join(f"{key} = {json.dumps(value)}" for key, value in row.items() if isinstance(value, (str, int))) for row in selected) + "\n")
    if not (data / "acp").exists():
        (data / "acp").symlink_to(USER_DATA / "acp", target_is_directory=True)
    socket_path = Path(f"/tmp/cpd-real-{os.getpid()}.sock")
    env = {**os.environ, "CODYPENDENT_DATA_DIR": str(data), "CODYPENDENT_SOCKET": str(socket_path), "NPM_CONFIG_OFFLINE": "true"}
    log = (root / "daemon.log").open("a")
    daemon = subprocess.Popen([str(ROOT / "target/debug/codypendent"), "__daemon"], cwd=repository, env=env, stdout=log, stderr=log, start_new_session=True)
    try:
        deadline = time.monotonic() + 12
        while not socket_path.exists() and time.monotonic() < deadline:
            if daemon.poll() is not None:
                raise RuntimeError("Isolated daemon exited; inspect its log")
            time.sleep(0.1)
        connection = Connection(socket_path)
        for model in args.model:
            summary = run_model(connection, model, repository, data, evidence, args.seconds)
            print(json.dumps({key: summary[key] for key in ("model", "sessionId", "runId", "outcome", "seconds", "desktopRoundtripPassed", "tools")}), flush=True)
    finally:
        os.killpg(daemon.pid, signal.SIGTERM)
        daemon.wait(timeout=10)
        log.close()
        socket_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
