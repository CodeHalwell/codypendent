#!/usr/bin/env python3
"""Serve a loopback-only synthetic provider for native desktop tool acceptance."""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

MARKER = "CODYPENDENT_DESKTOP_TOOLS_ACCEPTANCE"
MODEL = "desktop-tools-fixture"
CUSTOM = "desktop.custom.fixtureWorkspace"
OBJECTIVE = f"{MARKER}: Display an interactive workspace check with a working counter, create a reusable workspace helper using desktop.getWorkspace, run the newly advertised helper, and report its actual returned workspace information."
WIDGET = {
    "initialHeight": 300,
    "placeholderMessages": ["Preparing the isolated interactive check"],
    "css": "body{background:#17231e;color:#f3f0e7}h2{font-size:24px}button{border:0;border-radius:10px;padding:12px 18px;background:#e9b34a;color:#17231e;cursor:pointer}.count{font-size:36px;margin:18px 0}",
    "html": "<h2>Workspace tools are connected</h2><p>This interactive result was produced by the isolated native acceptance fixture.</p><div class='count' id='count'>0</div><button id='increment'>Add one</button>",
    "jsFunctions": "let fixtureCount = 0;",
    "jsExpressions": ["document.getElementById('increment').addEventListener('click', () => {document.getElementById('count').textContent = String(++fixtureCount);})"],
}
DEFINITION = {
    "name": CUSTOM,
    "description": "Read and label the current desktop workspace for this isolated acceptance check.",
    "parameters": {"type": "object", "properties": {"label": {"type": "string", "maxLength": 80}}, "required": ["label"], "additionalProperties": False},
    "functionBody": "const workspace = await host.call('desktop.getWorkspace', {}); return {fixture: 'CODYPENDENT_DESKTOP_TOOLS_ACCEPTANCE', label: args.label, workspace};",
    "capabilities": ["desktop.getWorkspace"],
}


def content_text(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "".join(str(part.get("text", "")) for part in value if isinstance(part, dict))
    return ""


def tool_aliases(request: dict) -> dict[str, str]:
    aliases = {}
    for tool in request.get("tools", []):
        function = tool.get("function", {})
        name = function.get("name", "")
        marker = re.search(r"\[native tool: ([^\]]+)\]", function.get("description", ""))
        aliases[marker.group(1) if marker else name] = name
    return aliases


def choose_reply(request: dict) -> tuple[str | None, object]:
    messages = request.get("messages", [])
    latest_user = next((message for message in reversed(messages) if message.get("role") == "user"), {})
    if MARKER not in content_text(latest_user.get("content")):
        return None, "Loopback synthetic provider is ready. Send the desktop-tools acceptance objective to run the interactive fixture."
    advertised = tool_aliases(request)
    canonical = {wire: native for native, wire in advertised.items()}
    calls = {call.get("id"): call.get("function", {}).get("name") for message in messages for call in message.get("tool_calls", [])}
    results = {canonical.get(calls.get(message.get("tool_call_id")), calls.get(message.get("tool_call_id"))): content_text(message.get("content")) for message in messages if message.get("role") == "tool"}
    for name, arguments in (("generateSandboxedUi", WIDGET), ("desktop.defineTool", DEFINITION), (CUSTOM, {"label": "Native desktop acceptance"})):
        if name not in results:
            if name not in advertised:
                return None, f"Fixture blocked: {name} was not advertised by this model request. No tool execution was fabricated."
            return advertised[name], arguments
    return None, "Native desktop tool sequence completed. The reusable helper was advertised after definition and returned this actual result:\n\n" + results[CUSTOM]


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, root: Path, port: int = 0):
        super().__init__(("127.0.0.1", port), Handler)
        self.root = root
        self.lock = threading.Lock()
        self.serial = 0

    def record(self, value: dict) -> None:
        with self.lock, (self.root / "requests.jsonl").open("a", encoding="utf-8") as output:
            output.write(json.dumps(value, ensure_ascii=False) + "\n")


class Handler(BaseHTTPRequestHandler):
    server: FixtureServer

    def log_message(self, format: str, *args: object) -> None:
        return

    def respond(self, status: int, body: bytes, kind: str = "application/json") -> None:
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path in ("/v1/models", "/models"):
            value = {"object": "list", "data": [{"id": MODEL, "object": "model", "created": 0, "owned_by": "isolated-loopback-fixture"}]}
        elif path == "/healthz":
            value = {"ready": True, "marker": MARKER, "root": str(self.server.root)}
        else:
            self.respond(404, b'{"error":"Unknown fixture route"}')
            return
        self.respond(200, json.dumps(value).encode())

    def do_POST(self) -> None:
        if urlsplit(self.path).path not in ("/v1/chat/completions", "/chat/completions"):
            self.respond(404, b'{"error":"Unknown fixture route"}')
            return
        size = int(self.headers.get("Content-Length", "0"))
        if size <= 0 or size > 8 * 1024 * 1024:
            self.respond(413, b'{"error":"Fixture request is empty or too large"}')
            return
        try:
            request = json.loads(self.rfile.read(size))
            name, value = choose_reply(request)
        except (ValueError, TypeError, AttributeError) as error:
            self.respond(400, json.dumps({"error": str(error)}).encode())
            return
        with self.server.lock:
            self.server.serial += 1
            serial = self.server.serial
        self.server.record({"request": serial, "stream": request.get("stream", False), "advertised": [tool.get("function", {}).get("name") for tool in request.get("tools", [])], "tool_results": [{"callId": message.get("tool_call_id"), "content": content_text(message.get("content"))[:32768]} for message in request.get("messages", []) if message.get("role") == "tool"], "response_tool": name, "response_text": value if name is None else None})
        body, kind = response_body(name, value, serial, request.get("stream", False))
        try:
            self.respond(200, body, kind)
        except (BrokenPipeError, ConnectionResetError):
            pass


def response_body(name: str | None, value: object, serial: int, stream: bool) -> tuple[bytes, str]:
    base = {"id": f"fixture-response-{serial}", "object": "chat.completion.chunk", "created": 0, "model": MODEL}
    usage = {"prompt_tokens": 10, "completion_tokens": 24, "total_tokens": 34}
    reason = "tool_calls" if name else "stop"
    if not stream:
        message = {"role": "assistant", "content": value if name is None else None}
        if name:
            message["tool_calls"] = [{"id": f"fixture-call-{serial}", "type": "function", "function": {"name": name, "arguments": json.dumps(value)}}]
        body = {**base, "object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": reason}], "usage": usage}
        return json.dumps(body).encode(), "application/json"
    chunks = [{**base, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]} for delta in response_deltas(name, value, serial)]
    chunks.append({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": usage})
    body = "".join("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
    return body.encode(), "text/event-stream"


def response_deltas(name: str | None, value: object, serial: int) -> list[dict]:
    deltas = [{"role": "assistant"}]
    if name:
        arguments = json.dumps(value, ensure_ascii=False)
        deltas.append({"tool_calls": [{"index": 0, "id": f"fixture-call-{serial}", "type": "function", "function": {"name": name, "arguments": ""}}]})
        deltas.extend({"tool_calls": [{"index": 0, "function": {"arguments": arguments[start:start + 97]}}]} for start in range(0, len(arguments), 97))
    else:
        text = str(value)
        deltas.extend({"content": text[start:start + 80]} for start in range(0, len(text), 80))
    return deltas


def main() -> None:
    repository_root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, default=repository_root / "target")
    parser.add_argument("--serve-root", type=Path, help="Resume this fixture without replacing its data or repository")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--source-repository", type=Path, default=repository_root)
    args = parser.parse_args()
    args.parent.mkdir(parents=True, exist_ok=True)
    root = args.serve_root.resolve() if args.serve_root else Path(tempfile.mkdtemp(prefix="native-agui-", dir=args.parent)).resolve()
    repository = root / "repository"
    if not args.serve_root:
        subprocess.run(["git", "clone", "--shared", "--no-checkout", "--quiet", str(args.source_repository.resolve()), str(repository)], check=True)
        subprocess.run(["git", "-C", str(repository), "sparse-checkout", "set", "--no-cone", "/README.md"], check=True)
        subprocess.run(["git", "-C", str(repository), "checkout", "--quiet", "--detach", "HEAD"], check=True)
    data = root / "data"
    config = data / "config"
    config.mkdir(parents=True, exist_ok=True)
    server = FixtureServer(root, args.port)
    port = server.server_address[1]
    model = {"id": MODEL, "provider": "openai-compatible", "base_url": f"http://127.0.0.1:{port}/v1", "model": MODEL, "api_key_env": "", "context_tokens": 32768}
    (data / "models.toml").write_text("[[model]]\n" + "".join(f"{key} = {json.dumps(value)}\n" for key, value in model.items()))
    if not (config / "desktop.json").exists():
        (config / "desktop.json").write_text(json.dumps({"repository": str(repository), "runDefaults": {"model": MODEL, "mode": {"type": "Build"}}}, indent=2) + "\n")
    fixture = {"root": str(root), "data": str(data), "config": str(config), "socket": str(root / "daemon.sock"), "repository": str(repository), "port": port, "baseUrl": model["base_url"], "model": MODEL, "objective": OBJECTIVE, "requests": str(root / "requests.jsonl"), "providerPid": os.getpid()}
    (root / "fixture.json").write_text(json.dumps(fixture, indent=2) + "\n")
    (root / "objective.txt").write_text(OBJECTIVE + "\n")
    print(json.dumps(fixture, indent=2), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
