import json
import struct
import unittest
from unittest.mock import patch

from scripts.tests.test_network_helper import helper, ID


class Connection:
    def __init__(self, uid=1001, chunks=None):
        self.uid = uid
        self.chunks = list(chunks or [])
        self.sent = b''
        self.reads = 0

    def settimeout(self, timeout):
        self.timeout = timeout

    def getsockopt(self, level, name, length):
        assert (level, name, length) == (helper.socket.SOL_SOCKET, helper.socket.SO_PEERCRED, 12)
        return struct.pack('3i', 123, self.uid, 1001)

    def recv(self, size):
        self.reads += 1
        if not self.chunks:
            return b''
        chunk = self.chunks.pop(0)
        if len(chunk) > size:
            self.chunks.insert(0, chunk[size:])
        return chunk[:size]

    def sendall(self, data):
        self.sent += data


class NetworkIpcTests(unittest.TestCase):
    def dispatch(self, frame, uid=1001):
        connection = Connection(uid, [frame])
        with patch.object(helper, 'operation', return_value={'ok': True}) as operation:
            helper.handle_connection(connection, 1001)
        return json.loads(connection.sent), operation, connection

    def test_peer_credentials_gate_dispatch_before_reading(self):
        result, operation, connection = self.dispatch(f'{ID} apply\n'.encode(), uid=2002)
        self.assertFalse(result['ok'])
        self.assertEqual(result['error'], 'unauthorized local caller')
        self.assertEqual(connection.reads, 0)
        operation.assert_not_called()

    def test_tako_and_operator_can_request_only_fixed_operations(self):
        for uid in (0, 1001):
            for action in ('apply', 'remove'):
                with self.subTest(uid=uid, action=action):
                    result, operation, _ = self.dispatch(f'{ID} {action}\n'.encode(), uid)
                    self.assertTrue(result['ok'])
                    operation.assert_called_once_with([ID, action])

    def test_bounded_protocol_rejects_injection_extra_requests_and_trailing_bytes(self):
        for frame in (f'{ID} apply'.encode(), f'{ID} shell\n'.encode(),
                      f'{ID} apply /tmp/secret\n'.encode(), f'{ID} apply\n\n'.encode(),
                      f'{ID} apply\n{ID} remove\n'.encode(), b'../etc apply\n', b'x' * 1000,
                      f'{ID.upper()} apply\n'.encode(), b'\xff\n'):
            with self.subTest(frame=frame[:45]):
                result, operation, _ = self.dispatch(frame)
                self.assertFalse(result['ok'])
                operation.assert_not_called()

    def test_partial_reads_require_eof_then_reconcile_once(self):
        connection = Connection(chunks=[ID[:10].encode(), ID[10:].encode(), b' remove', b'\n'])
        with patch.object(helper, 'operation', return_value={'ok': True}) as operation:
            helper.handle_connection(connection, 1001)
        operation.assert_called_once_with([ID, 'remove'])
        self.assertTrue(json.loads(connection.sent)['ok'])

    def test_errors_are_json_without_input_or_subprocess_secrets(self):
        connection = Connection(chunks=[f'{ID} apply\n'.encode()])
        with patch.object(helper, 'operation', side_effect=ValueError('private_key=secret')):
            helper.handle_connection(connection, 1001)
        self.assertFalse(json.loads(connection.sent)['ok'])
        self.assertNotIn(b'secret', connection.sent)

    def test_daemon_refuses_start_without_pid1_activation(self):
        with patch.object(helper.os, 'geteuid', return_value=0), patch.dict(helper.os.environ, {}, clear=True):
            with self.assertRaisesRegex(helper.Rejected, 'systemd socket activation required'):
                helper.serve()


if __name__ == '__main__':
    unittest.main()
