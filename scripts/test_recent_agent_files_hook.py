#!/usr/bin/env python3
"""Focused mock-payload tests for the Codex and Cursor recent-file hooks."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

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
        self.output = self.base / "outside cwd" / "résumé page.html"
        self.output.parent.mkdir()

    def records(self):
        path = self.state / hook.JOURNAL_NAME
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run_hook(self, provider, event, payload):
        env = dict(os.environ, HERDR_ENV="1", HERDR_PANE_ID="fixture-pane", HERDR_STATE_DIR=str(self.state))
        return subprocess.run(
            [os.sys.executable, str(ASSET), provider, event],
            input=json.dumps(payload), text=True, capture_output=True, env=env, check=True,
        )

    def test_cursor_direct_edit_outside_cwd_includes_untracked_unicode_path_and_no_content(self):
        self.output.write_text("harmless fixture content; must not enter the journal")
        payload = {"file_path": str(self.output), "edits": [{"new_string": "private"}]}
        self.run_hook("cursor", "afterFileEdit", payload)
        row = self.records()[0]
        self.assertEqual(row["path"], str(self.output))
        self.assertEqual(row["agent"], "cursor")
        self.assertEqual(row["provenance"], "cursor.afterFileEdit.afterFileEdit")
        journal = (self.state / hook.JOURNAL_NAME).read_text()
        self.assertNotIn("harmless fixture content", journal)
        self.assertNotIn("private", journal)

    def test_codex_successful_shell_output_outside_cwd_is_registered_by_post_tool_event(self):
        target = self.base / "shell output.txt"
        command = "printf 'tiny safe fixture' > " + subprocess.list2cmdline([str(target)])
        subprocess.run(command, shell=True, cwd=self.cwd, check=True)
        payload = {
            "tool_name": "exec",
            "tool_use_id": "fixture-tool-use",
            "cwd": str(self.cwd),
            "tool_input": {"command": command},
            "tool_response": {"exitCode": 0},
        }
        self.run_hook("codex", "PostToolUse", payload)
        self.assertEqual(self.records()[0]["path"], str(target))
        self.assertEqual(self.records()[0]["provenance"], "codex.PostToolUse.exec")
        self.assertNotIn("tiny safe fixture", (self.state / hook.JOURNAL_NAME).read_text())

    def test_other_process_write_alone_does_not_create_an_agent_event(self):
        self.output.write_text("process-only fixture")
        self.assertEqual(self.records(), [])

    def test_dedup_order_restart_and_journal_self_event_filter(self):
        first = self.base / "first.txt"
        second = self.base / "second.txt"
        first.write_text("1")
        second.write_text("2")
        self.assertEqual(hook.capture("cursor", "afterFileEdit", {"file_path": str(first)}, self.state, now_ns=100), 1)
        self.assertEqual(hook.capture("codex", "PostToolUse", {
            "tool_name": "apply_patch", "tool_use_id": "second-write",
            "cwd": str(self.cwd), "tool_input": {"command": "*** Begin Patch\n*** Add File: " + str(second) + "\n+safe\n*** End Patch"},
            "tool_response": {"exitCode": 0},
        }, self.state, now_ns=200), 1)
        self.assertEqual(hook.capture("cursor", "afterFileEdit", {"file_path": str(first)}, self.state, now_ns=300), 1)
        journal = self.state / hook.JOURNAL_NAME
        before = journal.read_text()
        self.assertEqual(hook.capture("cursor", "afterFileEdit", {"file_path": str(journal)}, self.state, now_ns=400), 0)
        self.assertEqual(journal.read_text(), before)

        reloaded_spec = importlib.util.spec_from_file_location("recent_agent_files_reloaded", ASSET)
        reloaded = importlib.util.module_from_spec(reloaded_spec)
        reloaded_spec.loader.exec_module(reloaded)
        rows = [json.loads(line) for line in (self.state / reloaded.JOURNAL_NAME).read_text().splitlines()]
        self.assertEqual([row["path"] for row in rows], [str(first), str(second)])
        self.assertEqual(rows[0]["time_ns"], 300)

    def test_unsupported_or_failed_hook_events_are_not_claimed_as_writes(self):
        self.output.write_text("untouched by supported file event")
        cursor_shell = {"command": "touch " + str(self.output), "output": "done", "duration": 1}
        self.assertEqual(hook.capture("cursor", "afterShellExecution", cursor_shell, self.state), 0)
        self.assertEqual(hook.capture("codex", "PostToolUseFailure", {
            "tool_name": "exec", "cwd": str(self.cwd),
            "tool_input": {"command": "touch " + str(self.output)},
        }, self.state), 0)
        self.assertEqual(self.records(), [])

    def test_bounded_journal_keeps_newest_records_and_marks_lost_history(self):
        old_limit = hook.MAX_RECORDS
        hook.MAX_RECORDS = 2
        try:
            for index in range(3):
                output = self.base / f"result-{index}.txt"
                output.write_text("small fixture")
                self.assertEqual(hook.capture(
                    "cursor", "afterFileEdit", {"file_path": str(output)}, self.state,
                    now_ns=100 + index,
                ), 1)
        finally:
            hook.MAX_RECORDS = old_limit
        rows = self.records()
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["time_ns"], 102)
        self.assertTrue(rows[-1]["truncated"])


if __name__ == "__main__":
    unittest.main()
