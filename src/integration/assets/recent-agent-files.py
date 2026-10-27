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
PENDING_NAME = "recent-agent-files-pending.json"
MAX_PENDING_EVENTS = 64
MAX_PENDING_BYTES = 512 * 1024
MAX_CANDIDATE_PATHS = 64
PENDING_TTL_NS = 120 * 1_000_000_000
WRITE_MANIFEST_KEY = "herdr_write_manifest"
WRITE_MANIFEST_VERSION = 1


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
    raw_command = command
    command = re.sub(r"(?<!\S)(\d+)(?=(?:>>|>))", r"HERDR_FD_\1", command)
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
    if _shell_uses_unsupported_context(raw_command) and re.search(
        r"(?<!\S)(?!(?:2(?:>>|>)))(?:\d*(?:>>|>)|&>>|&>)(?!&)\s*(?!/dev/null\b)[^\s;&|]+",
        raw_command,
    ):
        return True
    return False


def _is_supported_write_tool(tool_name):
    name = str(tool_name or "").casefold()
    return any(word in name for word in ("write", "edit", "patch", "create", "replace"))


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


NESTED_PATCH_HEADER = re.compile(r"\*\*\* (?:Update File|Add File|Move to): ([^\\\r\n\"'`]+)")


def _nested_patch_paths(tool_input):
    """Find explicit patch headers in a code-mode tool input, without parsing arbitrary outputs."""
    if isinstance(tool_input, dict):
        source = tool_input.get("input") or tool_input.get("command") or tool_input.get("cmd")
    else:
        source = tool_input
    if not isinstance(source, str) or "apply_patch" not in source or len(source) > MAX_INPUT:
        return []
    return [match.strip() for match in NESTED_PATCH_HEADER.findall(source) if match.strip()]


def _manifest_paths(response):
    """Read only the opt-in, versioned write manifest from a tool result."""
    if isinstance(response, str):
        if len(response.encode("utf-8", errors="replace")) > MAX_INPUT:
            return []
        try:
            response = json.loads(response)
        except (ValueError, TypeError):
            return []
    if not isinstance(response, dict):
        return []
    manifest = response.get(WRITE_MANIFEST_KEY)
    if not isinstance(manifest, dict):
        return []
    if type(manifest.get("version")) is not int or manifest.get("version") != WRITE_MANIFEST_VERSION:
        return []
    if type(manifest.get("success")) is not bool:
        return []
    paths = manifest.get("files")
    if not isinstance(paths, list) or not paths or len(paths) > MAX_CANDIDATE_PATHS:
        return []
    if any(not isinstance(path, str) for path in paths):
        return []
    return paths


def _direct_paths(tool_name, tool_input):
    if not isinstance(tool_input, dict):
        return []
    name = str(tool_name or "").casefold()
    if name in ("apply_patch", "applypatch"):
        return _patch_paths(tool_input)
    if not any(word in name for word in ("write", "edit", "patch", "create", "replace")):
        return []
    paths = []
    for key in ("file_path", "path", "target_path", "destination"):
        value = tool_input.get(key)
        if isinstance(value, str):
            paths.append(value)
    return paths


def _normalize_path(raw_path, cwd):
    if not isinstance(raw_path, str) or not raw_path or len(raw_path) > 4096:
        return None
    if "\x00" in raw_path:
        return None
    candidate = Path(raw_path).expanduser()
    if not candidate.is_absolute():
        if not isinstance(cwd, str) or not cwd or not Path(cwd).is_absolute():
            return None
        candidate = Path(cwd) / candidate
    try:
        return str(candidate.resolve(strict=False))
    except OSError:
        return None


def _candidate_paths(provider, event, payload):
    if not isinstance(payload, dict):
        return [], "", False
    cwd = payload.get("cwd")
    paths = []
    tool_name = str(payload.get("tool_name") or event or "unknown")
    normalized_name = tool_name.casefold()
    tool_input = payload.get("tool_input")
    if provider == "cursor" and event in ("afterFileEdit", "afterTabFileEdit"):
        paths.append(payload.get("file_path"))
        candidate = True
    elif provider == "codex" and event in ("PreToolUse", "PostToolUse"):
        command = (tool_input.get("command") or tool_input.get("cmd")) if isinstance(tool_input, dict) else tool_input
        candidate = _is_supported_write_tool(tool_name)
        if normalized_name in ("bash", "exec", "exec_command", "shell"):
            candidate = candidate or _shell_has_candidate_write(command)
            paths.extend(_shell_destinations(command))
        if normalized_name in ("exec", "code_mode"):
            nested = _nested_patch_paths(tool_input)
            if nested:
                candidate = True
                paths.extend(nested)
        paths.extend(_direct_paths(tool_name, tool_input))
        if isinstance(tool_input, dict):
            workdir = tool_input.get("workdir")
            if isinstance(workdir, str) and Path(workdir).is_absolute():
                cwd = workdir
    elif provider == "cursor" and event in ("preToolUse", "postToolUse", "postToolUseFailure"):
        command = tool_input.get("command") if isinstance(tool_input, dict) else None
        candidate = _is_supported_write_tool(tool_name)
        if normalized_name == "shell":
            candidate = candidate or _shell_has_candidate_write(command)
            paths.extend(_shell_destinations(command))
        paths.extend(_direct_paths(tool_name, tool_input))
        if isinstance(tool_input, dict):
            workdir = tool_input.get("cwd") or tool_input.get("workdir")
            if isinstance(workdir, str) and Path(workdir).is_absolute():
                cwd = workdir
    else:
        return [], tool_name, False

    normalized = []
    for raw_path in paths:
        path = _normalize_path(raw_path, cwd)
        if path and path not in normalized and path not in ("/dev/null",):
            normalized.append(path)
    return normalized, tool_name, candidate


def _fingerprint(path):
    try:
        info = os.lstat(path)
    except OSError:
        return None
    if not stat.S_ISREG(info.st_mode):
        return None
    return [info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns, info.st_ctime_ns]


def _event_key(provider, payload):
    event_id = payload.get("tool_use_id") if isinstance(payload, dict) else None
    if not isinstance(event_id, str) or not event_id or len(event_id) > 1024:
        return None
    return hashlib.sha256((provider + "\0" + event_id).encode("utf-8")).hexdigest()


def _pending_update(path, change, *, consume=True):
    """Atomically update short-lived metadata; never persist command or file contents."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock_path = path.with_suffix(path.suffix + ".lock")
    with lock_path.open("a", encoding="utf-8") as lock:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            state = json.loads(path.read_text()) if path.exists() else {}
        except (OSError, ValueError, TypeError):
            state = {}
        if not isinstance(state, dict):
            state = {}
        now = time.time_ns()
        stale = [key for key, item in state.items()
                 if not isinstance(item, dict) or type(item.get("started_ns")) is not int
                 or now - item["started_ns"] > PENDING_TTL_NS]
        for key in stale:
            state.pop(key, None)
        key, entry = change
        matched = None
        if key is not None:
            if entry is None:
                matched = state.pop(key, None) if consume else state.get(key)
            else:
                state[key] = entry
        evicted = 0
        while len(state) > MAX_PENDING_EVENTS:
            oldest = min(state, key=lambda item: state[item].get("started_ns", 0))
            state.pop(oldest, None)
            evicted += 1
        encoded = json.dumps(state, separators=(",", ":")).encode("utf-8")
        while len(encoded) > MAX_PENDING_BYTES and state:
            oldest = min(state, key=lambda item: state[item].get("started_ns", 0))
            state.pop(oldest, None)
            evicted += 1
            encoded = json.dumps(state, separators=(",", ":")).encode("utf-8")
        fd, temporary = tempfile.mkstemp(prefix="recent-agent-files-pending-", dir=path.parent)
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            output.write(encoded.decode("utf-8"))
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    return len(stale) + evicted, matched


def _record_observed(provider, event, tool, event_key, path, state_dir, timestamp):
    journal = Path(state_dir) / JOURNAL_NAME
    state_root = Path(state_dir).resolve(strict=False)
    try:
        canonical = Path(path).resolve(strict=False)
    except OSError:
        return False
    if canonical == journal.resolve(strict=False) or (
        canonical.parent == state_root and canonical.name.startswith("recent-agent-files-")
    ):
        return True
    try:
        _bounded_upsert(journal, {
            "path": str(canonical),
            "time_ns": timestamp,
            "agent": provider,
            "provenance": f"{provider}.{event}.{tool}"[:120],
            "event_id": f"{event_key}:{hashlib.sha256(path.encode('utf-8')).hexdigest()[:24]}"[:160],
        })
        return True
    except OSError:
        return False


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
        record_path = record.get("path")
        records = [
            {key: value for key, value in item.items() if key != "truncated"}
            for item in old_records
            if item["event_id"] != record["event_id"]
            and (not record_path or item.get("path") != record_path)
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
def _is_tool_event(provider, event):
    return (provider == "codex" and event in ("PreToolUse", "PostToolUse")) or (
        provider == "cursor" and event in ("preToolUse", "postToolUse", "postToolUseFailure")
    )


def capture(provider, event, payload, state_dir, *, now_ns=None):
    if provider not in ("codex", "cursor"):
        return 0
    if not isinstance(payload, dict):
        if _is_tool_event(provider, event):
            _mark_gap(provider, event, "unparseable-or-non-object-input", state_dir, now_ns)
        return 0
    paths, tool, candidate = _candidate_paths(provider, event, payload)
    timestamp = int(now_ns if now_ns is not None else time.time_ns())
    if len(paths) > MAX_CANDIDATE_PATHS:
        if candidate:
            _mark_gap(provider, event, "too-many-explicit-paths", state_dir, timestamp)
        return 0
    if provider == "cursor" and event in ("afterFileEdit", "afterTabFileEdit"):
        key = hashlib.sha256((provider + "\0" + str(payload.get("generation_id", ""))).encode()).hexdigest()
        return sum(_record_observed(provider, event, tool, key, path, state_dir, timestamp + index)
                   for index, path in enumerate(paths) if _fingerprint(path) is not None)

    pre_event = event == "PreToolUse" if provider == "codex" else event == "preToolUse"
    post_event = event == "PostToolUse" if provider == "codex" else event in ("postToolUse", "postToolUseFailure")
    if not (pre_event or post_event):
        return 0
    manifest_eligible = provider == "codex" and tool.casefold() in ("bash", "exec_command")
    event_key = _event_key(provider, payload)
    pending = Path(state_dir) / PENDING_NAME
    if pre_event:
        if not candidate and not manifest_eligible:
            return 0
        fingerprints = [{"path": path, "before": _fingerprint(path)} for path in paths]
        if event_key is None or (not fingerprints and (candidate or not manifest_eligible)):
            _mark_gap(provider, event, tool, state_dir, timestamp)
            return 0
        entry = {"started_ns": timestamp, "files": fingerprints, "tool": tool}
        expired, _ = _pending_update(pending, (event_key, entry))
        if expired:
            _mark_gap(provider, event, "stale-or-evicted-pending", state_dir, timestamp)
        return 0

    manifest_paths = (
        _manifest_paths(payload.get("tool_response"))
        if manifest_eligible else []
    )
    if event_key is None:
        if manifest_paths:
            _mark_gap(provider, event, "manifest-missing-event-id:" + tool, state_dir, timestamp)
        elif candidate:
            _mark_gap(provider, event, tool, state_dir, timestamp)
        return 0
    expired, entry = _pending_update(pending, (event_key, None), consume=False)
    if expired:
        _mark_gap(provider, event, "stale-or-evicted-pending", state_dir, timestamp)
    if not isinstance(entry, dict) or not isinstance(entry.get("files"), list):
        if manifest_paths:
            _mark_gap(provider, event, "manifest-pre-event-unavailable:" + tool, state_dir, timestamp)
        elif candidate:
            _mark_gap(provider, event, "unmatched:" + tool, state_dir, timestamp)
        return 0
    if entry.get("tool") != tool:
        _pending_update(pending, (event_key, None))
        _mark_gap(provider, event, "tool-mismatch:" + tool, state_dir, timestamp)
        return 0
    captured = 0
    failed_record = False
    ambiguous = False
    observed = list(entry["files"])
    if manifest_paths:
        cwd = payload.get("cwd")
        tool_input = payload.get("tool_input")
        if isinstance(tool_input, dict):
            workdir = tool_input.get("workdir")
            if isinstance(workdir, str) and Path(workdir).is_absolute():
                cwd = workdir
        known_paths = {item["path"] for item in observed if isinstance(item, dict)}
        for raw_path in manifest_paths:
            path = _normalize_path(raw_path, cwd)
            if path and path not in known_paths:
                observed.append({"path": path, "before": None, "manifest": True})
                known_paths.add(path)
    for index, item in enumerate(observed):
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            failed_record = True
            continue
        after = _fingerprint(item["path"])
        if item.get("manifest") is True and (
            after is None or after[4] <= entry.get("started_ns", timestamp)
        ):
            continue
        if after is None and item.get("before") is not None:
            ambiguous = True
            continue
        if after is None or after == item.get("before"):
            continue
        if not _record_observed(provider, event, tool, event_key, item["path"], state_dir, timestamp + index):
            failed_record = True
        else:
            captured += 1
    if not failed_record:
        _pending_update(pending, (event_key, None))
    if ambiguous:
        _mark_gap(provider, event, "candidate-disappeared-during-tool", state_dir, timestamp)
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


def _state_dir_from_environment(environ=None):
    """Resolve state for a managed pane whose environment predates HERDR_STATE_DIR."""
    env = os.environ if environ is None else environ
    if "HERDR_STATE_DIR" in env:
        return env["HERDR_STATE_DIR"] or None
    if env.get("HERDR_ENV") != "1" or not env.get("HERDR_PANE_ID"):
        return None

    # The socket is injected by Herdr into managed panes. Accept only its
    # standard config/session layout so arbitrary socket overrides cannot
    # redirect capture into a guessed state directory.
    socket_value = env.get("HERDR_SOCKET_PATH", "")
    if not socket_value or not os.path.isabs(socket_value) or Path(socket_value).name != "herdr.sock":
        return None
    try:
        if not stat.S_ISSOCK(os.lstat(socket_value).st_mode):
            return None
    except OSError:
        return None
    socket_parent = Path(socket_value).parent
    if socket_parent.parent.name == "sessions":
        if not socket_parent.name:
            return None
        config_root = socket_parent.parent.parent
    else:
        config_root = socket_parent
    app_dir = config_root.name
    if app_dir not in ("herdr", "herdr-dev"):
        return None
    if "XDG_CONFIG_HOME" in env:
        expected_config_root = Path(env["XDG_CONFIG_HOME"]) / app_dir
    elif "HOME" in env:
        expected_config_root = Path(env["HOME"]) / ".config" / app_dir
    else:
        expected_config_root = Path("/tmp") / app_dir
    if os.path.abspath(config_root) != os.path.abspath(expected_config_root):
        return None

    # Match config::state_dir(): config-path overrides do not change this
    # location; the Herdr app flavor comes from the verified socket layout.
    if "XDG_STATE_HOME" in env:
        return str(Path(env["XDG_STATE_HOME"]) / app_dir)
    if "HOME" in env:
        return str(Path(env["HOME"]) / ".local" / "state" / app_dir)
    return str(Path("/tmp") / (app_dir + "-state"))


def main():
    if os.environ.get("HERDR_ENV") != "1":
        return
    if not os.environ.get("HERDR_PANE_ID"):
        return
    state_dir = _state_dir_from_environment()
    if not state_dir:
        return
    provider = sys.argv[1] if len(sys.argv) > 1 else ""
    event = sys.argv[2] if len(sys.argv) > 2 else ""
    raw = sys.stdin.buffer.read(MAX_INPUT + 1)
    if len(raw) > MAX_INPUT:
        if _is_tool_event(provider, event):
            _mark_gap(provider, event, "oversize-input", state_dir)
        return
    try:
        payload = json.loads(raw) if raw.strip() else {}
    except (ValueError, TypeError):
        if _is_tool_event(provider, event):
            _mark_gap(provider, event, "malformed-input", state_dir)
        return
    capture(provider, event, payload, state_dir)


if __name__ == "__main__":
    main()
