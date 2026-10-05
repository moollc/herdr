"""Offline real-client acceptance fixture; the only HTTP target is loopback."""
import http.server
import json
import os
import shlex
from pathlib import Path
import subprocess
import sys
import threading

root, hook, socket_path, pane_id, task = sys.argv[1:]
root = Path(root)
client_home = root / 'client'
client_home.mkdir()
project = root / 'project'
project.mkdir()
subprocess.run(['git', 'init', '--quiet', str(project)], check=True)
(project / 'docs').mkdir()
(project / 'docs' / 'CODE_INDEX.md').write_text('Local fixture index.\n')
(project / 'SKILL.md').write_text('---\nname: herdr\n---\nLocal fixture skill.\n')
for name in ['AGENTS.md', 'CLAUDE.md', 'GEMINI.md']:
    (project / name).write_text('Local fixture instructions.\n')
requests = []


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{}')

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(request)
        message = {'id': 'msg_local', 'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'output_text', 'text': 'done', 'annotations': []}]}
        response = {'id': 'resp_local', 'object': 'response', 'status': 'completed',
                    'output': [message]}
        events = [
            {'type': 'response.created', 'response': dict(response, status='in_progress', output=[])},
            {'type': 'response.output_item.added', 'output_index': 0, 'item': message},
            {'type': 'response.output_item.done', 'output_index': 0, 'item': message},
            {'type': 'response.completed', 'response': response},
        ]
        body = ''.join('event: ' + event['type'] + '\ndata: ' + json.dumps(event) + '\n\n'
                       for event in events).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = http.server.HTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
(client_home / 'hooks.json').write_text(json.dumps({'hooks': {'SessionStart': [
    {'matcher': '*', 'hooks': [{'type': 'command', 'command': 'sh ' + shlex.quote(hook) + ' session', 'timeout': 10}]}
]}}))
(client_home / 'config.toml').write_text('''model="deepseek-v4-flash-local-fixture"
model_provider="local"
check_for_update_on_startup=false
[features]
hooks=true
[model_providers.local]
name="DeepSeek V4 Flash (offline fixture)"
base_url="http://127.0.0.1:%d/v1"
wire_api="responses"
requires_openai_auth=false
''' % server.server_port)
env = dict(os.environ, CODEX_HOME=str(client_home), HERDR_ENV='1',
           HERDR_SOCKET_PATH=socket_path, HERDR_PANE_ID=pane_id)
env.pop('OPENAI_API_KEY', None)
try:
    result = subprocess.run([
        os.environ.get('HERDR_TEST_CODEX', 'codex'), 'exec', '--dangerously-bypass-hook-trust',
        '--skip-git-repo-check', '-C', str(project), task,
    ], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, timeout=20)
    if result.returncode != 0 or not requests:
        raise RuntimeError(result.stderr.decode())
    print(json.dumps(requests[0]))
finally:
    server.shutdown()
