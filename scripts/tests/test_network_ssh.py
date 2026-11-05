from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
POLICY = SOURCE.split("<<'SSH_POLICY_PY'\n", 1)[1].split('\nSSH_POLICY_PY', 1)[0]


@unittest.skipUnless(Path('/usr/sbin/sshd').exists() and shutil.which('ssh-keygen'), 'OpenSSH tools required')
class SshPolicyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.parts = self.root / 'sshd_config.d'
        self.parts.mkdir()
        self.key = self.root / 'hostkey'
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(self.key)], check=True)
        self.config = self.root / 'sshd_config'
        self.config.write_text(f'HostKey {self.key}\nInclude {self.parts}/*.conf\nAuthorizedKeysFile .ssh/authorized_keys\n')
        self.policy = self.parts / '00-tako-network.conf'

    def install(self):
        code = POLICY.replace("Path('/etc/ssh')", f'Path({str(self.root)!r})')
        code = code.replace("['/usr/sbin/sshd', '-t']", "['/usr/sbin/sshd', '-t', '-f', str(config_path)]")
        return subprocess.run(['python3', '-I', '-'], input=code, text=True, capture_output=True, timeout=10)

    def effective(self, user='tako', address='127.0.0.1', host='localhost'):
        result = subprocess.run(['/usr/sbin/sshd', '-T', '-f', str(self.config), '-C',
                                 f'user={user},host={host},addr={address}'], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return dict(line.split(' ', 1) for line in result.stdout.splitlines() if ' ' in line)

    def test_address_host_and_user_match_key_file_policy_is_preserved(self):
        (self.parts / 'admin.conf').write_text('''Match User tako Address 203.0.113.0/24
    AuthorizedKeysFile /etc/admin-keys/remote-tako
Match User tako Host secure.example
    AuthorizedKeysFile /etc/admin-keys/host-tako
Match User root
    AuthorizedKeysFile /etc/admin-keys/root
Match all
''')
        contexts = [('tako', '203.0.113.1', 'localhost'), ('tako', '127.0.0.1', 'secure.example'),
                    ('tako', '127.0.0.1', 'localhost'), ('root', '203.0.113.1', 'secure.example')]
        before = [self.effective(*context) for context in contexts]
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        for context, prior in zip(contexts, before):
            after = self.effective(*context)
            self.assertEqual(after['authorizedkeysfile'], prior['authorizedkeysfile'])
            expected = '/usr/local/libexec/tako/network-authorized-keys %u' if context[0] == 'tako' else 'none'
            self.assertEqual(after['authorizedkeyscommand'], expected)
        self.assertNotIn('AuthorizedKeysFile', self.policy.read_text())

    def test_existing_authorized_keys_command_in_any_include_fails_before_changes(self):
        for directive in ('AuthorizedKeysCommand /etc/custom-key-provider %u',
                          'AuthorizedKeysCommand="/etc/custom-key-provider"'):
            with self.subTest(directive=directive):
                (self.parts / 'remote.conf').write_text('Match User tako Address 203.0.113.0/24\n    ' + directive + '\n    AuthorizedKeysCommandUser nobody\n')
                before = self.config.read_bytes()
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('existing AuthorizedKeysCommand', result.stderr)
                self.assertEqual(self.config.read_bytes(), before)
                self.assertFalse(self.policy.exists())

    def test_refresh_is_idempotent_and_does_not_sample_only_loopback(self):
        self.assertEqual(self.install().returncode, 0)
        before = self.config.read_bytes(), self.policy.read_bytes()
        self.assertEqual(self.install().returncode, 0)
        self.assertEqual((self.config.read_bytes(), self.policy.read_bytes()), before)

    def test_old_generated_key_file_override_is_replaced_by_command(self):
        self.policy.write_text('Match User tako\n    AuthorizedKeysFile .ssh/authorized_keys /etc/ssh/tako-network/authorized_keys\nMatch all\n')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.effective()['authorizedkeysfile'], '.ssh/authorized_keys')

    def test_foreign_policy_file_is_never_overwritten(self):
        self.policy.write_text('Match User tako\n    AuthorizedKeysCommand /etc/custom %u\nMatch all\n')
        before = self.config.read_bytes(), self.policy.read_bytes()
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual((self.config.read_bytes(), self.policy.read_bytes()), before)

    def test_failed_sshd_validation_restores_original_files(self):
        self.config.write_text(self.config.read_text() + 'NotAnSshDirective yes\n')
        before = self.config.read_bytes()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('original policy restored', result.stderr)
        self.assertEqual(self.config.read_bytes(), before)
        self.assertFalse(self.policy.exists())


if __name__ == '__main__':
    unittest.main()
