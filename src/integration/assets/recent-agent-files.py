#!/usr/bin/env python3
"""Capture explicit successful agent file writes without retaining tool payloads."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
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
    # Keep fd digits attached to a redirect distinguishable from ordinary
    # arguments such as the 2 in `echo 2 > report.txt`.
    command = re.sub(r"(?<!\S)(\d+)(?=(?:>>|>))", r"HERDR_FD_\1", command)
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
        if token in (">&", ">>&") and i + 1 < len(tokens):
            target = tokens[i + 1]
            # A numeric target is fd duplication (`2>&1`), not a file.
            if not target.isdigit() and target != "/dev/null":
                result.append(target)
            i += 2
            continue
        if token in (">", ">>", "&>", "&>>") and i + 1 < len(tokens):
            target = tokens[i + 1]
            previous = tokens[i - 1] if i else ""
            # Ignore stderr redirection; only attached fd digits are marked.
            if not previous.startswith("HERDR_FD_2") and target != "/dev/null":
                result.append(target)
            i += 2
            continue
        segment.append(token)
        i += 1
    if segment:
        segments.append(segment)

    for words in segments:
        # `cd`, subshells, and wrapper commands can change the meaning or
        # destination of relative paths. Do not guess through them.
        if any(word in ("cd", "(", ")", "()") or word in ("bash", "sh", "zsh", "env", "sudo", "command", "xargs") for word in words):
            return []
        command_index = 0
        while command_index < len(words) and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", words[command_index]):
            command_index += 1
        if command_index >= len(words):
            continue
        base = os.path.basename(words[command_index])
        args = words[command_index + 1:]
        if base in ("tee", "touch"):
            result.extend(arg for arg in args if not arg.startswith("-") and arg != "/dev/null")
        elif base in ("cp", "mv", "install"):
            positional = [arg for arg in args if not arg.startswith("-") and arg != "/dev/null"]
            if len(positional) >= 2:
                result.append(positional[-1])
    return result


def _shell_uses_unsupported_context(command):
    if not isinstance(command, str):
        return False
    return bool(re.search(r"(?:^|[;&|]\s*)(?:cd|bash|sh|zsh|env|sudo|command|xargs)\b|[()]", command))


def _shell_has_candidate_write(command):
    """Identify likely writes before trusting a result, without treating reads as gaps."""
    if _shell_destinations(command):
        return True
    if not isinstance(command, str) or len(command) > MAX_INPUT:
        return False
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|<>")
        tokens = list(lexer)
    except ValueError:
        return False
    write_commands = {"tee", "touch", "cp", "mv", "install"}
    segment = []
    for token in tokens + [";"]:
        if token in (";", "&&", "||", "|"):
            command_index = 0
            while command_index < len(segment) and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", segment[command_index]):
                command_index += 1
            if command_index < len(segment):
                command = os.path.basename(segment[command_index])
                args = [arg for arg in segment[command_index + 1:] if not arg.startswith("-") and arg != "/dev/null"]
                if command in ("tee", "touch") and args:
                    return True
                if command in ("cp", "mv", "install") and len(args) >= 2:
                    return True
            segment = []
        else:
            segment.append(token)
    # A destination in an unsupported wrapper or directory context remains a
    # candidate, but is never resolved to a file by this conservative check.
    for index, token in enumerate(tokens[:-1]):
        if token in (">&", ">>&") and tokens[index + 1].isdigit():
            continue
        if token in (">", ">>", "&>", "&>>") and tokens[index + 1] != "/dev/null":
            previous = tokens[index - 1] if index else ""
            if not previous.startswith("HERDR_FD_2"):
                return True
    return False


def _is_supported_write_tool(tool_name):
    name = str(tool_name or "").casefold()
    return any(word in name for word in ("write", "edit", "patch", "create"))


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
    elif provider == "cursor" and event == "postToolUse":
        tool = str(payload.get("tool_name") or "postToolUse")
        if tool.casefold() != "shell":
            return [], tool
        tool_input = payload.get("tool_input")
        command = tool_input.get("command") if isinstance(tool_input, dict) else None
        if not _shell_has_candidate_write(command):
            return [], tool
        if not _cursor_shell_succeeded(payload.get("tool_output")):
            return [], "gap:" + tool
        if isinstance(tool_input, dict):
            workdir = tool_input.get("workdir")
            if isinstance(workdir, str) and Path(workdir).is_absolute():
                cwd = workdir
        paths.extend(_shell_destinations(command))
        if not paths and _shell_uses_unsupported_context(command):
            return [], "gap:" + tool
    elif provider == "codex" and event == "PostToolUse":
        tool_name = payload.get("tool_name")
        tool_input = payload.get("tool_input")
        tool = str(tool_name or "PostToolUse")
        normalized_name = str(tool_name or "").casefold()
        command = (
            (tool_input.get("command") or tool_input.get("cmd"))
            if isinstance(tool_input, dict)
            else tool_input
        )
        candidate = _is_supported_write_tool(tool_name)
        if normalized_name in ("bash", "exec", "exec_command", "shell"):
            candidate = candidate or _shell_has_candidate_write(command)
        if not _codex_tool_succeeded(payload.get("tool_response")):
            return [], ("gap:" if candidate else "") + tool
        paths.extend(_direct_paths(tool_name, tool_input))
        if normalized_name in ("bash", "exec", "exec_command", "shell"):
            if isinstance(tool_input, dict):
                workdir = tool_input.get("workdir")
                if isinstance(workdir, str) and Path(workdir).is_absolute():
                    cwd = workdir
            paths.extend(_shell_destinations(command))
            if not paths and _shell_uses_unsupported_context(command):
                return [], "gap:" + tool
    else:
        return [], "unsupported:" + str(event or "unknown")

    normalized = []
    for raw_path in paths:
        path = _normalize_path(raw_path, cwd)
        if path and path not in normalized and _is_regular_file(path):
            normalized.append(path)
    return normalized, tool


def _response_object(value):
    if isinstance(value, dict):
        return value
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
        except (ValueError, TypeError):
            return None
        return parsed if isinstance(parsed, dict) else None
    return None


def _cursor_shell_succeeded(value):
    response = _response_object(value)
    code = response.get("exitCode") if response else None
    return type(code) is int and code == 0


def _codex_tool_succeeded(value):
    # A string here is rendered command output, not provider-owned metadata.
    # In particular, status-looking text or JSON stdout cannot prove success.
    response = value if isinstance(value, dict) else None
    if response is None:
        return False
    if response.get("success") is False or str(response.get("status", "")).casefold() in ("failed", "failure", "error"):
        return False
    for key in ("exit_code", "exitCode"):
        code = response.get(key)
        if type(code) is int:
            return code == 0
    return response.get("success") is True or str(response.get("status", "")).casefold() in ("success", "succeeded")


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
            and isinstance(record.get("time_ns"), int)
            and isinstance(record.get("agent"), str)
            and isinstance(record.get("provenance"), str)
            and isinstance(record.get("event_id"), str)
            and (record.get("gap") or (isinstance(record.get("path"), str) and os.path.isabs(record["path"])))
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
            if item.get("path") != record.get("path") and item["event_id"] != record["event_id"]
        ]
        records.append(record)
        records.sort(key=lambda item: (item["time_ns"], item.get("path", "")), reverse=True)
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
        if tool.startswith("gap:"):
            _mark_gap(provider, event, tool[4:], state_dir, now_ns)
        return 0
    journal = Path(state_dir) / JOURNAL_NAME
    captured = 0
    timestamp = int(now_ns if now_ns is not None else time.time_ns())
    event_id = payload.get("tool_use_id") or payload.get("generation_id") or uuid.uuid4().hex
    for index, path in enumerate(paths):
        if os.path.abspath(path) == os.path.abspath(journal):
            continue
        try:
            _bounded_upsert(journal, {
                "path": path,
                "time_ns": timestamp + index,
                "agent": provider,
                "provenance": f"{provider}.{event}.{tool}"[:120],
                "event_id": f"{event_id}:{hashlib.sha256(path.encode('utf-8')).hexdigest()[:24]}"[:160],
            })
        except OSError:
            continue
        captured += 1
    return captured


def _mark_gap(provider, event, tool, state_dir, now_ns=None):
    timestamp = int(now_ns if now_ns is not None else time.time_ns())
    journal = Path(state_dir) / JOURNAL_NAME
    try:
        _bounded_upsert(journal, {
            "time_ns": timestamp,
            "agent": provider,
            "provenance": f"{provider}.{event}.{tool}.unverified"[:120],
            "event_id": f"gap:{provider}:{event}:{uuid.uuid4().hex}",
            "gap": "unverified or failed write event",
        })
    except OSError:
        pass


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
