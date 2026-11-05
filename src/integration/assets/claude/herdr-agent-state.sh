#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=9

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
[ -n "${HERDR_SOCKET_PATH:-}" ] || { echo "herdr orientation: missing HERDR_SOCKET_PATH" >&2; exit 0; }
[ -n "${HERDR_PANE_ID:-}" ] || { echo "herdr orientation: missing HERDR_PANE_ID" >&2; exit 0; }
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" python3 - 3<&0 <<'PY'
import fcntl
import hashlib
import json
import os
import random
import socket
import stat
import sys
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
if hook_event_name == "SubagentStop":
    # A recap can emit this after the main turn stopped; do not revive an idle pane.
    raise SystemExit(0)
request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
if not agent_session_id:
    print("herdr orientation: missing native session_id; startup context unavailable", file=sys.stderr)
    raise SystemExit(0)
request = {
    "id": request_id,
    "method": "pane.report_agent_session",
    "params": {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
        "turn_reporting": hook_event_name != "UserPromptSubmit",
        "startup_source": ("user_prompt_submit" if hook_event_name == "UserPromptSubmit"
                           else hook_input.get("source")),
    },
}

def diagnostic(message):
    print("herdr orientation: " + message, file=sys.stderr)


def call(request):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.5)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        response = b""
        while not response.endswith(b"\n") and len(response) < 65536:
            chunk = client.recv(4096)
            if not chunk:
                break
            response += chunk
    result = json.loads(response)
    if not isinstance(result, dict) or "error" in result or not isinstance(result.get("result"), dict):
        raise ValueError("invalid/error server response")
    result = result["result"]
    if request["method"] == "pane.ack_orientation" and result.get("type") != "ok":
        raise ValueError("invalid emission acknowledgement response")
    return result


def retry(request):
    for attempt in range(3):
        try:
            return call(request)
        except Exception as error:
            if attempt == 2:
                raise error
            time.sleep(0.05 * (attempt + 1))


def deliver(context, nonce, hook_event_name):
    # The socket inode scopes receipts to this server instance; nonce binds the offer.
    server = os.stat(socket_path)
    identity = [os.path.realpath(socket_path), server.st_dev, server.st_ino,
                pane_id, agent_session_id, nonce]
    key = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
    directory = os.path.join(os.environ.get("TMPDIR", "/tmp"),
                             "herdr-claude-orientation-" + str(os.getuid()))
    try:
        os.mkdir(directory, 0o700)
    except FileExistsError:
        pass
    info = os.lstat(directory)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("receipt directory is not private")
    receipt = os.path.join(directory, key + ".json")
    # Serialize processes that received a stale copy of the same unacknowledged offer.
    with open(os.path.join(directory, key + ".lock"), "a") as lock:
        deadline = time.monotonic() + 2
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("another hook is emitting context; retry the native hook")
                time.sleep(0.05)
        state = None
        try:
            with open(receipt, encoding="utf-8") as handle:
                state = json.load(handle)["state"]
        except FileNotFoundError:
            pass
        if state not in (None, "emitted"):
            diagnostic("previous stdout emission is uncertain; context was not re-emitted or acknowledged. Start a fresh conversation to recover")
            return

        def record(state):
            temporary = receipt + ".tmp"
            with open(temporary, "w", encoding="utf-8") as handle:
                json.dump({"state": state}, handle)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, receipt)

        if state is None:
            # Persist uncertainty first; an emission receipt never means consumption.
            record("emission_uncertain")
            print(json.dumps({"hookSpecificOutput": {
                "hookEventName": hook_event_name, "additionalContext": context}}), flush=True)
            record("emitted")
        ack = {"id": request_id + ":ack", "method": "pane.ack_orientation", "params": {
            "pane_id": pane_id, "source": source, "agent_session_id": agent_session_id,
            "nonce": nonce}}
        try:
            retry(ack)
        except Exception as error:
            diagnostic("stdout emitted, acknowledgement failed (not model receipt); retry the native hook: " + str(error))


try:
    result = retry(request)
    context, nonce = result.get("context"), result.get("nonce")
    if isinstance(context, str) and context and isinstance(nonce, str) and nonce:
        event_name = "UserPromptSubmit" if hook_event_name == "UserPromptSubmit" else "SessionStart"
        deliver(context, nonce, event_name)
    elif hook_input.get("source") == "startup":
        diagnostic("server returned no startup context: already delivered or incompatible server; check Herdr version and integration status")
except Exception as error:
    diagnostic("startup delivery failed; context may be unavailable or emission uncertain; retry SessionStart: " + str(error))
PY
