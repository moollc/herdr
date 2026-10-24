#!/usr/bin/env python3
"""Capture explicit successful agent file writes without retaining tool payloads."""
import fcntl
import json
import os
from pathlib import Path
import shlex
import stat
import sys
import tempfile
import time
import uuid

MAX_INPUT = 1024 * 1024
MAX_JOURNAL_BYTES = 512 * 1024
MAX_RECORDS = 500
JOURNAL_NAME = "recent-agent-files.jsonl"


def _is_regular_file(path):
    try:
        return stat.S_ISREG(os.lstat(path).st_mode)
    except OSError:
        return False


def _shell_destinations(command):
    """Recognize explicit write targets without interpreting or running a command."""
    if not isinstance(command, str) or len(command) > MAX_INPUT:
        return []
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|<>")
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return []
    result = []
    segment = []
    segments = []
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token in (";", "&&", "||", "|"):
            if segment:
                segments.append(segment)
                segment = []
            i += 1
            continue
        if token in (">", ">>") and i + 1 < len(tokens):
            target = tokens[i + 1]
            previous = tokens[i - 1] if i else ""
            # Ignore stderr redirection; its destination is not an output file.
            if previous not in ("2", "1", "&"):
                result.append(target)
            i += 2
            continue
        segment.append(token)
        i += 1
    if segment:
        segments.append(segment)

    for words in segments:
        commands = [word for word in words if word not in ("sudo", "env")]
        for index, word in enumerate(commands):
            base = os.path.basename(word)
            if base == "tee":
                args = [arg for arg in commands[index + 1:] if not arg.startswith("-")]
                result.extend(args)
                break
            if base == "touch":
                result.extend(arg for arg in commands[index + 1:] if not arg.startswith("-"))
                break
            if base in ("cp", "mv", "install"):
                args = [arg for arg in commands[index + 1:] if not arg.startswith("-")]
                if len(args) >= 2:
                    result.append(args[-1])
                break
    return result


def _patch_paths(value):
    if isinstance(value, dict):
        value = value.get("command") or value.get("patch") or ""
    if not isinstance(value, str) or len(value) > MAX_INPUT:
        return []
    paths = []
    for line in value.splitlines():
        for marker in ("*** Update File: ", "*** Add File: ", "*** Move to: "):
            if line.startswith(marker):
                paths.append(line[len(marker):].strip())
                break
        else:
            if line.startswith("+++ b/"):
                paths.append(line[6:].strip())
    return paths


def _direct_paths(tool_name, tool_input):
    if not isinstance(tool_input, dict):
        return []
    name = str(tool_name or "").casefold()
    if name in ("apply_patch", "applypatch"):
        return _patch_paths(tool_input)
    if not any(word in name for word in ("write", "edit", "patch", "create")):
        return []
    paths = []
    for key in ("file_path", "path", "target_path", "destination"):
        value = tool_input.get(key)
        if isinstance(value, str):
            paths.append(value)
    return paths


def _normalize_path(raw_path, cwd):
    if not isinstance(raw_path, str) or not raw_path or len(raw_path) > 16 * 1024:
        return None
    if "\x00" in raw_path:
        return None
    candidate = Path(raw_path).expanduser()
    if not candidate.is_absolute():
        if not isinstance(cwd, str) or not cwd or not Path(cwd).is_absolute():
            return None
        candidate = Path(cwd) / candidate
    return os.path.abspath(os.fspath(candidate))


def _event_paths(provider, event, payload):
    if not isinstance(payload, dict):
        return [], ""
    cwd = payload.get("cwd")
    paths = []
    if provider == "cursor" and event == "afterFileEdit":
        paths.append(payload.get("file_path"))
        tool = "afterFileEdit"
    elif provider == "cursor" and event == "afterTabFileEdit":
        paths.append(payload.get("file_path"))
        tool = "afterTabFileEdit"
    elif provider == "cursor" and event == "afterShellExecution":
        # Cursor's documented post-shell payload has no exit status or cwd.
        # Do not infer success or resolve relative targets from hook cwd.
        tool = "afterShellExecution-unsupported"
        return [], tool
    elif provider == "codex" and event == "PostToolUse":
        tool_name = payload.get("tool_name")
        tool_input = payload.get("tool_input")
        tool = str(tool_name or "PostToolUse")
        paths.extend(_direct_paths(tool_name, tool_input))
        normalized_name = str(tool_name or "").casefold()
        if normalized_name in ("bash", "exec", "exec_command", "shell"):
            command = (
                (tool_input.get("command") or tool_input.get("cmd"))
                if isinstance(tool_input, dict)
                else tool_input
            )
            paths.extend(_shell_destinations(command))
    else:
        return [], "unsupported:" + str(event or "unknown")

    normalized = []
    for raw_path in paths:
        path = _normalize_path(raw_path, cwd)
        if path and path not in normalized and _is_regular_file(path):
            normalized.append(path)
    return normalized, tool


def _read_journal(path):
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        return []
    if len(raw) > MAX_JOURNAL_BYTES:
        raw = raw[-MAX_JOURNAL_BYTES:]
    records = []
    for line in raw.splitlines()[-MAX_RECORDS:]:
        try:
            record = json.loads(line)
        except (ValueError, TypeError):
            continue
        if (
            isinstance(record, dict)
            and isinstance(record.get("path"), str)
            and os.path.isabs(record["path"])
            and isinstance(record.get("time_ns"), int)
            and isinstance(record.get("agent"), str)
            and isinstance(record.get("provenance"), str)
            and isinstance(record.get("event_id"), str)
        ):
            records.append(record)
    return records


def _bounded_upsert(journal, record):
    journal.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        os.chmod(journal.parent, 0o700)
    except OSError:
        pass
    lock_path = journal.with_suffix(journal.suffix + ".lock")
    with lock_path.open("a", encoding="utf-8") as lock:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        old_records = _read_journal(journal)
        history_truncated = any(item.get("truncated") is True for item in old_records)
        records = [
            {key: value for key, value in item.items() if key != "truncated"}
            for item in old_records
            if item["path"] != record["path"] and item["event_id"] != record["event_id"]
        ]
        records.append(record)
        records.sort(key=lambda item: (item["time_ns"], item["path"]), reverse=True)
        if len(records) > MAX_RECORDS:
            history_truncated = True
        records = records[:MAX_RECORDS]
        if records and history_truncated:
            records[-1]["truncated"] = True
        encoded = b"".join(
            (json.dumps(item, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
            for item in records
        )
        while len(encoded) > MAX_JOURNAL_BYTES and records:
            history_truncated = True
            records.pop()
            if records:
                records[-1]["truncated"] = True
            encoded = b"".join(
                (json.dumps(item, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
                for item in records
            )
        fd, temporary = tempfile.mkstemp(prefix="recent-agent-files-", dir=journal.parent)
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as output:
            output.write(encoded)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, journal)
        dir_fd = os.open(journal.parent, os.O_RDONLY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
def capture(provider, event, payload, state_dir, *, now_ns=None):
    if provider not in ("codex", "cursor") or not isinstance(payload, dict):
        return 0
    paths, tool = _event_paths(provider, event, payload)
    if not paths:
        return 0
    journal = Path(state_dir) / JOURNAL_NAME
    captured = 0
    timestamp = int(now_ns if now_ns is not None else time.time_ns())
    event_id = payload.get("tool_use_id") or payload.get("generation_id")
    for index, path in enumerate(paths):
        if os.path.abspath(path) == os.path.abspath(journal):
            continue
        try:
            _bounded_upsert(journal, {
                "path": path,
                "time_ns": timestamp + index,
                "agent": provider,
                "provenance": f"{provider}.{event}.{tool}"[:120],
                "event_id": f"{event_id or uuid.uuid4().hex}:{index}"[:160],
            })
        except OSError:
            continue
        captured += 1
    return captured


def main():
    if os.environ.get("HERDR_ENV") != "1":
        return
    if not os.environ.get("HERDR_PANE_ID") or not os.environ.get("HERDR_STATE_DIR"):
        return
    provider = sys.argv[1] if len(sys.argv) > 1 else ""
    event = sys.argv[2] if len(sys.argv) > 2 else ""
    raw = sys.stdin.buffer.read(MAX_INPUT + 1)
    if len(raw) > MAX_INPUT:
        return
    try:
        payload = json.loads(raw) if raw.strip() else {}
    except (ValueError, TypeError):
        return
    capture(provider, event, payload, os.environ["HERDR_STATE_DIR"])


if __name__ == "__main__":
    main()
