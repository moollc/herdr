#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=7

set -eu

action="${1:-}"
case "$action" in
  started|ended|notify)
    command -v python3 >/dev/null 2>&1 || exit 0
    exec python3 "$(dirname "$0")/herdr-turn-report.py" claude "$action" "${2:-}"
    ;;
esac

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" python3 - 3<&0 <<'PY'
import json
import os
import random
import socket
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")

if not pane_id or not socket_path:
    raise SystemExit(0)

try:
    with os.fdopen(3, encoding="utf-8") as handle:
        hook_input = json.load(handle)
    if not isinstance(hook_input, dict):
        raise ValueError("hook input is not an object")
except (OSError, ValueError):
    raise SystemExit(0)

hook_event_name = str(hook_input.get("hook_event_name") or "")
is_subagent = bool(hook_input.get("agent_id"))
if hook_event_name == "SubagentStop":
    # SubagentStop is a completion event. Older Herdr integrations mapped it
    # to durable working, but Claude recap/away-summary can emit it after the
    # main turn has already stopped. Never let it revive an idle pane.
    raise SystemExit(0)
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
            "agent": "claude",
            "seq": report_seq,
            "agent_session_id": agent_session_id,
            "turn_reporting": True,
        },
    }
else:
    raise SystemExit(0)

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
PY
