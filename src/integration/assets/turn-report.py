#!/usr/bin/env python3
"""Managed native turn reporting. Never transmit prompts or response text.

Reports the native Cursor session_id for the parent pane only (child agents never replace the parent
identity). Binding happens on sessionStart and on beforeSubmitPrompt (action started), using session_id
only. Turn start or end is reported through pane.report_turn.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

kind, action = sys.argv[1:3]
notify = action == "notify"
raw = sys.argv[3] if notify and len(sys.argv) > 3 else sys.stdin.read()
try:
    payload = json.loads(raw) if raw.strip() else {}
except (ValueError, TypeError):
    payload = {}

SESSION_SOURCE = "herdr:cursor"
# Native hook event that binds the parent session for each supported action. Both carry the same session_id.
SESSION_EVENTS = {"session": "sessionStart", "started": "beforeSubmitPrompt"}


def valid_native_session_id(value):
    """Bounded, printable, unpadded native session id. Refuse anything else; never truncate or repair it."""
    return isinstance(value, str) and 0 < len(value) <= 512 and value.isprintable() and value == value.strip()


def report_session():
    """Bind the parent native Cursor session so Herdr can verify the recipient. Uses session_id only, never
    conversation_id. Never sends prompt, response, tool, attachment, user, or credential text."""
    event = SESSION_EVENTS.get(action)
    if kind != "cursor" or event is None or notify or not isinstance(payload, dict):
        return
    # A child agent never replaces the parent pane identity (same rule as the turn report below).
    if payload.get("agent_id") or payload.get("hook_event_name") != event:
        return
    if os.environ.get("HERDR_ENV") != "1":
        return
    pane = os.environ.get("HERDR_PANE_ID")
    address = os.environ.get("HERDR_SOCKET_PATH")
    session_id = payload.get("session_id")
    if not pane or not address or not valid_native_session_id(session_id):
        return
    seq = time.time_ns()
    request = {"id": f"{SESSION_SOURCE}:{seq}", "method": "pane.report_agent_session",
               "params": {"pane_id": pane, "source": SESSION_SOURCE, "agent": "cursor", "seq": seq,
                          "agent_session_id": session_id, "turn_reporting": True}}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(0.5)
            client.connect(address)
            client.sendall((json.dumps(request) + "\n").encode())
            client.recv(4096)
    except OSError:
        pass


def report():
    if os.environ.get("HERDR_ENV") != "1":
        return
    pane = os.environ.get("HERDR_PANE_ID")
    address = os.environ.get("HERDR_SOCKET_PATH")
    if not pane or not address or not isinstance(payload, dict) or payload.get("agent_id"):
        return
    if notify:
        if payload.get("type") != "agent-turn-complete":
            return
        event = "ended"
    else:
        expected = {
            "claude": {"started": "UserPromptSubmit", "ended": "Stop"},
            "codex": {"started": "UserPromptSubmit"},
            "cursor": {"session": "sessionStart", "started": "beforeSubmitPrompt", "ended": "stop"},
        }
        if payload.get("hook_event_name") != expected.get(kind, {}).get(action):
            return
        event = {"session": "ready", "started": "started", "ended": "ended"}.get(action)
        if event is None:
            return
    params = {"pane_id": pane, "agent": kind, "event": event, "seq": time.time_ns()}
    # Only the native turn identifier is retained, for idempotent notifications.
    turn = payload.get("turn-id") if notify else None
    if isinstance(turn, str) and len(turn) <= 512:
        params["turn_id"] = turn
    request = {"id": "native-turn", "method": "pane.report_turn", "params": params}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(0.5)
            client.connect(address)
            client.sendall((json.dumps(request) + "\n").encode())
            client.recv(4096)
    except OSError:
        pass


try:
    report_session()
    report()
finally:
    # Preserve user notification behavior inside and outside Herdr, including
    # the original unmodified native JSON argument. Never use a shell.
    if notify:
        try:
            delegate = json.loads(Path(__file__).with_name("herdr-notify-delegate.json").read_text())
            if delegate:
                subprocess.run(delegate + [raw], check=False, timeout=10)
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
