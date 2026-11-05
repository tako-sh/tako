import hashlib
import os
import pwd
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
NATIVE = SOURCE[SOURCE.index('install_native_network() {'):SOURCE.index('install_setcap_tool() {')]


class NativeInstallerTests(unittest.TestCase):
    def install(self, *, provider='native', source=True, digest=True, packaged=False, download=False):
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
install_native_corrosion() { printf 'install verified corrosion\\n'; }
download_file() { [ "$TEST_DOWNLOAD" = 1 ] || return 1; cp "$TEST_HELPER" "$2"; }
chown() { :; }
python3() { cat >/dev/null; }
id() { printf '1001\\n'; }
fake_sshd() {
  [ "$1" != -T ] || printf 'authorizedkeysfile .ssh/authorized_keys /etc/admin-keys/tako %s/etc/ssh/tako-network/authorized_keys\\n' "$TEST_ROOT"
}
''' + script + '\ninstall_native_network\n', text=True, capture_output=True, timeout=10,
            env={**os.environ, 'TEST_ROOT': str(root), 'TAKO_VPN': provider,
                 'TEST_DOWNLOAD': '1' if download else '0',
                 'TEST_HELPER': str(ROOT / 'scripts/tako-network-helper.py'),
                 'TAKO_REPO_OWNER': 'tako-sh', 'TAKO_REPO_NAME': 'tako', 'TAKO_RELEASE_TAG': 'latest',
                 'TAKO_USER': 'tako', 'TAKO_HOME': '/opt/tako', 'SERVICE_MANAGER': 'none',
                 'tmp_extract': str(root / 'extract'),
                 'TAKO_NETWORK_HELPER_SOURCE': str(local) if source else '',
                 'TAKO_NETWORK_HELPER_SHA256': hashlib.sha256(payload).hexdigest() if digest else ''})
        return root, result

    def test_bootstrap_installs_verified_helper_and_independent_boot_unit(self):
        root, result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('packages wireguard-tools iproute2 nftables python3 openssh-server', result.stdout)
        self.assertIn('install verified corrosion', result.stdout)
        self.assertEqual((root / 'usr/local/libexec/tako/tako-network-helper.py').read_text(), '# local helper fixture\n')
        unit = (root / 'etc/systemd/system/tako-network@.service').read_text()
        self.assertIn('%i apply', unit)
        self.assertNotIn('ExecStop', unit)
        self.assertNotIn('PartOf=tako-server', unit)
        self.assertIn('Corrosion service hook prepared', result.stdout)
        self.assertFalse((root / 'usr/local/libexec/tako/corrosion').exists())
        command = (root / 'usr/local/libexec/tako/network-authorized-keys').read_text()
        self.assertIn("sys.argv[1:] != ['tako']", command)
        self.assertIn("network.read_file(fd, 'authorized_keys', private=False)", command)

    def test_bootstrap_accepts_helper_in_verified_archive(self):
        _, result = self.install(source=False, packaged=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_default_download_is_pinned_to_this_helper(self):
        root, result = self.install(source=False, download=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((root / 'usr/local/libexec/tako/tako-network-helper.py').read_bytes(),
                         (ROOT / 'scripts/tako-network-helper.py').read_bytes())

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
            self.assertNotIn('install verified corrosion', result.stdout)
            self.assertFalse((root / 'usr/local/libexec/tako/tako-network-helper.py').exists())

    def test_runtime_cannot_regain_net_admin_after_setuid(self):
        runtime = SOURCE[SOURCE.index('install_systemd_service_unit() {'):]
        for line in runtime.splitlines():
            if line.startswith(('AmbientCapabilities=', 'CapabilityBoundingSet=')):
                self.assertNotIn('NET_ADMIN', line)
                self.assertNotIn('$TAKO_NETWORK_BOUNDING_CAPABILITY', line)
        for line in SOURCE.splitlines():
            if 'NOPASSWD:' in line:
                self.assertNotIn('tako-network-helper', line)
        root, result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = (root / 'etc/systemd/system/tako-network-helper.service').read_text()
        self.assertIn('User=root', unit)
        self.assertIn('CAP_NET_ADMIN', unit)
        self.assertIn(' --serve', unit)
        socket = (root / 'etc/systemd/system/tako-network-helper.socket').read_text()
        self.assertIn('SocketMode=0660', socket)
        self.assertIn('SocketGroup=tako', socket)
        self.assertIn('ListenStream=/run/tako-network-helper.sock', socket)

    def test_installer_refresh_preserves_admin_and_management_keys(self):
        block = SOURCE[SOURCE.index('# Install authorized_keys for SSH'):SOURCE.index('# ── Server config')]
        with tempfile.TemporaryDirectory(dir=ROOT) as tmp:
            root = Path(tmp)
            (root / '.ssh').mkdir()
            admin = root / '.ssh/authorized_keys'
            admin.write_text('ssh-ed25519 existing-admin\n')
            management = root / 'management-authorized-keys'
            management.write_text('ssh-ed25519 existing-management\n')
            script = 'tako_home_dir() { printf "%s\\n" "$TEST_ROOT"; }\nneed_cmd() { command -v "$1" >/dev/null; }\n' + block
            for _ in range(2):
                result = subprocess.run(['sh', '-eu'], input=script, text=True, capture_output=True,
                    env={**os.environ, 'TEST_ROOT': str(root), 'TAKO_USER': pwd.getpwuid(os.getuid()).pw_name,
                         'TAKO_HOME': str(root),
                         'TAKO_SSH_PUBKEY': 'ssh-ed25519 new-admin',
                         'TAKO_MANAGEMENT_AUTH_KEYS': str(management)}, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('existing-admin', admin.read_text())
            self.assertIn('existing-management', management.read_text())
            self.assertEqual(admin.read_text().count('new-admin'), 1)
            self.assertEqual(management.read_text().count('new-admin'), 1)


if __name__ == '__main__':
    unittest.main()
