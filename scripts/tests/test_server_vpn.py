import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "scripts/install-tako-server.sh").read_text()
FUNCTIONS = SOURCE[SOURCE.index("vpn_functions() {"):SOURCE.index("process_has_management_host() {")]


class ServerVpnTests(unittest.TestCase):
    def configure(self, *, provider="native", saved="native", host="",
                  restart="1",
                  previous="100.80.0.9", native="fd00:1234:5678::1"):
        with tempfile.TemporaryDirectory() as temporary:
            settings = Path(temporary) / "vpn-provider"
            settings.write_text(saved + "\n")
            script = FUNCTIONS.replace("/usr/local/libexec/tako/vpn-provider", str(settings))
            script = script.replace("/usr/local/libexec/tako/native-address", "native_address")
            script += """
native_address() { [ "$TEST_NATIVE" != disconnected ] || return 1; printf '%s\\n' "$TEST_NATIVE"; }
is_enabled() { [ "$1" = 1 ]; }
existing_service_arg_value() { printf '%s\\n' "$TEST_PREVIOUS"; }
TAKO_VPN=$TEST_PROVIDER
TAKO_MANAGEMENT_HOST=$TEST_HOST
TAKO_RESTART_SERVICE=$TEST_RESTART
TAKO_MANAGEMENT_REQUIRED_MESSAGE='VPN required.'
TAKO_MANAGEMENT_ARGS=''
configure_management_http
printf 'PROVIDER=%s\\nHOST=%s\\nARGS=%s\\n' "$TAKO_VPN" "$TAKO_MANAGEMENT_HOST" "$TAKO_MANAGEMENT_ARGS"
"""
            return subprocess.run(["sh", "-eu"], input=script, text=True,
                                  capture_output=True, timeout=5, env={
                                      **os.environ, "TEST_PROVIDER": provider,
                                      "TEST_HOST": host,
                                      "TEST_RESTART": restart,
                                      "TEST_PREVIOUS": previous, "TEST_NATIVE": native,
                                  })

    def test_removed_providers_fail_before_configuring_a_listener(self):
        for provider in ("tailscale", "netbird", "unknown"):
            with self.subTest(provider=provider):
                result = self.configure(provider=provider)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("only native Tako networking", result.stderr)
                self.assertNotIn("--management-host", result.stdout)

    def test_saved_removed_provider_fails_without_explicit_reenrollment(self):
        for provider in ("tailscale", "netbird"):
            result = self.configure(provider="", saved=provider)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("re-enroll", result.stderr)

    def test_explicit_native_bootstrap_discards_old_binding(self):
        result = self.configure(saved="tailscale", restart="0", native="disconnected")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ARGS=\n", result.stdout)

    def test_rejects_public_and_loopback_addresses(self):
        for address in ["8.8.8.8", "127.0.0.1", "::1", "::", "fc::1", "fd00::::1"]:
            with self.subTest(address=address):
                self.assertNotEqual(self.configure(native=address).returncode, 0)

    def test_native_management_uses_assigned_ipv6(self):
        result = self.configure(provider="native", saved="native")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--management-host fd00:1234:5678::1", result.stdout)

    def test_native_rejects_unassigned_address(self):
        result = self.configure(provider="native", host="fd00:1234:5678::9")
        self.assertNotEqual(result.returncode, 0)

    def test_native_bootstrap_does_not_reuse_unverified_previous_binding(self):
        result = self.configure(provider="native", saved="native", restart="0",
                                previous="fd00::1", native="disconnected")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ARGS=\n", result.stdout)

    def test_native_start_requires_applied_state(self):
        result = self.configure(provider="native", saved="native", native="disconnected")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
