#!/usr/bin/env python3
"""Measure real IPC over a disposable synthetic repository and session history.

Sessions are created through the daemon. Large history is seeded only while its
owned daemon is stopped, using valid NoteAppended events in the disposable DB.
A single read-only run uses the loopback synthetic provider before measurement,
so file search exercises the normal repository ownership check. No external
provider or user configuration is used. The fixture is retained for UI QA.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import socket
import sqlite3
import statistics
import struct
import subprocess
import tempfile
import threading
import time
import uuid


class FixtureModel(BaseHTTPRequestHandler):
    """The same fixed, loopback-only model contract as smoke-provider.py."""
    def log_message(self, *_args):
        pass

    def reply(self, body, kind="application/json"):
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply(json.dumps({"object": "list", "data": [{"id": "local-smoke", "object": "model",
                     "created": 0, "owned_by": "local-fixture"}]}).encode())

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        base = {"id": "workspace-fixture", "created": 0, "model": "local-smoke"}
        text = "Synthetic workspace fixture ready. No files were changed."
        if request.get("stream"):
            chunks = [
                {**base, "object": "chat.completion.chunk", "choices": [{"index": 0,
                    "delta": {"role": "assistant", "content": text}, "finish_reason": None}]},
                {**base, "object": "chat.completion.chunk", "choices": [{"index": 0,
                    "delta": {}, "finish_reason": "stop"}]},
            ]
            self.reply(("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
                        + "data: [DONE]\n\n").encode(), "text/event-stream")
        else:
            self.reply(json.dumps({**base, "object": "chat.completion", "choices": [{"index": 0,
                "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]}).encode())


def establish_repository(client, root, session, repository):
    client.command({"type": "StartRun", "session_id": session, "objective": "Reply with the fixture readiness message.",
                    "mode": {"type": "Ask"}, "repository": str(repository), "model": "local-smoke"})
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        with sqlite3.connect(f"file:{root / 'data/codypendent.db'}?mode=ro", uri=True) as database:
            row = database.execute("SELECT state FROM runs WHERE session_id=? ORDER BY rowid DESC LIMIT 1", (session,)).fetchone()
        state = row[0].lower() if row else None
        if state in ("succeeded", "completed"):
            return
        if state in ("failed", "cancelled", "interrupted"):
            raise RuntimeError("synthetic fixture run failed: " + state)
        time.sleep(.1)
    raise RuntimeError("synthetic fixture run did not complete; inspect " + str(root / "daemon.log"))


def receive(peer, count):
    data = bytearray()
    while len(data) < count:
        chunk = peer.recv(count - len(data))
        if not chunk: raise RuntimeError("daemon disconnected")
        data.extend(chunk)
    return bytes(data)


class Client:
    def __init__(self, path, workspace):
        self.peer = socket.socket(socket.AF_UNIX)
        self.peer.settimeout(120)
        self.peer.connect(str(path))
        self.client = str(uuid.uuid4())
        self.workspace = workspace
        self.request({"type": "ClientHello", "client_name": "workspace-performance-fixture",
                      "client_version": "0.14.0", "supported_protocols": [{"major": 1, "minor": 10}],
                      "capabilities": {"session_library": True}})

    def send(self, payload):
        message = {"protocol_version": {"major": 1, "minor": 10}, "client_id": self.client,
                   "workspace_id": self.workspace, "message_id": str(uuid.uuid4()), "payload": payload}
        body = json.dumps(message).encode()
        self.peer.sendall(struct.pack(">I", len(body)) + body)
        return message["message_id"]

    def request(self, payload):
        message_id = self.send(payload)
        while True:
            size = struct.unpack(">I", receive(self.peer, 4))[0]
            if size > 16 * 1024 * 1024: raise RuntimeError("oversized response")
            response = json.loads(receive(self.peer, size))
            if response["payload"]["type"] == "Ping":
                self.send({"type": "Pong"})
                continue
            if response.get("correlation_id") == message_id:
                if response["payload"]["type"] in ("CommandRejected", "Error"):
                    raise RuntimeError(response["payload"])
                self.response_bytes = size
                return response

    def command(self, body):
        command = str(uuid.uuid4())
        return self.request({"type": "Command", "command_id": command, "idempotency_key": command, "body": body})


def start(binary, root, workspace):
    env = {"HOME": str(root / "home"), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "RUST_LOG": "info",
           "CODYPENDENT_DATA_DIR": str(root / "data"), "CODYPENDENT_CONFIG_DIR": str(root / "config"),
           "CODYPENDENT_SOCKET": str(root / "daemon.sock")}
    log = (root / "daemon.log").open("a")
    process = subprocess.Popen([str(binary), "__daemon"], cwd=root, env=env, stdout=log, stderr=log)
    log.close()
    try:
        deadline = time.monotonic() + 120
        while not (root / "daemon.sock").exists():
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("fixture daemon did not start; inspect " + str(root / "daemon.log"))
            time.sleep(.01)
        return process, Client(root / "daemon.sock", workspace)
    except BaseException:
        stop(process)
        raise


def stop(process):
    if process.poll() is None:
        process.terminate()
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired: process.kill(); process.wait()


def measure(client, body, repetitions=15):
    timings = []
    response = None
    for _ in range(repetitions):
        started = time.perf_counter()
        response = client.command(body)
        timings.append((time.perf_counter() - started) * 1000)
        payload = response["payload"]
        items = payload.get("sessions", payload.get("files", payload.get("matches", payload.get("page", {}).get("items"))))
        if items is None or not items:
            raise RuntimeError("measurement received no results: " + str(payload))
    values = sorted(timings)
    return {"result_count": len(items), "samples": repetitions, "first_ms": round(timings[0], 3),
            "median_ms": round(statistics.median(values), 3),
            "p95_ms": round(values[min(len(values)-1, int(len(values)*.95))], 3),
            "response_type": response["payload"]["type"],
            "response_bytes": client.response_bytes}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("--files", type=int, default=5000)
    parser.add_argument("--sessions", type=int, default=1000)
    parser.add_argument("--events", type=int, default=25000)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not (1 <= args.files <= 50000 and 1 <= args.sessions <= 10000 and 1 <= args.events <= 100000):
        parser.error("fixture size is out of bounds")
    binary = args.app.resolve() / "Contents/MacOS/codypendent"
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("app must contain the executable bundled CLI")
    with binary.open("rb") as executable:
        executable_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
    root = Path(tempfile.mkdtemp(prefix="cody-workspace-perf-", dir="/tmp"))
    (root / "home").mkdir()
    repository = root / "repository"
    repository.mkdir()
    for number in range(args.files):
        directory = repository / f"package_{number // 100:03}"
        directory.mkdir(exist_ok=True)
        (directory / f"file_{number:05}.ts").write_text(
            f"// Synthetic workspace fixture {number}\nexport function value_{number}(input: number): number {{\n  return input + {number};\n}}\n")
    git_env = {"HOME": str(root / "home"), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "GIT_CONFIG_NOSYSTEM": "1"}
    subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "init", "-q", str(repository)], check=True, env=git_env)
    subprocess.run(["git", "-C", str(repository), "add", "."], check=True, env=git_env)
    subprocess.run(["git", "-C", str(repository), "-c", "user.name=Performance Fixture", "-c", "user.email=fixture@example.invalid",
                    "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "Synthetic performance fixture"], check=True, env=git_env)
    workspace = str(uuid.uuid4())
    (root / "config").mkdir()
    (root / "config/desktop.json").write_text(json.dumps({"repository": str(repository), "workspace": workspace}) + "\n")
    (root / "data").mkdir()
    provider = ThreadingHTTPServer(("127.0.0.1", 0), FixtureModel)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    (root / "data/models.toml").write_text(
        f'[[model]]\nid = "local-smoke"\nprovider = "openai-compatible"\n'
        f'base_url = "http://127.0.0.1:{provider.server_address[1]}/v1"\n'
        'model = "local-smoke"\napi_key_env = ""\ncontext_tokens = 8192\n')
    first_session = None
    try:
        process, client = start(binary, root, workspace)
        try:
            for number in range(args.sessions):
                response = client.command({"type": "CreateSession", "workspace": workspace,
                                           "title": f"Synthetic session {number:05}", "repository": str(repository)})
                first_session = first_session or response["session_id"]
            establish_repository(client, root, first_session, repository)
        finally:
            client.peer.close()
            stop(process)
    finally:
        provider.shutdown()
        provider.server_close()
    # Valid session-level notes, separate from the actual synthetic provider run.
    database = root / "data/codypendent.db"
    now = datetime.now(timezone.utc).isoformat()
    with sqlite3.connect(database) as connection:
        sequence = connection.execute("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id=?", (first_session,)).fetchone()[0]
        connection.executemany("INSERT INTO events(session_id,sequence,occurred_at,actor,body) VALUES(?,?,?,?,?)",
            [(first_session, sequence + number + 1, now, '{"type":"System"}',
              json.dumps({"type": "NoteAppended", "text": f"Synthetic history note {number:05}: validating workspace navigation and durable history. ✓"})) for number in range(args.events)])
        connection.execute("UPDATE sessions SET revision=?, updated_at=? WHERE id=?", (sequence + args.events, now, first_session))
    manifest = {"fixture": str(root), "repository": str(repository), "workspace_id": workspace,
                "history_session": first_session, "files": args.files, "sessions": args.sessions,
                "additional_history_events": args.events, "synthetic": True}
    (root / "fixture.json").write_text(json.dumps(manifest, indent=2) + "\n")
    started = time.monotonic()
    process, client = start(binary, root, workspace)
    try:
        report = {**manifest, "app": str(args.app.resolve()), "executable_sha256": executable_sha256,
                  "executable_bytes": binary.stat().st_size, "load_average": os.getloadavg(),
                  "history_startup_seconds": round(time.monotonic() - started, 4),
                  "list_sessions_200": measure(client, {"type": "ListSessions", "workspace": workspace, "limit": 200}),
                  "search_sessions_50": measure(client, {"type": "SearchSessions", "query": {"query": "Synthetic", "limit": 50}}),
                  "read_history_100": measure(client, {"type": "ReadSessionHistory", "session_id": first_session, "limit": 100}),
                  "find_workspace_file": measure(client, {"type": "SearchWorkspaceFiles", "repository": str(repository), "query": f"file_{args.files-1:05}.ts", "limit": 20})}
        encoded = json.dumps(report, indent=2) + "\n"
        (root / "measurements.json").write_text(encoded)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(encoded)
        print(encoded, end="")
    finally:
        client.peer.close()
        stop(process)


if __name__ == "__main__":
    main()
