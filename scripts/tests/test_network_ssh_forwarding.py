"""Exercise the generated network SSH key options against a real sshd.

A key line OpenSSH cannot parse is ignored without an error, which would lock
every network administrator out, so the options are checked end to end.
"""
import getpass
import importlib.util
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest


SOURCE = Path(__file__).resolve().parents[1] / 'tako-network-helper.py'
spec = importlib.util.spec_from_file_location('network_helper_forwarding', SOURCE)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


def free_port(host='127.0.0.1'):
    with socket.socket() as probe:
        probe.bind((host, 0))
        return probe.getsockname()[1]


def outbound_address():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.connect(('192.0.2.1', 9))
        return probe.getsockname()[0]


class Banner:
    """A TCP listener that greets each connection, so a forward is observable."""

    def __init__(self, host):
        self.socket = socket.socket()
        self.socket.bind((host, 0))
        self.socket.listen()
        self.port = self.socket.getsockname()[1]
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                connection, _ = self.socket.accept()
            except OSError:
                return
            with connection:
                connection.sendall(b'banner\n')

    def close(self):
        self.socket.close()


@unittest.skipUnless(Path('/usr/sbin/sshd').exists() and shutil.which('ssh-keygen') and shutil.which('ssh'),
                     'OpenSSH tools required')
class ForwardingPolicyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for name in ('client', 'host'):
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root / name)], check=True)
        public = (root / 'client.pub').read_text().split()
        state = {'networks': {'ab' * 16: {'peers': [
            {'address': '127.0.0.1/128', 'ssh_public_key': f'{public[0]} {public[1]}'}]}}}
        authorized = root / 'authorized_keys'
        authorized.write_text(helper.ssh_authorizations(state))
        port = free_port()
        (root / 'known_hosts').write_text(f'[127.0.0.1]:{port} ' + (root / 'host.pub').read_text())
        self.sshd = subprocess.Popen(
            ['/usr/sbin/sshd', '-D', '-e', '-f', '/dev/null', '-h', str(root / 'host'), '-p', str(port),
             '-o', 'ListenAddress=127.0.0.1', '-o', 'PasswordAuthentication=no', '-o', 'KbdInteractiveAuthentication=no',
             '-o', 'StrictModes=no', '-o', 'UsePAM=no', '-o', 'PermitRootLogin=prohibit-password',
             '-o', f'AuthorizedKeysFile={authorized}', '-o', f'PidFile={root / "sshd.pid"}'],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(self.sshd.wait, 5)
        self.addCleanup(self.sshd.terminate)
        deadline = time.monotonic() + 5
        while True:
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=0.2):
                    break
            except OSError:
                self.assertLess(time.monotonic(), deadline, 'sshd did not start')
                time.sleep(0.05)
        self.ssh = ['ssh', '-F', '/dev/null', '-i', str(root / 'client'), '-o', 'IdentitiesOnly=yes',
                    '-o', 'IdentityAgent=none', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
                    '-o', f'UserKnownHostsFile={root / "known_hosts"}', '-p', str(port),
                    f'{getpass.getuser()}@127.0.0.1']

    def forward(self, host, port):
        return subprocess.run([*self.ssh[:-1], '-W', f'{host}:{port}', self.ssh[-1]], stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=20)

    def test_key_grants_a_shell_and_loopback_forwarding_only(self):
        shell = subprocess.run([*self.ssh, 'echo', 'shell-ok'], stdin=subprocess.DEVNULL,
                               capture_output=True, text=True, timeout=20)
        self.assertEqual(shell.stdout.strip(), 'shell-ok', shell.stderr)

        loopback = Banner('127.0.0.1')
        self.addCleanup(loopback.close)
        for host in ('127.0.0.1', 'localhost'):
            with self.subTest(host=host):
                self.assertEqual(self.forward(host, loopback.port).stdout, 'banner\n')

        address = outbound_address()
        if address.startswith('127.'):
            self.skipTest('no non-loopback address to test against')
        other = Banner(address)
        self.addCleanup(other.close)
        with socket.create_connection((address, other.port), timeout=2) as direct:
            self.assertEqual(direct.recv(16), b'banner\n')
        self.assertEqual(self.forward(address, other.port).stdout, '', 'non-loopback forward must be refused')

        remote = subprocess.run([*self.ssh[:-1], '-o', 'ExitOnForwardFailure=yes', '-R',
                                 f'{free_port()}:127.0.0.1:{loopback.port}', self.ssh[-1], 'true'],
                                stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20)
        self.assertNotEqual(remote.returncode, 0, 'remote forwarding must be refused')
        self.assertIn('remote port forwarding failed', remote.stderr)

        agent = subprocess.run([*self.ssh[:-1], '-A', self.ssh[-1], 'printf %s "${SSH_AUTH_SOCK:-none}"'],
                               stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20)
        self.assertEqual(agent.stdout, 'none')


if __name__ == '__main__':
    unittest.main()
