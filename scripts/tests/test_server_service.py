import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "scripts/install-tako-server.sh").read_text()
IS_ENABLED = SOURCE[
    SOURCE.index("is_enabled() {") : SOURCE.index("private_network_functions() {")
]
SERVICE_SETUP = SOURCE[
    SOURCE.rindex('\nif [ "$SERVICE_MANAGER" = "systemd" ]; then\n') :
    SOURCE.index('\necho "OK installed tako-server"')
]


class ServerServiceTests(unittest.TestCase):
    def run_setup(self, *, active=False, restart=True, enable_fails=False, manager="systemd"):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "systemctl.log"
            result = subprocess.run(
                ["sh", "-eu"],
                input=IS_ENABLED + """
systemctl() {
  printf '%s\\n' "$*" >> "$TEST_SYSTEMCTL_LOG"
  case "$1" in
    is-active) [ "$TEST_SERVICE_ACTIVE" = 1 ] ;;
    enable)
      if [ "$TEST_ENABLE_FAILS" = 1 ]; then
        echo 'Failed to enable unit' >&2
        return 1
      fi
      ;;
    start|restart) TEST_SERVICE_ACTIVE=1 ;;
  esac
}
""" + SERVICE_SETUP,
                env={
                    **os.environ,
                    "SERVICE_MANAGER": manager,
                    "TAKO_RESTART_SERVICE": "1" if restart else "0",
                    "TEST_SERVICE_ACTIVE": "1" if active else "0",
                    "TEST_ENABLE_FAILS": "1" if enable_fails else "0",
                    "TEST_SYSTEMCTL_LOG": str(log),
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            calls = log.read_text().splitlines() if log.exists() else []
            return result, calls

    def test_default_install_enables_stopped_service_before_starting(self):
        result, calls = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(calls.index("enable tako-server"), calls.index("start tako-server"))

    def test_default_install_enables_running_service_before_restarting(self):
        result, calls = self.run_setup(active=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(calls.index("enable tako-server"), calls.index("restart tako-server"))

    def test_install_fails_before_starting_when_boot_startup_cannot_be_enabled(self):
        for active in [False, True]:
            with self.subTest(active=active):
                result, calls = self.run_setup(active=active, enable_fails=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Failed to enable unit", result.stderr)
                self.assertEqual(calls, ["daemon-reload", "enable tako-server"])

    def test_refresh_repairs_boot_startup_without_restarting_running_service(self):
        result, calls = self.run_setup(active=True, restart=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [
            "daemon-reload", "is-active --quiet tako-server", "enable tako-server",
        ])
        self.assertIn("without restarting", result.stdout)

    def test_refresh_fails_when_boot_startup_cannot_be_repaired(self):
        result, _ = self.run_setup(active=True, restart=False, enable_fails=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Failed to enable unit", result.stderr)
        self.assertNotIn("OK refreshed", result.stdout)

    def test_bootstrap_does_not_enable_or_start_stopped_service(self):
        result, calls = self.run_setup(restart=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, ["daemon-reload", "is-active --quiet tako-server"])
        self.assertIn("service (not started)", result.stdout)

    def test_image_bootstrap_does_not_require_systemd(self):
        result, calls = self.run_setup(restart=False, manager="none")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])
        self.assertIn("skipped service definition install", result.stdout)


if __name__ == "__main__":
    unittest.main()
