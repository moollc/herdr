#!/usr/bin/env python3
"""Replay real Codex and Cursor hook shapes through the installed helper entrypoint."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ASSET = Path(__file__).parents[1] / "src/integration/assets/recent-agent-files.py"
spec = importlib.util.spec_from_file_location("recent_agent_files", ASSET)
hook = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hook)


class RecentAgentFilesHookTests(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="herdr-recent-agent-files-test-"))
        self.cwd = self.base / "workspace"
        self.cwd.mkdir()
        self.state = self.base / "herdr-state"
        self.outside = self.base / "outside cwd"
        self.outside.mkdir()

    def records(self):
        path = self.state / hook.JOURNAL_NAME
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run_hook(self, provider, event, payload):
        env = dict(os.environ, HERDR_ENV="1", HERDR_PANE_ID="fixture-pane", HERDR_STATE_DIR=str(self.state))
        return subprocess.run(
            [sys.executable, str(ASSET), provider, event],
            input=json.dumps(payload), text=True, capture_output=True, env=env, check=True,
        )

    def codex(self, tool, tool_id, command_or_args, *, before=True, response="tool returned text"):
        payload = {"tool_name": tool, "tool_use_id": tool_id, "cwd": str(self.cwd),
                   "tool_input": command_or_args}
        if before:
            self.run_hook("codex", "PreToolUse", payload)
        payload["tool_response"] = response
        return payload, self.run_hook("codex", "PostToolUse", payload)

    def test_codex_bash_creation_outside_cwd_uses_real_pre_post_id_and_string_response(self):
        path = self.outside / "shell output.txt"
        command = "printf harmless > '" + str(path) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "bash-event-opaque", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        pending = (self.state / hook.PENDING_NAME).read_text()
        self.assertNotIn(command, pending)
        self.assertNotIn("harmless", pending)
        self.assertNotIn("bash-event-opaque", pending)
        path.write_text("harmless fixture content")
        self.run_hook("codex", "PostToolUse", {**pre, "tool_response": "Process exited with code 0"})
        self.assertEqual(self.records()[0]["path"], str(path.resolve()))
        journal = (self.state / hook.JOURNAL_NAME).read_text()
        self.assertNotIn("harmless fixture content", journal)
        self.assertNotIn(command, journal)

    def test_codex_apply_patch_creation_is_observed_despite_unstructured_response(self):
        path = self.cwd / "patched.txt"
        command = "*** Begin Patch\n*** Add File: patched.txt\n+fixture\n*** End Patch"
        pre = {"tool_name": "apply_patch", "tool_use_id": "patch-event", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        path.write_text("fixture")
        self.run_hook("codex", "PostToolUse", {**pre, "tool_response": "unstructured result"})
        self.assertEqual([row["path"] for row in self.records() if row.get("path")], [str(path.resolve())])

    def test_failed_codex_tool_that_changed_file_is_recorded_from_observed_change(self):
        path = self.cwd / "partial.txt"
        command = "echo partial > '" + str(path) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "failed-but-changed", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        path.write_text("partial")
        self.run_hook("codex", "PostToolUse", {**pre, "tool_response": "failed: nonzero status"})
        self.assertEqual([row["path"] for row in self.records() if row.get("path")], [str(path.resolve())])

    def test_failed_or_success_labeled_nochange_and_preexisting_file_do_not_create_write_or_gap(self):
        path = self.cwd / "preexisting.txt"
        path.write_text("same content")
        for tool_id, response in (("nochange-failed", "command failed"),
                                  ("nochange-forged", '{"success":true}')):
            command = "tee '" + str(path) + "'"
            pre = {"tool_name": "Bash", "tool_use_id": tool_id, "cwd": str(self.cwd),
                   "tool_input": {"command": command}}
            self.run_hook("codex", "PreToolUse", pre)
            self.run_hook("codex", "PostToolUse", {**pre, "tool_response": response})
        self.assertEqual(self.records(), [])

    def test_candidate_deleted_during_tool_is_a_gap_not_a_file_write(self):
        path = self.cwd / "removed.txt"
        path.write_text("before")
        command = "touch '" + str(path) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "candidate-disappeared", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        hook.capture("codex", "PreToolUse", pre, self.state)
        with patch.object(hook, "_fingerprint", return_value=None):
            hook.capture("codex", "PostToolUse", {**pre, "tool_response": "tool result"}, self.state)
        self.assertFalse(any(row.get("path") == str(path) for row in self.records()))
        self.assertTrue(any(row.get("gap") for row in self.records()))

    def test_only_explicit_candidate_path_is_compared_not_other_files_in_window(self):
        target = self.cwd / "named.txt"
        unrelated = self.cwd / "unrelated.txt"
        command = "echo ok > '" + str(target) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "one-path-only", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        unrelated.write_text("other actor")
        self.run_hook("codex", "PostToolUse", {**pre, "tool_response": "unstructured"})
        self.assertEqual(self.records(), [])

    def test_cursor_generic_write_pre_post_and_failure_events_capture_outside_cwd_path(self):
        path = self.outside / "cursor editor.txt"
        pre = {"tool_name": "Write", "tool_use_id": "cursor-write-id", "cwd": str(self.cwd),
               "tool_input": {"file_path": str(path), "content": "must not persist"}}
        self.run_hook("cursor", "preToolUse", pre)
        path.write_text("harmless fixture")
        self.run_hook("cursor", "postToolUseFailure", {**pre, "tool_output": "write failed after partial change"})
        self.assertEqual(self.records()[0]["path"], str(path.resolve()))
        journal = (self.state / hook.JOURNAL_NAME).read_text()
        self.assertNotIn("must not persist", journal)
        self.assertNotIn("harmless fixture", journal)

    def test_cursor_shell_uses_pre_tool_input_cwd_and_does_not_trust_output(self):
        path = self.outside / "cursor shell.txt"
        command = "echo safe > '" + str(path) + "'"
        pre = {"tool_name": "Shell", "tool_use_id": "cursor-shell-id", "cwd": str(self.cwd),
               "tool_input": {"command": command, "cwd": str(self.cwd)}}
        self.run_hook("cursor", "preToolUse", pre)
        path.write_text("safe")
        self.run_hook("cursor", "postToolUse", {**pre, "tool_output": "not JSON success metadata"})
        self.assertEqual(self.records()[0]["path"], str(path.resolve()))

    def test_unmatched_post_records_gap_but_never_claims_candidate_path(self):
        path = self.cwd / "mismatch.txt"
        command = "echo x > '" + str(path) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "pre-id", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        path.write_text("x")
        self.run_hook("codex", "PostToolUse", {**pre, "tool_use_id": "different-id", "tool_response": "done"})
        self.assertFalse(any(row.get("path") == str(path) for row in self.records()))
        self.assertTrue(any(row.get("gap") for row in self.records()))

    def test_expired_pending_event_is_replaced_and_reported_as_gap(self):
        path = self.cwd / "stale.txt"
        command = "echo x > '" + str(path) + "'"
        pre = {"tool_name": "Bash", "tool_use_id": "stale-event", "cwd": str(self.cwd),
               "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", pre)
        pending_path = self.state / hook.PENDING_NAME
        state = json.loads(pending_path.read_text())
        key = next(iter(state))
        state[key]["started_ns"] = time.time_ns() - hook.PENDING_TTL_NS - 1
        pending_path.write_text(json.dumps(state))
        second = {"tool_name": "Bash", "tool_use_id": "next-event", "cwd": str(self.cwd),
                  "tool_input": {"command": command}}
        self.run_hook("codex", "PreToolUse", second)
        self.assertNotIn(key, json.loads(pending_path.read_text()))
        self.assertTrue(any(row.get("gap") for row in self.records()))
        self.assertFalse(any(row.get("path") == str(path) for row in self.records()))

    def test_cursor_after_file_edit_legacy_explicit_path_remains_supported(self):
        path = self.outside / "cursor explicit.txt"
        path.write_text("safe")
        self.run_hook("cursor", "afterFileEdit", {"file_path": str(path), "generation_id": "gen-a"})
        self.assertEqual(self.records()[0]["path"], str(path.resolve()))

    def test_read_only_shell_redirections_and_command_arguments_are_not_candidates(self):
        cases = (("ls -la 2>/dev/null", []), ("ls -la 2>&1 | head", []),
                 ("grep foo file.txt > /dev/null", []), ("grep -r cp src", []),
                 ("echo 2 > report.txt", ["report.txt"]), ("echo x 1> report.txt", ["report.txt"]),
                 ("echo x 2> error.log", []), ("echo x &> all.log", ["all.log"]),
                 ("echo x &>> all.log", ["all.log"]))
        for command, expected in cases:
            with self.subTest(command=command):
                self.assertEqual(hook._shell_destinations(command), expected)
                if not expected:
                    self.assertFalse(hook._shell_has_candidate_write(command))
                    self.run_hook("codex", "PreToolUse", {"tool_name": "Bash", "tool_use_id": command,
                                   "cwd": str(self.cwd), "tool_input": {"command": command}})
        self.assertEqual(self.records(), [])

    def test_unresolved_cd_or_wrapper_context_never_claims_file(self):
        for command in ("cd elsewhere && echo x > out.txt", "bash -c 'echo x > out.txt'"):
            self.assertFalse(hook._shell_destinations(command))
            self.assertTrue(hook._shell_has_candidate_write(command))
            self.run_hook("codex", "PreToolUse", {"tool_name": "Bash", "tool_use_id": command,
                           "cwd": str(self.cwd), "tool_input": {"command": command}})
        self.assertTrue(any(row.get("gap") for row in self.records()))
        self.assertFalse(any(row.get("path") for row in self.records()))

    def test_journal_is_bounded_deduplicated_and_self_events_are_ignored(self):
        for index in range(hook.MAX_RECORDS + 5):
            path = self.cwd / f"file-{index}.txt"
            path.write_text("safe")
            self.run_hook("cursor", "afterFileEdit", {"file_path": str(path), "generation_id": str(index)})
        rows = self.records()
        self.assertEqual(len(rows), hook.MAX_RECORDS)
        self.assertTrue(any(row.get("truncated") for row in rows))
        journal = self.state / hook.JOURNAL_NAME
        self.run_hook("cursor", "afterFileEdit", {"file_path": str(journal), "generation_id": "self"})
        self.assertEqual(len(self.records()), hook.MAX_RECORDS)

    def test_pending_events_are_bounded_and_eviction_is_reported_as_gap(self):
        for index in range(hook.MAX_PENDING_EVENTS + 5):
            path = self.cwd / f"pending-{index}.txt"
            payload = {"tool_name": "Write", "tool_use_id": f"opaque-{index}", "cwd": str(self.cwd),
                       "tool_input": {"file_path": str(path)}}
            hook.capture("codex", "PreToolUse", payload, self.state)
        pending = json.loads((self.state / hook.PENDING_NAME).read_text())
        self.assertLessEqual(len(pending), hook.MAX_PENDING_EVENTS)
        self.assertLessEqual((self.state / hook.PENDING_NAME).stat().st_size, hook.MAX_PENDING_BYTES)
        self.assertTrue(any(row.get("gap") for row in self.records()))
        self.assertNotIn("opaque-", (self.state / hook.PENDING_NAME).read_text())


if __name__ == "__main__":
    unittest.main()
