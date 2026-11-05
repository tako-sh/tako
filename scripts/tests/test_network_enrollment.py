import os
from pathlib import Path
import pwd
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
ENROLLMENT = SOURCE.split("<<'ENROLL_PY'\n", 1)[1].split('\nENROLL_PY', 1)[0]


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.data = self.root / 'data'
        self.home = self.root / 'home'
        self.data.mkdir()
        self.home.mkdir()
        self.ssh = self.home / '.ssh'
        self.ssh.mkdir(mode=0o700)
        self.management = self.data / 'management-authorized-keys'
        self.authorized = self.ssh / 'authorized_keys'
        self.management.write_text('ssh-ed25519 existing-management')
        self.authorized.write_text('ssh-ed25519 existing-admin\n')
        self.victim = self.root / 'privileged-helper'
        self.victim.write_text('# privileged file\n')
        self.victim.chmod(0o644)

    def enroll(self, key='ssh-ed25519 new-key'):
        return subprocess.run(['python3', '-I', '-', pwd.getpwuid(os.getuid()).pw_name,
                               str(self.data), str(self.home)], input=ENROLLMENT, text=True,
                              capture_output=True, env={**os.environ, 'TAKO_ENROLL_KEY': key}, timeout=5)

    def test_management_and_ssh_symlinks_leave_target_bytes_mode_and_owner_unchanged(self):
        for target in (self.management, self.authorized):
            with self.subTest(target=target.name):
                original = target.read_bytes()
                target.unlink()
                target.symlink_to(self.victim)
                info = self.victim.stat()
                before = (self.victim.read_bytes(), info.st_mode, info.st_uid, info.st_gid)
                result = self.enroll()
                self.assertNotEqual(result.returncode, 0)
                info = self.victim.stat()
                self.assertEqual((self.victim.read_bytes(), info.st_mode, info.st_uid, info.st_gid), before)
                target.unlink()
                target.write_bytes(original)

    def test_symlinked_ssh_directory_or_ancestor_is_refused(self):
        actual = self.root / 'actual-ssh'
        self.ssh.rename(actual)
        self.ssh.symlink_to(actual, target_is_directory=True)
        before = (actual / 'authorized_keys').read_bytes()
        self.assertNotEqual(self.enroll().returncode, 0)
        self.assertEqual((actual / 'authorized_keys').read_bytes(), before)
        self.ssh.unlink()
        actual.rename(self.ssh)
        actual_home = self.root / 'actual-home'
        self.home.rename(actual_home)
        self.home.symlink_to(actual_home, target_is_directory=True)
        self.assertNotEqual(self.enroll().returncode, 0)
        self.assertEqual((actual_home / '.ssh/authorized_keys').read_bytes(), before)

    def test_hardlinks_and_nonregular_files_are_refused_without_blocking(self):
        for target in (self.management, self.authorized):
            original = target.read_bytes()
            target.unlink()
            os.link(self.victim, target)
            self.assertNotEqual(self.enroll().returncode, 0)
            self.assertEqual(self.victim.read_text(), '# privileged file\n')
            target.unlink()
            os.mkfifo(target, 0o600)
            self.assertNotEqual(self.enroll().returncode, 0)
            target.unlink()
            target.write_bytes(original)

    def test_refresh_appends_once_and_never_touches_neighboring_ssh_files(self):
        neighbor = self.ssh / 'other-key'
        neighbor.symlink_to(self.victim)
        for _ in range(2):
            result = self.enroll()
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.management.read_text(), 'ssh-ed25519 existing-management\nssh-ed25519 new-key\n')
        self.assertEqual(self.authorized.read_text(), 'ssh-ed25519 existing-admin\nssh-ed25519 new-key\n')
        self.assertEqual(self.authorized.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.management.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.ssh.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.victim.stat().st_mode & 0o777, 0o644)
        self.assertTrue(neighbor.is_symlink())

    def test_new_files_are_created_without_recursive_chown(self):
        self.authorized.unlink()
        self.ssh.rmdir()
        self.management.unlink()
        result = self.enroll()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.authorized.read_text(), 'ssh-ed25519 new-key\n')
        self.assertEqual(self.management.read_text(), 'ssh-ed25519 new-key\n')

    def test_multiline_key_and_writable_directory_fail_before_enrollment(self):
        before = self.management.read_bytes(), self.authorized.read_bytes()
        self.assertNotEqual(self.enroll('ssh-ed25519 key\nssh-ed25519 injected').returncode, 0)
        self.ssh.chmod(0o777)
        self.assertNotEqual(self.enroll().returncode, 0)
        self.assertEqual((self.management.read_bytes(), self.authorized.read_bytes()), before)


if __name__ == '__main__':
    unittest.main()
