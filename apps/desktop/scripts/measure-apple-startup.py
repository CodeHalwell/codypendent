#!/usr/bin/env python3
"""Measure a bundled daemon in disposable local data dirs; never use user sessions."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import struct
import subprocess
import tempfile
import time
import uuid


def receive(peer: socket.socket, count: int) -> bytes:
    data = bytearray()
    while len(data) < count:
        part = peer.recv(count - len(data))
        if not part:
            raise ValueError("daemon closed before replying")
        data.extend(part)
    return bytes(data)


def process_stats(pid: int) -> tuple[float, int]:
    fields = subprocess.check_output(["ps", "-p", str(pid), "-o", "time=,rss="], text=True).split()
    elapsed = 0.0
    for value in fields[0].split(":"):
        elapsed = elapsed * 60 + float(value)
    return elapsed, int(fields[1])


def sample(app: Path, idle_seconds: float, clean_home: bool) -> dict:
    binary = app / "Contents/MacOS/codypendent"
    with tempfile.TemporaryDirectory(prefix="cody-startup-", dir="/tmp") as temporary:
        root = Path(temporary)
        home = root / "home" if clean_home else Path.home()
        if clean_home: home.mkdir()
        env = {key: os.environ[key] for key in ("USER", "LOGNAME", "TMPDIR", "LANG") if key in os.environ}
        env.update(HOME=str(home), PATH="/usr/bin:/bin:/usr/sbin:/sbin", RUST_LOG="info",
                   CODYPENDENT_DATA_DIR=str(root / "data"), CODYPENDENT_CONFIG_DIR=str(root / "config"),
                   CODYPENDENT_SOCKET=str(root / "daemon.sock"))
        log_path = root / "daemon.log"
        load_started = os.getloadavg()
        started = time.monotonic()
        with log_path.open("w") as log:
            daemon = subprocess.Popen([str(binary), "__daemon"], cwd=root, env=env, stdout=log, stderr=log)
            try:
                deadline = started + 60
                while not (root / "daemon.sock").exists():
                    if daemon.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"daemon did not bind: {log_path.read_text()[-4000:]}")
                    time.sleep(.01)
                bound = time.monotonic() - started
                with socket.socket(socket.AF_UNIX) as peer:
                    peer.settimeout(max(.1, deadline - time.monotonic()))
                    peer.connect(str(root / "daemon.sock"))
                    request = json.dumps({"protocol_version": {"major": 1, "minor": 10},
                                          "message_id": str(uuid.uuid4()), "client_id": str(uuid.uuid4()),
                                          "payload": {"type": "Ping"}}).encode()
                    peer.sendall(struct.pack(">I", len(request)) + request)
                    length = struct.unpack(">I", receive(peer, 4))[0]
                    if length > 16 * 1024 * 1024: raise ValueError("oversized daemon frame")
                    response = json.loads(receive(peer, length))
                    if response["payload"]["type"] != "Pong": raise ValueError("unexpected daemon reply")
                ready = time.monotonic() - started
                # Let the initial registry/index tasks settle before idle sampling.
                time.sleep(1)
                before_cpu, _ = process_stats(daemon.pid)
                before = time.monotonic()
                time.sleep(idle_seconds)
                after_cpu, rss_kib = process_stats(daemon.pid)
                interval = time.monotonic() - before
                text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text())
                integrity = re.search(r"bundled Node runtime integrity verified.*?elapsed_ms=(\d+)|elapsed_ms=(\d+).*?bundled Node runtime integrity verified", text)
                return {"load_average_start": load_started, "load_average_end": os.getloadavg(),
                        "socket_seconds": round(bound, 4), "ready_seconds": round(ready, 4),
                        "integrity_ms": int(next(group for group in integrity.groups() if group)) if integrity else None,
                        "idle_cpu_percent_one_core": round(max(0, after_cpu - before_cpu) / interval * 100, 3),
                        "idle_rss_mib": round(rss_kib / 1024, 2), "idle_sample_seconds": round(interval, 3)}
            finally:
                if daemon.poll() is None:
                    daemon.terminate()
                    try: daemon.wait(timeout=10)
                    except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--idle-seconds", type=float, default=5)
    parser.add_argument("--clean-home", action="store_true", help="Use empty HOME in addition to fresh data/config; this is not a clean-Mac test")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not 1 <= args.samples <= 10 or not 1 <= args.idle_seconds <= 30:
        parser.error("samples must be 1–10 and idle-seconds 1–30")
    app = args.app.resolve()
    binary = app / "Contents/MacOS/codypendent"
    with binary.open("rb") as executable:
        executable_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
    report = {"app": str(app), "macos": platform.mac_ver()[0], "architecture": platform.machine(),
              "configuration": "empty-home" if args.clean_home else "host-home-fresh-data",
              "binary_bytes": binary.stat().st_size, "executable_sha256": executable_sha256,
              "samples": [sample(app, args.idle_seconds, args.clean_home) for _ in range(args.samples)]}
    encoded = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
