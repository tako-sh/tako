import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "scripts/install-tako-server.sh").read_text()
FUNCTIONS = SOURCE[SOURCE.index("private_network_functions() {"):SOURCE.index("process_has_management_host() {")]


class ServerNetworkTests(unittest.TestCase):
    def configure(self, *, provider="netbird", saved="netbird", host="",
                  tailscale="100.64.0.1", netbird="100.80.0.1", restart="1",
                  previous="100.80.0.9"):
        with tempfile.TemporaryDirectory() as temporary:
            settings = Path(temporary) / "network-provider"
            settings.write_text(saved + "\n")
            script = FUNCTIONS.replace("/usr/local/libexec/tako/network-provider", str(settings))
            script += """
is_enabled() { [ "$1" = 1 ]; }
existing_service_arg_value() { printf '%s\\n' "$TEST_PREVIOUS"; }
tailscale() { [ "$TEST_TAILSCALE" != disconnected ] || return 1; if [ "$1" = status ]; then printf '%s\\n' '{"BackendState":"Running"}'; return; fi; printf '%s\\n' "$TEST_TAILSCALE"; }
netbird() { [ "$TEST_NETBIRD" != disconnected ] || return 1; if [ "$#" = 1 ]; then printf '%s\\n' 'Management: Connected'; return; fi; printf '%s\\n' "$TEST_NETBIRD"; }
TAKO_NETWORK=$TEST_PROVIDER
TAKO_MANAGEMENT_HOST=$TEST_HOST
TAKO_RESTART_SERVICE=$TEST_RESTART
TAKO_MANAGEMENT_REQUIRED_MESSAGE='Private network required.'
TAKO_MANAGEMENT_ARGS=''
configure_management_http
printf 'PROVIDER=%s\\nHOST=%s\\nARGS=%s\\n' "$TAKO_NETWORK" "$TAKO_MANAGEMENT_HOST" "$TAKO_MANAGEMENT_ARGS"
"""
            return subprocess.run(["sh", "-eu"], input=script, text=True,
                                  capture_output=True, timeout=5, env={
                                      **os.environ, "TEST_PROVIDER": provider,
                                      "TEST_HOST": host, "TEST_TAILSCALE": tailscale,
                                      "TEST_NETBIRD": netbird, "TEST_RESTART": restart,
                                      "TEST_PREVIOUS": previous,
                                  })

    def test_selected_provider_wins_when_both_agents_report_addresses(self):
        result = self.configure()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("HOST=100.80.0.1", result.stdout)
        self.assertNotIn("HOST=100.64.0.1", result.stdout)

    def test_existing_binding_is_kept_when_agent_still_owns_it(self):
        result = self.configure(netbird="100.80.0.1\n100.80.0.9")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("HOST=100.80.0.9", result.stdout)

    def test_unknown_provider_fails_before_configuring_a_listener(self):
        result = self.configure(provider="unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("--management-host", result.stdout)

    def test_rejects_an_address_owned_by_the_other_provider(self):
        result = self.configure(host="100.64.0.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("assigned to this server by netbird", result.stderr)

    def test_rejects_public_and_loopback_addresses_even_if_reported_by_agent(self):
        for address in ["8.8.8.8", "127.0.0.1", "0.0.0.0", "::1", "::", "fc::1", "fd00::::1", "fd00:1:2:3:4:5:6:7:8"]:
            with self.subTest(address=address):
                self.assertNotEqual(self.configure(netbird=address).returncode, 0)

    def test_supports_custom_private_netbird_ranges_and_ipv6(self):
        for address in ["10.10.0.1", "192.168.7.1", "fd00::1"]:
            with self.subTest(address=address):
                result = self.configure(netbird=address)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"--management-host {address}", result.stdout)

    def test_saved_provider_survives_offline_binary_refresh(self):
        result = self.configure(provider="", netbird="disconnected", restart="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PROVIDER=netbird", result.stdout)
        self.assertIn("--management-host 100.80.0.9", result.stdout)

    def test_start_refuses_disconnected_selected_provider(self):
        result = self.configure(netbird="disconnected")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Connect this server to netbird", result.stderr)

    def test_bootstrap_does_not_reuse_old_binding_when_switching_providers(self):
        result = self.configure(saved="tailscale", netbird="disconnected", restart="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ARGS=\n", result.stdout)
        self.assertNotIn("--management-host", result.stdout)


if __name__ == "__main__":
    unittest.main()
