#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=7

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-codex-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import socket
import time

source = "herdr:codex"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
if agent_session_id:
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "codex",
            "seq": report_seq,
            "agent_session_id": agent_session_id,
            "startup_source": hook_input.get("source"),
        },
    }
else:
    raise SystemExit(0)

def call(request):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    response = b""
    while not response.endswith(b"\n") and len(response) < 65536:
        chunk = client.recv(4096)
        if not chunk:
            break
        response += chunk
    client.close()
    return json.loads(response)

# A receipt survives a lost acknowledgement response.  It is deliberately
# written after stdout: a process crash in that tiny interval is uncertain and
# may duplicate context; pretending that it was delivered would lose context.
receipt = os.path.join(os.environ.get("TMPDIR", "/tmp"),
    "herdr-codex-orientation-" + str(abs(hash((pane_id, agent_session_id)))) + ".json")
try:
    response = call(request)
    result = response.get("result", {}) if isinstance(response, dict) else {}
    context, nonce = result.get("context"), result.get("nonce")
    if isinstance(context, str) and context and isinstance(nonce, str) and nonce:
        emitted = False
        try:
            with open(receipt, encoding="utf-8") as handle:
                emitted = json.load(handle).get("nonce") == nonce
        except Exception:
            pass
        if not emitted:
            print(json.dumps({"hookSpecificOutput": {
                "hookEventName": "SessionStart", "additionalContext": context}}), flush=True)
            try:
                with open(receipt, "w", encoding="utf-8") as handle:
                    json.dump({"nonce": nonce}, handle)
            except Exception:
                pass
        ack = {"id": request_id + ":ack", "method": "pane.ack_orientation", "params": {
            "pane_id": pane_id, "source": source, "agent_session_id": agent_session_id, "nonce": nonce}}
        for _ in range(2):
            try:
                ack_response = call(ack)
                if isinstance(ack_response, dict) and "error" not in ack_response:
                    try: os.unlink(receipt)
                    except OSError: pass
                    break
            except Exception:
                time.sleep(0.05)
except Exception:
    pass
PY
