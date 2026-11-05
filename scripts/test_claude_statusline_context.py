"""Production Claude status line reporting block: context readings and explicit unknowns.

Runs the reporting block of the installed-asset source against an isolated socket.
Nothing is installed; the delegate command and shell cleanup are not run.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest

ASSET = Path(__file__).resolve().parents[1] / 'src/integration/assets/claude/herdr-statusline.sh'


def reporting_block():
    source = ASSET.read_text(encoding='utf-8')
    return source.split("python3 - <<'PY' || true\n", 1)[1].split('\nPY\nfi', 1)[0]


class StatuslineContextTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix='herdr-statusline-', dir='/tmp'))
        self.socket_path = self.root / 'meter.sock'
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(self.socket_path))
        self.server.listen(5)
        self.server.settimeout(5)

    def tearDown(self):
        self.server.close()

    def report(self, payload):
        """The params the block sends for one status line input."""
        input_path = self.root / 'input.json'
        input_path.write_text(json.dumps(payload), encoding='utf-8')
        received = []
        errors = []

        def serve():
            try:
                client, _ = self.server.accept()
                with client:
                    data = b''
                    while not data.endswith(b'\n'):
                        data += client.recv(4096)
                    received.append(json.loads(data))
                    client.sendall(b'{"result":{"type":"ok"}}\n')
            except Exception as error:  # reported by the assertion below
                errors.append(str(error))

        thread = threading.Thread(target=serve)
        thread.start()
        environment = dict(os.environ, HERDR_ENV='1', HERDR_PANE_ID='p_fixture',
                           HERDR_SOCKET_PATH=str(self.socket_path),
                           HERDR_STATUSLINE_INPUT_FILE=str(input_path))
        subprocess.run(['python3', '-c', reporting_block()], env=environment, check=True, timeout=10)
        thread.join(10)
        self.assertFalse(errors, errors)
        self.assertEqual(len(received), 1)
        return received[0]['params']

    def test_numeric_reading_is_sent_with_its_session(self):
        params = self.report({'session_id': 's1', 'context_window': {'used_percentage': 92}})
        self.assertEqual((params['context_used_percent'], params['session_id']), (92, 's1'))
        self.assertNotIn('context_unknown', params)

    def test_null_after_a_reading_is_an_explicit_unknown_for_that_session(self):
        params = self.report({'session_id': 's1', 'context_window': {'used_percentage': None}})
        self.assertEqual(params['session_id'], 's1')
        self.assertTrue(params['context_unknown'])
        self.assertNotIn('context_used_percent', params)

    def test_missing_reading_is_the_same_explicit_unknown(self):
        params = self.report({'session_id': 's1'})
        self.assertEqual(params['session_id'], 's1')
        self.assertTrue(params['context_unknown'])
        self.assertNotIn('context_used_percent', params)

    def test_valid_zero_is_a_reading(self):
        params = self.report({'session_id': 's1', 'context_window': {'used_percentage': 0}})
        self.assertEqual(params['context_used_percent'], 0)
        self.assertNotIn('context_unknown', params)

    def test_out_of_range_and_boolean_readings_are_unknown(self):
        for value in (150, -1, True):
            params = self.report({'session_id': 's1', 'context_window': {'used_percentage': value}})
            self.assertTrue(params.get('context_unknown'), value)
            self.assertNotIn('context_used_percent', params, value)

    def test_without_a_session_nothing_context_related_is_sent(self):
        params = self.report({'context_window': {'used_percentage': 92}})
        for key in ('session_id', 'context_used_percent', 'context_unknown'):
            self.assertNotIn(key, params)

    def test_session_title_patch_still_carries_the_title(self):
        params = self.report({'session_id': 's1', 'session_name': 'Sample work'})
        self.assertEqual(params['title'], 'Sample work')
        self.assertTrue(params['context_unknown'])


if __name__ == '__main__':
    unittest.main()
