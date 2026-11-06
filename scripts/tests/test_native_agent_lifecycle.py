import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
CAPABILITIES = SOURCE[SOURCE.index('ensure_privileged_bind_capability() {'):
                      SOURCE.index('\nif is_enabled "$TAKO_RESTART_SERVICE" &&')]
MAINTENANCE = SOURCE.split('cat > "$helper_dir/service" <<\'EOF\'\n', 1)[1].split('\nEOF\n', 1)[0]


class NativeAgentLifecycleTests(unittest.TestCase):
    def test_native_binary_gets_no_file_capabilities(self):
        for provider in ('native', 'tailscale', 'netbird'):
            for manager in ('systemd', 'none'):
                with self.subTest(provider=provider, manager=manager):
                    result = subprocess.run(['sh', '-eu'], input='''
need_cmd() { return 0; }
setcap() { printf 'setcap %s\\n' "$*"; }
''' + CAPABILITIES + '\nensure_privileged_bind_capability\n',
                        env={**os.environ, 'TAKO_VPN': provider, 'SERVICE_MANAGER': manager,
                             'TAKO_SERVER_CAPABILITIES': 'cap_net_bind_service,cap_setuid,cap_setgid,cap_kill'},
                        text=True, capture_output=True, timeout=5)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual('setcap ' in result.stdout, provider != 'native')

    def maintenance(self, action, provider='native'):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        for directory in ('usr/local/bin', 'usr/local/libexec/tako', 'etc/systemd/system',
                          'etc/tako', 'etc/sudoers.d', 'opt/tako', 'run/tako'):
            (root / directory).mkdir(parents=True)
        (root / 'usr/local/libexec/tako/vpn-provider').write_text(provider + '\n')
        (root / 'usr/local/libexec/tako/install-settings').write_text(
            f'tako\n{root}/opt/tako\n{root}/run/tako/tako.sock\n')
        (root / 'usr/local/bin/tako-server.prev').write_text('previous binary\n')
        (root / 'etc/systemd/system/tako-network-agent.service').write_text('agent\n')
        script = MAINTENANCE
        for prefix in ('/usr/local/', '/etc/', '/opt/', '/run/'):
            script = script.replace(prefix, str(root) + prefix)
        result = subprocess.run(['sh', '-s', '--', action], input='''
systemctl() { printf 'systemctl %s\\n' "$*"; }
setcap() { printf 'setcap %s\\n' "$*"; }
rm() { printf 'rm %s\\n' "$*"; command rm "$@"; }
''' + script, text=True, capture_output=True, env=os.environ, timeout=5)
        return root, result

    def test_proxy_reload_and_restart_do_not_touch_agent(self):
        for action in ('reload', 'restart'):
            with self.subTest(action=action):
                root, result = self.maintenance(action)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), [
                    'systemctl enable tako-server', f'systemctl {action} tako-server'])
                self.assertTrue((root / 'etc/systemd/system/tako-network-agent.service').exists())

    def test_native_rollback_preserves_capability_free_executable(self):
        for provider in ('native', 'tailscale', 'netbird'):
            with self.subTest(provider=provider):
                root, result = self.maintenance('rollback', provider)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / 'usr/local/bin/tako-server').read_text(), 'previous binary\n')
                self.assertEqual('setcap ' in result.stdout, provider != 'native')
                self.assertNotIn('systemctl ', result.stdout)

    def test_implode_stops_and_removes_agent_before_removing_data(self):
        root, result = self.maintenance('implode')
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = result.stdout.splitlines()
        self.assertIn('systemctl disable --now tako-network-agent.service', calls)
        self.assertLess(calls.index('systemctl disable --now tako-network-agent.service'),
                        calls.index('systemctl daemon-reload'))
        self.assertLess(calls.index('systemctl disable --now tako-network-agent.service'),
                        calls.index(f'rm -rf -- {root}/opt/tako'))
        self.assertFalse((root / 'etc/systemd/system/tako-network-agent.service').exists())
        self.assertFalse((root / 'opt/tako').exists())


if __name__ == '__main__':
    unittest.main()
