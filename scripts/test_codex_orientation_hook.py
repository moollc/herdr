"""Production SessionStart hook tests: temporary sockets/homes, no Codex client."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest

HOOK = Path(__file__).resolve().parents[1] / 'src/integration/assets/codex/herdr-agent-state.sh'


class HookTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='hrd-hook-', dir='/tmp')
        self.root = Path(self.tmp.name)
        self.path = self.root / 'server.sock'
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(self.path))
        self.server.listen(20)
        self.server.settimeout(.1)
        self.stop = threading.Event()
        self.requests = []
        self.errors = []
        self.reply = lambda request: {'result': {'context': 'Managed by Herdr', 'nonce': 'term_1:offer'}} if request['method'] == 'pane.report_agent_session' else {'result': {'type': 'ok'}}
        self.thread = threading.Thread(target=self.serve)
        self.thread.start()

    def tearDown(self):
        self.stop.set()
        self.thread.join(timeout=3)
        self.server.close()
        self.tmp.cleanup()
        self.assertFalse(self.errors, self.errors)
        self.assertFalse(self.thread.is_alive())

    def serve(self):
        while not self.stop.is_set():
            try:
                client, _ = self.server.accept()
            except socket.timeout:
                continue
            try:
                with client:
                    client.settimeout(2)
                    data = b''
                    while not data.endswith(b'\n'):
                        data += client.recv(4096)
                    request = json.loads(data)
                    self.requests.append(request)
                    response = self.reply(request)
                    if response is not None:
                        client.sendall((response if isinstance(response, bytes) else (json.dumps(response) + '\n').encode()))
            except BrokenPipeError:
                pass  # deliberately lost response/timeout
            except Exception as error:
                self.errors.append(str(error))

    def hook(self, session='session', source='startup', env=None):
        environment = dict(os.environ, HERDR_ENV='1', HERDR_SOCKET_PATH=str(self.path),
                           HERDR_PANE_ID='p_1', TMPDIR=str(self.root))
        if env:
            environment.update(env)
        result = subprocess.run(['sh', str(HOOK), 'session'],
                                input=json.dumps({'session_id': session, 'source': source}),
                                env=environment, capture_output=True, text=True, timeout=9)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def test_cross_process_lost_ack_emits_once_and_retries_ack(self):
        self.reply = lambda request: {'result': {'context': 'Managed by Herdr', 'nonce': 'term_1:offer'}} if request['method'] == 'pane.report_agent_session' else {'error': {'message': 'lost ack'}}
        first = self.hook()
        second = self.hook()
        self.assertIn('additionalContext', first.stdout)
        self.assertEqual(second.stdout, '')
        self.assertIn('not model receipt', first.stderr)
        self.assertEqual(sum(r['method'] == 'pane.ack_orientation' for r in self.requests), 6)

    def test_invalid_ack_result_is_reported_and_keeps_receipt(self):
        self.reply = lambda request: {'result': {'context': 'Managed by Herdr', 'nonce': 'term_1:offer'}} if request['method'] == 'pane.report_agent_session' else {'result': {}}
        self.assertIn('invalid emission acknowledgement', self.hook().stderr)
        self.assertEqual(self.hook().stdout, '')

    def test_concurrent_stale_offers_emit_once_even_after_ack(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            outputs = list(pool.map(lambda _: self.hook(), range(4)))
        self.assertEqual(sum('additionalContext' in result.stdout for result in outputs), 1)
        self.assertEqual(sum(r['method'] == 'pane.ack_orientation' for r in self.requests), 4)

    def test_receipt_is_scoped_to_conversation_offer_and_server(self):
        self.assertTrue(self.hook().stdout)
        self.assertTrue(self.hook(session='new-session').stdout)
        self.reply = lambda request: {'result': {'context': 'Managed by Herdr', 'nonce': 'term_1:new-offer'}} if request['method'] == 'pane.report_agent_session' else {'result': {'type': 'ok'}}
        self.assertTrue(self.hook().stdout)
        self.assertEqual(self.hook().stdout, '')
        old_path = self.path
        self.path = self.root / 'alias.sock'
        self.path.symlink_to(old_path)
        self.assertEqual(self.hook().stdout, '', 'same server through symlink is same receipt')

    def test_offer_retries_lost_and_malformed_responses_before_emission(self):
        def reply(request):
            if len(self.requests) == 1:
                return None
            if len(self.requests) == 2:
                return b'bad json\n'
            return {'result': {'context': 'Managed by Herdr', 'nonce': 'term_1:offer'}} if request['method'] == 'pane.report_agent_session' else {'result': {'type': 'ok'}}
        self.reply = reply
        result = self.hook()
        self.assertIn('additionalContext', result.stdout)
        self.assertEqual(len(self.requests), 4)
        self.assertEqual(result.stderr, '')

    def test_offer_timeout_retries_and_reports_failure(self):
        def reply(_request):
            self.stop.wait(.6)
            return None
        self.reply = reply
        result = self.hook()
        self.assertEqual(result.stdout, '')
        self.assertIn('startup delivery failed', result.stderr)
        self.assertEqual(len(self.requests), 3)

    def test_missing_socket_environment_session_and_old_server_are_visible(self):
        for env, session in [({'HERDR_SOCKET_PATH': ''}, 'session'),
                             ({'HERDR_PANE_ID': ''}, 'session'),
                             ({'HERDR_SOCKET_PATH': str(self.root / 'absent')}, 'session'),
                             ({}, '')]:
            with self.subTest(env=env, session=session):
                result = self.hook(session=session, env=env)
                self.assertEqual(result.stdout, '')
                self.assertIn('herdr orientation:', result.stderr)
        self.reply = lambda _request: {'result': {}}
        self.assertIn('incompatible server', self.hook().stderr)
        self.assertEqual(self.hook(source='resume').stderr, '')

    def test_malformed_and_error_response_fail_visibly_without_emission(self):
        for reply in [b'[]\n', {'error': {'message': 'old server'}}, {'result': {'context': 'missing nonce'}}]:
            self.reply = lambda _request: reply
            result = self.hook()
            self.assertEqual(result.stdout, '')
            self.assertIn('herdr orientation:', result.stderr)

    def test_receipt_write_failure_does_not_emit_or_ack(self):
        (self.root / ('herdr-codex-orientation-' + str(os.getuid()))).write_text('blocked')
        result = self.hook()
        self.assertEqual(result.stdout, '')
        self.assertIn('receipt directory', result.stderr)
        self.assertEqual(len(self.requests), 1)

    def test_uncertain_emission_is_visible_and_never_reemitted_or_acknowledged(self):
        server = self.path.stat()
        identity = [str(self.path.resolve()), server.st_dev, server.st_ino, 'p_1', 'session', 'term_1:offer']
        key = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
        directory = self.root / ('herdr-codex-orientation-' + str(os.getuid()))
        directory.mkdir(mode=0o700)
        (directory / (key + '.json')).write_text(json.dumps({'state': 'emission_uncertain'}))
        result = self.hook()
        self.assertEqual(result.stdout, '')
        self.assertIn('emission is uncertain', result.stderr)
        self.assertEqual(len(self.requests), 1)


if __name__ == '__main__':
    unittest.main()
