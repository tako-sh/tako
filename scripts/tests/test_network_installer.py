import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
NATIVE = SOURCE[SOURCE.index('install_native_network() {'):SOURCE.index('install_setcap_tool() {')]


class NativeInstallerTests(unittest.TestCase):
    def install(self, *, provider='native', source=True, digest=True, packaged=False):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for directory in ('usr/local/libexec/tako', 'etc/ssh/tako-network', 'etc/systemd/system',
                          'var/lib/tako-network-helper', 'opt/tako/networks', 'extract/scripts'):
            (root / directory).mkdir(parents=True)
        (root / 'etc/ssh/sshd_config').write_text('Include /etc/ssh/sshd_config.d/*.conf\n')
        payload = b'# local helper fixture\n'
        local = root / 'source.py'
        local.write_bytes(payload)
        if packaged:
            (root / 'extract/scripts/tako-network-helper.py').write_bytes(payload)
        script = NATIVE
        # Relocate privileged paths; command fakes never alter this machine's services.
        for prefix in ('/usr/local/', '/etc/', '/var/lib/', '/opt/tako/'):
            script = script.replace(prefix, str(root) + prefix)
        script = script.replace('[ "$TAKO_USER:$TAKO_HOME" = tako:/opt/tako ]', '[ "$TAKO_USER" = tako ]')
        script = script.replace('/usr/sbin/sshd', 'fake_sshd')
        result = subprocess.run(['sh', '-eu'], input='''
need_cmd() { case "$1" in apt-get|wg|ip|nft|python3) return 0;; *) return 1;; esac; }
install_pkgs() { printf 'packages %s\\n' "$*"; }
chown() { :; }
python3() { cat >/dev/null; }
id() { printf '1001\\n'; }
fake_sshd() {
  [ "$1" != -T ] || printf 'authorizedkeysfile .ssh/authorized_keys /etc/admin-keys/tako %s/etc/ssh/tako-network/authorized_keys\\n' "$TEST_ROOT"
}
''' + script + '\ninstall_native_network\n', text=True, capture_output=True, timeout=10,
            env={**os.environ, 'TEST_ROOT': str(root), 'TAKO_VPN': provider,
                 'TAKO_USER': 'tako', 'TAKO_HOME': '/opt/tako', 'SERVICE_MANAGER': 'none',
                 'tmp_extract': str(root / 'extract'),
                 'TAKO_NETWORK_HELPER_SOURCE': str(local) if source else '',
                 'TAKO_NETWORK_HELPER_SHA256': hashlib.sha256(payload).hexdigest() if digest else ''})
        return root, result

    def test_bootstrap_installs_verified_helper_and_independent_boot_unit(self):
        root, result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('packages wireguard-tools iproute2 nftables python3 openssh-server', result.stdout)
        self.assertEqual((root / 'usr/local/libexec/tako/tako-network-helper.py').read_text(), '# local helper fixture\n')
        unit = (root / 'etc/systemd/system/tako-network@.service').read_text()
        self.assertIn('%i apply', unit)
        self.assertNotIn('ExecStop', unit)
        self.assertNotIn('PartOf=tako-server', unit)
        self.assertIn('Corrosion service hook prepared', result.stdout)
        self.assertFalse((root / 'usr/local/libexec/tako/corrosion').exists())
        ssh = (root / 'etc/ssh/sshd_config.d/00-tako-network.conf').read_text()
        self.assertIn('.ssh/authorized_keys /etc/admin-keys/tako', ssh)
        self.assertIn('Match User tako', ssh)

    def test_bootstrap_accepts_helper_in_verified_archive(self):
        _, result = self.install(source=False, packaged=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_helper_or_unverified_local_source_fails(self):
        for options in ({'source': False}, {'digest': False}):
            with self.subTest(options=options):
                root, result = self.install(**options)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((root / 'usr/local/libexec/tako/tako-network-helper').exists())

    def test_other_providers_do_not_install_native_dependencies(self):
        for provider in ('netbird', 'tailscale'):
            root, result = self.install(provider=provider)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn('packages', result.stdout)
            self.assertFalse((root / 'usr/local/libexec/tako/tako-network-helper.py').exists())

    def test_runtime_has_no_ambient_net_admin_or_unrestricted_network_sudo(self):
        for line in SOURCE.splitlines():
            if line.startswith('AmbientCapabilities='):
                self.assertNotIn('NET_ADMIN', line)
        policy = next(line for line in SOURCE.splitlines() if 'NOPASSWD:' in line and 'tako-network-helper * apply' in line)
        self.assertIn('tako-network-helper * remove', policy)
        self.assertNotIn('/usr/bin/python', policy)
        self.assertNotIn('/usr/sbin/ip', policy)

    def test_installer_refresh_preserves_admin_and_management_keys(self):
        block = SOURCE[SOURCE.index('# Install authorized_keys for SSH'):SOURCE.index('# ── Server config')]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / '.ssh').mkdir()
            admin = root / '.ssh/authorized_keys'
            admin.write_text('ssh-ed25519 existing-admin\n')
            management = root / 'management-authorized-keys'
            management.write_text('ssh-ed25519 existing-management\n')
            script = 'tako_home_dir() { printf "%s\\n" "$TEST_ROOT"; }\nchown() { :; }\n' + block
            for _ in range(2):
                result = subprocess.run(['sh', '-eu'], input=script, text=True, capture_output=True,
                    env={**os.environ, 'TEST_ROOT': str(root), 'TAKO_USER': 'tako',
                         'TAKO_SSH_PUBKEY': 'ssh-ed25519 new-admin',
                         'TAKO_MANAGEMENT_AUTH_KEYS': str(management)}, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('existing-admin', admin.read_text())
            self.assertIn('existing-management', management.read_text())
            self.assertEqual(admin.read_text().count('new-admin'), 1)
            self.assertEqual(management.read_text().count('new-admin'), 1)


if __name__ == '__main__':
    unittest.main()
