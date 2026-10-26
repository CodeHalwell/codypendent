#!/usr/bin/env python3
"""Loopback-only synthetic model for manual desktop and TUI smoke checks.

Creates disposable configuration and a git repository. Never use this fixture
for model quality or usage measurements; its reply and token counts are fixed.
No external model, credentials or downloads are used.
"""
import argparse
import json
import subprocess
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--parent', type=Path, default=Path('/tmp'), help='Parent of a new disposable fixture directory')
args = parser.parse_args()
ROOT = Path(tempfile.mkdtemp(prefix='cody-smoke-', dir=args.parent)).resolve()
(ROOT / 'config').mkdir()
(ROOT / 'repository').mkdir()
subprocess.run(['git', 'init', '--quiet', str(ROOT / 'repository')], check=True)
(ROOT / 'repository' / 'README.md').write_text('# Codypendent local smoke fixture\n\nDisposable repository for native client checks.\n')
subprocess.run(['git', '-C', str(ROOT / 'repository'), 'add', 'README.md'], check=True)
subprocess.run(['git', '-C', str(ROOT / 'repository'), '-c', 'user.name=Codypendent Smoke',
                '-c', 'user.email=smoke@example.invalid', 'commit', '--quiet', '-m', 'Initial smoke fixture'], check=True)
TEXT = 'Local smoke check passed. The desktop sent the objective through the daemon and displayed this streamed fixture response. No external model was contacted.'

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({'object': 'list', 'data': [{'id': 'local-smoke', 'object': 'model', 'created': 0, 'owned_by': 'local-fixture'}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        base = {'id': 'local-smoke-response', 'object': 'chat.completion.chunk', 'created': 0, 'model': 'local-smoke'}
        if request.get('stream'):
            chunks = []
            for content in (TEXT[:36], TEXT[36:]):
                chunks.append('data: ' + json.dumps({**base, 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': content}, 'finish_reason': None}]}) + '\n\n')
            chunks.append('data: ' + json.dumps({**base, 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 10, 'completion_tokens': 24, 'total_tokens': 34}}) + '\n\ndata: [DONE]\n\n')
            body = ''.join(chunks).encode()
            kind = 'text/event-stream'
        else:
            body = json.dumps({**base, 'object': 'chat.completion', 'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': TEXT}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 10, 'completion_tokens': 24, 'total_tokens': 34}}).encode()
            kind = 'application/json'
        self.send_response(200)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
port = server.server_address[1]
(ROOT / 'models.toml').write_text(f'[[model]]\nid = "local-smoke"\nprovider = "openai-compatible"\nbase_url = "http://127.0.0.1:{port}/v1"\nmodel = "local-smoke"\napi_key_env = ""\ncontext_tokens = 8192\n')
print(f'Loopback synthetic provider listening on {port}', flush=True)
print(f'Disposable data: {ROOT}', flush=True)
print(f'CODYPENDENT_DATA_DIR={ROOT}', flush=True)
print(f'CODYPENDENT_CONFIG_DIR={ROOT / "config"}', flush=True)
print(f'CODYPENDENT_SOCKET={ROOT / "daemon.sock"}', flush=True)
print(f'Repository: {ROOT / "repository"}', flush=True)
try:
    server.serve_forever()
except KeyboardInterrupt:
    pass
finally:
    server.server_close()
