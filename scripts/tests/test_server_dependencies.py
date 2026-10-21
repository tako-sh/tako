import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ServerDependencyTests(unittest.TestCase):
    def run_remi_install(self, failures, stage="epel-release"):
        source = (ROOT / "scripts/install-tako-server.sh").read_text()
        functions = source[
            source.index("# dnf hosts fall into two groups:") :
            source.index("install_libvips_runtime() {")
        ]
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            result = subprocess.run(
                ["sh", "-eu"],
                input=functions + """
need_cmd() { command -v "$1" >/dev/null 2>&1; }
rpm() { echo 10; }
sleep() { :; }
dnf() {
  printf '%s\\n' "$*" >> "$TEST_LOG"
  case "$*" in
    *"$TEST_STAGE"*)
      if [ "$remaining" -gt 0 ]; then
        remaining=$((remaining - 1))
        return 1
      fi
      ;;
  esac
}
remaining=$TEST_FAILURES
install_libvips_from_remi
""",
                env={
                    **os.environ,
                    "TEST_LOG": str(directory / "dnf.log"),
                    "TEST_FAILURES": str(failures),
                    "TEST_STAGE": stage,
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result, (directory / "dnf.log").read_text().splitlines()

    def test_refreshes_metadata_and_retries_repository_setup(self):
        result, calls = self.run_remi_install(1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        installs = [call for call in calls if "epel-release" in call]
        self.assertEqual(len(installs), 2)
        self.assertTrue(all("--refresh" in call for call in installs))
        self.assertIn("clean metadata", calls)

    def test_installs_vips_without_php_module_metadata(self):
        result, calls = self.run_remi_install(0)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        vips = next(call for call in calls if call.endswith(" vips"))
        self.assertIn("--disablerepo=remi-modular*", vips)
        self.assertIn("--setopt=install_weak_deps=False", vips)
        remi = next(call for call in calls if "remi-release-10.rpm" in call)
        self.assertIn("--disablerepo=epel*", remi)

    def test_stops_when_required_repository_setup_keeps_failing(self):
        result, calls = self.run_remi_install(100)
        self.assertNotEqual(result.returncode, 0)
        installs = [call for call in calls if "epel-release" in call]
        self.assertEqual(len(installs), 3)

    def test_recovers_from_stale_vips_metadata(self):
        result, calls = self.run_remi_install(1, " vips")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len([call for call in calls if call.endswith(" vips")]), 2)
        self.assertIn("clean metadata", calls)

    def test_reports_persistent_vips_install_failure(self):
        result, calls = self.run_remi_install(100, " vips")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len([call for call in calls if call.endswith(" vips")]), 3)


if __name__ == "__main__":
    unittest.main()
