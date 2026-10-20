import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class InstallerTests(unittest.TestCase):
    def test_installs_macos_bundle_for_each_architecture(self):
        for machine, arch in [("x86_64", "x86_64"), ("arm64", "aarch64")]:
            with self.subTest(machine=machine), tempfile.TemporaryDirectory() as tmp:
                directory = Path(tmp)
                commands = directory / "commands"
                commands.mkdir()
                for name, body in {
                    "uname": f'case "$1" in -s) echo Darwin ;; -m) echo {machine} ;; esac',
                    "codesign": "exit 0",
                    "vips": "exit 0",
                }.items():
                    command = commands / name
                    command.write_text(f"#!/bin/sh\n{body}\n")
                    command.chmod(0o755)

                payload = directory / "payload"
                cli = payload / "Tako.app/Contents/MacOS/tako"
                cli.parent.mkdir(parents=True)
                cli.write_text(f"CLI for {arch}\n")
                for name in ["tako-dev-server", "tako-dev-proxy"]:
                    (payload / name).write_text(f"{name} for {arch}\n")
                archive = directory / f"tako-darwin-{arch}.tar.gz"
                with tarfile.open(archive, "w:gz") as tar:
                    for entry in payload.iterdir():
                        tar.add(entry, arcname=entry.name)
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                Path(f"{archive}.sha256").write_text(f"{digest}  {archive.name}\n")

                install_dir = directory / "bin"
                app_dir = directory / "apps"
                result = subprocess.run(
                    ["sh", str(ROOT / "scripts/install-tako.sh")],
                    env={
                        **os.environ,
                        "PATH": f"{commands}:{os.environ['PATH']}",
                        "TAKO_INSTALL_DIR": str(install_dir),
                        "TAKO_MACOS_APP_DIR": str(app_dir),
                        "TAKO_DOWNLOAD_BASE_URL": directory.as_uri(),
                        "TAKO_URL": "",
                    },
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue((install_dir / "tako").is_symlink())
                self.assertEqual((install_dir / "tako").read_text(), f"CLI for {arch}\n")
                for name in ["tako-dev-server", "tako-dev-proxy"]:
                    self.assertEqual(
                        (install_dir / name).read_text(), f"{name} for {arch}\n"
                    )


class IntelPublishTests(unittest.TestCase):
    def run_publish(self, latest, lookup_status=0):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        directory = Path(temporary.name)
        command = directory / "gh"
        command.write_text(
            '#!/bin/sh\n'
            'case "$1" in\n'
            '  api) printf "%s\\n" "$TEST_LATEST"; exit "$TEST_LOOKUP_STATUS" ;;\n'
            '  release) printf "%s\\n" "$@" > "$TEST_UPLOAD_LOG" ;;\n'
            '  *) exit 1 ;;\n'
            'esac\n'
        )
        command.chmod(0o755)
        log = directory / "upload.log"
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/publish-intel-release.sh"), "a" * 40],
            cwd=directory,
            env={
                **os.environ,
                "PATH": f"{directory}:{os.environ['PATH']}",
                "GH_REPO": "tako-sh/tako",
                "TEST_LATEST": latest,
                "TEST_LOOKUP_STATUS": str(lookup_status),
                "TEST_UPLOAD_LOG": str(log),
            },
            capture_output=True,
            text=True,
            timeout=30,
        )
        return result, log.read_text().splitlines() if log.exists() else []

    def test_uploads_archive_and_checksum_for_current_release(self):
        result, upload = self.run_publish("a" * 40)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            upload,
            [
                "release", "upload", "latest", "--clobber",
                "signed-dist/tako-darwin-x86_64.tar.gz",
                "signed-dist/tako-darwin-x86_64.tar.gz.sha256",
            ],
        )

    def test_skips_upload_when_newer_release_has_published(self):
        result, upload = self.run_publish("b" * 40)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(upload, [])

    def test_failed_release_lookup_cannot_publish(self):
        result, upload = self.run_publish("", lookup_status=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(upload, [])


class MacosPackageTests(unittest.TestCase):
    def test_packages_both_architectures_with_verified_checksums(self):
        for arch in ["aarch64", "x86_64"]:
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as tmp:
                directory = Path(tmp)
                (directory / "scripts").symlink_to(ROOT / "scripts")
                commands = directory / "commands"
                commands.mkdir()
                for name, body in {
                    "security": "exit 0",
                    "codesign": 'printf "%s\\n" "$*" >> "$TEST_CODESIGN_LOG"',
                    "ditto": 'cp -R "$1" "$2"',
                }.items():
                    command = commands / name
                    command.write_text(f"#!/bin/sh\n{body}\n")
                    command.chmod(0o755)
                source = directory / f"raw/bin-tako-darwin-{arch}"
                source.mkdir(parents=True)
                for name in ["tako", "tako-dev-server", "tako-dev-proxy"]:
                    (source / name).write_text(f"{name} for {arch}\n")

                log = directory / "codesign.log"
                result = subprocess.run(
                    ["bash", str(ROOT / "scripts/sign-macos-release.sh"), arch],
                    cwd=directory,
                    env={
                        **os.environ,
                        "PATH": f"{commands}:{os.environ['PATH']}",
                        "TAKO_CODESIGN_CERTIFICATE_BASE64": "Y2VydA==",
                        "TAKO_CODESIGN_CERTIFICATE_PASSWORD": "",
                        "TAKO_CODESIGN_IDENTITY": "test-identity",
                        "TAKO_APPLE_TEAM_ID": "test-team",
                        "TAKO_KEYCHAIN_ACCESS_GROUP": "test-group",
                        "TAKO_PROVISION_PROFILE_BASE64": "cHJvZmlsZQ==",
                        "TAKO_PROVISION_PROFILE_PATH": "",
                        "TEST_CODESIGN_LOG": str(log),
                    },
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                archive = directory / f"signed-dist/tako-darwin-{arch}.tar.gz"
                expected_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
                self.assertEqual(
                    Path(f"{archive}.sha256").read_text().split()[0], expected_hash
                )
                with tarfile.open(archive) as tar:
                    for name in [
                        "Tako.app/Contents/MacOS/tako",
                        "tako-dev-server",
                        "tako-dev-proxy",
                    ]:
                        self.assertEqual(
                            tar.extractfile(name).read().decode(),
                            f"{Path(name).name} for {arch}\n",
                        )
                signing = log.read_text().splitlines()
                signed = [line for line in signing if "--force" in line]
                self.assertEqual(len(signed), 3)
                self.assertTrue(all("--sign test-identity" in line for line in signed))
                self.assertEqual(
                    all("--pagesize 16384" in line for line in signed), arch == "aarch64"
                )
                self.assertEqual(
                    len([line for line in signing if "--verify --strict" in line]), 4
                )


if __name__ == "__main__":
    unittest.main()
