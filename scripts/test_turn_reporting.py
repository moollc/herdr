"""Installed hooks exercised by fake native agents in an isolated home."""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import unittest


class NativeTurnHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(tempfile.mkdtemp(prefix="herdr-turn-hooks-", dir="/private/tmp"))
        cls.env = dict(os.environ)
        cls.env.update(HOME=str(cls.root), XDG_CONFIG_HOME=str(cls.root / "config"),
                       HERDR_CONFIG_PATH=str(cls.root / "herdr.toml"), SPEAK="0",
                       CODEX_HOME=str(cls.root / ".codex"), CLAUDE_CONFIG_DIR=str(cls.root / ".claude"),
                       CURSOR_CONFIG_DIR=str(cls.root / ".cursor"), HERDR_ENV="1",
                       HERDR_PANE_ID="p_41", HERDR_SOCKET_PATH=str(cls.root / "hook.sock"))
        cls.binary = os.environ["HERDR_TEST_BINARY"]
        for kind in ("claude", "codex", "cursor"):
            (cls.root / ("." + kind)).mkdir()
        cls.user_hook = "printf keep"
        for kind, name, event, entry in [
            ("claude", "settings.json", "Stop", {"hooks": [{"type": "command", "command": cls.user_hook}]}),
            ("cursor", "hooks.json", "stop", {"command": cls.user_hook}),
        ]:
            (cls.root / ("." + kind) / name).write_text(json.dumps({"version": 1, "hooks": {event: [entry]}}))
        cls.delegate = cls.root / "delegate.json"
        notify = [shutil.which("python3"), "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text(sys.argv[2])", str(cls.delegate)]
        cls.original_config = "# custom notification\nnotify = " + json.dumps(notify) + "\nmodel = 'test'\n"
        (cls.root / ".codex/config.toml").write_text(cls.original_config)
        for kind in ("claude", "codex", "cursor"):
            for _ in range(2):
                subprocess.run([cls.binary, "integration", "install", kind], env=cls.env, check=True, capture_output=True)
        cls.requests = []
        cls.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        cls.listener.bind(cls.env["HERDR_SOCKET_PATH"])
        cls.listener.listen()
        cls.listener.settimeout(0.2)
        cls.stopped = threading.Event()
        def serve():
            while not cls.stopped.is_set():
                try:
                    client, _ = cls.listener.accept()
                except socket.timeout:
                    continue
                with client:
                    data = b""
                    while b"\n" not in data:
                        data += client.recv(4096)
                    cls.requests.append(json.loads(data))
                    client.sendall(b'{"result":{"type":"ok"}}\n')
        cls.thread = threading.Thread(target=serve)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.stopped.set()
        cls.thread.join(2)
        cls.listener.close()
        # Retain the synthetic home for failure investigation. No real settings.

    def invoke(self, kind, event, action):
        before = len(self.requests)
        payload = {"hook_event_name": event, "session_id": "fake-native-session",
                   "prompt": "SECRET_PROMPT", "last_assistant_message": "SECRET_RESPONSE"}
        folder = self.root / ("." + kind)
        script = folder / "herdr-agent-state.sh"
        if kind == "claude":
            script = folder / "hooks/herdr-agent-state.sh"
        subprocess.run(["sh", str(script), action], input=json.dumps(payload), text=True,
                       env=self.env, check=True, capture_output=True)
        requests = self.requests[before:]
        self.assertEqual(len(requests), 1)
        self.assertEqual(requests[0]["method"], "pane.report_turn")
        self.assertEqual(requests[0]["params"]["agent"], kind)
        self.assertNotIn("SECRET", json.dumps(requests))
        return requests[0]["params"]

    def test_claude_fake_agent(self):
        self.assertEqual(self.invoke("claude", "UserPromptSubmit", "started")["event"], "started")
        self.assertEqual(self.invoke("claude", "Stop", "ended")["event"], "ended")
        before = len(self.requests)
        self.invoke_ignored("claude", "SubagentStop", "ended")
        self.assertEqual(len(self.requests), before)

    def test_cursor_fake_agent(self):
        for event, action, expected in [("sessionStart", "session", "ready"),
                                        ("beforeSubmitPrompt", "started", "started"), ("stop", "ended", "ended")]:
            self.assertEqual(self.invoke("cursor", event, action)["event"], expected)

    def test_codex_fake_agent_and_notification_delegate(self):
        before = len(self.requests)
        payload = json.dumps({"type": "agent-turn-complete", "thread-id": "session", "turn-id": "turn-one",
                              "input-messages": ["SECRET_PROMPT"], "last-assistant-message": "SECRET_RESPONSE"})
        subprocess.run(["sh", str(self.root / ".codex/herdr-agent-state.sh"), "notify", payload],
                       env=self.env, stdin=subprocess.DEVNULL, check=True, capture_output=True)
        requests = self.requests[before:]
        self.assertEqual(requests[0]["params"]["event"], "ended")
        self.assertEqual(requests[0]["params"]["turn_id"], "turn-one")
        self.assertNotIn("SECRET", json.dumps(requests))
        self.assertEqual(self.delegate.read_text(), payload)
        self.assertEqual(self.invoke("codex", "UserPromptSubmit", "started")["event"], "started")

    def invoke_ignored(self, kind, event, action):
        subprocess.run(["sh", str(self.root / ".claude/hooks/herdr-agent-state.sh"), action],
                       input=json.dumps({"hook_event_name": event, "agent_id": "subagent"}),
                       text=True, env=self.env, check=True, capture_output=True)

    def test_z_uninstall_preserves_existing_user_hooks(self):
        for kind in ("claude", "codex", "cursor"):
            subprocess.run([self.binary, "integration", "uninstall", kind], env=self.env, check=True, capture_output=True)
        self.assertEqual((self.root / ".codex/config.toml").read_text().split("[features]")[0], self.original_config + "\n")
        claude = json.loads((self.root / ".claude/settings.json").read_text())
        cursor = json.loads((self.root / ".cursor/hooks.json").read_text())
        self.assertEqual(claude["hooks"]["Stop"][0]["hooks"][0]["command"], self.user_hook)
        self.assertEqual(cursor["hooks"]["stop"], [{"command": self.user_hook}])
        self.assertNotIn("UserPromptSubmit", claude["hooks"])
        self.assertNotIn("beforeSubmitPrompt", cursor["hooks"])
        status = subprocess.run([self.binary, "integration", "status"], env=self.env, check=True, capture_output=True, text=True)
        self.assertIn("cursor: not installed", status.stdout)


if __name__ == "__main__":
    unittest.main()
