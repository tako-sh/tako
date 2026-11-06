import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / 'scripts/install-tako-server.sh').read_text()
CORROSION = SOURCE[SOURCE.index('install_native_corrosion() {'):SOURCE.index('install_native_network() {')]
PINS = {
    'x86_64': '3504d7d1b4b53737457fc40f2353a400cf4df0c1217ec318924d7ee310876194',
    'aarch64': 'b00b5c6996dee89c36e51e1d4196602bb7e12bbb25abc9b68390e062bdd13be3',
}
PAYLOAD = b'#!/bin/sh\nprintf "fixture corrosion\\n"\n'


class CorrosionInstallerTests(unittest.TestCase):
    def install(self, *, arch='x86_64', valid=True, member='corrosion', symlink=False,
                existing=False, payload=PAYLOAD):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        destination = root / 'usr/local/libexec/tako'
        destination.mkdir(parents=True)
        extract = root / 'extract'
        extract.mkdir()
        if existing:
            (destination / 'corrosion').write_bytes(b'old binary')
            (destination / 'corrosion.sha256').write_bytes(b'old digest')
        archive = root / 'fixture.tar.gz'
        with tarfile.open(archive, 'w:gz') as bundle:
            entry = tarfile.TarInfo(member)
            if symlink:
                entry.type = tarfile.SYMTYPE
                entry.linkname = '/bin/sh'
                bundle.addfile(entry)
            else:
                entry.size = len(payload)
                bundle.addfile(entry, io.BytesIO(payload))
        script = CORROSION.replace('/usr/local/', str(root) + '/usr/local/')
        if valid and arch in PINS:
            script = script.replace(PINS[arch], hashlib.sha256(archive.read_bytes()).hexdigest())
        result = subprocess.run(['sh', '-eu'], input='''
download_file() { printf 'download %s\\n' "$1"; cp "$TEST_ARCHIVE" "$2"; }
install() {
  printf 'install %s\\n' "$*"
  [ "$1 $2 $3 $4" = '-o root -g root' ] || exit 1
  shift 4
  command install "$@"
}
''' + script + '\ninstall_native_corrosion\n', text=True, capture_output=True,
            env={**os.environ, 'arch': arch, 'tmp_extract': str(extract),
                 'TEST_ARCHIVE': str(archive)}, timeout=10)
        return destination, result

    def test_each_architecture_installs_verified_binary_and_its_own_digest(self):
        for arch in PINS:
            with self.subTest(arch=arch):
                destination, result = self.install(arch=arch)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f'/v1.0.0/corrosion-{arch}-unknown-linux-gnu.tar.gz', result.stdout)
                binary = destination / 'corrosion'
                digest = destination / 'corrosion.sha256'
                self.assertEqual(binary.read_bytes(), PAYLOAD)
                self.assertEqual(digest.read_text(), hashlib.sha256(PAYLOAD).hexdigest() + '\n')
                self.assertNotEqual(digest.read_text().strip(), PINS[arch])
                self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
                self.assertEqual(digest.stat().st_mode & 0o777, 0o644)
                self.assertEqual(result.stdout.count('install -o root -g root'), 2)

    def test_production_pins_reject_untrusted_archive_before_extraction(self):
        for arch, digest in PINS.items():
            with self.subTest(arch=arch):
                self.assertIn(f'{arch}) corrosion_archive_sha256={digest}', CORROSION)
                destination, result = self.install(arch=arch, valid=False, existing=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('archive checksum mismatch', result.stderr)
                self.assertEqual((destination / 'corrosion').read_bytes(), b'old binary')
                self.assertEqual((destination / 'corrosion.sha256').read_bytes(), b'old digest')
                self.assertNotIn('install -o', result.stdout)
                self.assertFalse((destination.parents[3] / 'extract/corrosion-release').exists())

    def test_missing_symlink_or_empty_binary_is_rejected(self):
        for options in ({'member': 'other'}, {'symlink': True}, {'payload': b''}):
            with self.subTest(options=options):
                destination, result = self.install(**options)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((destination / 'corrosion').exists())
                self.assertFalse((destination / 'corrosion.sha256').exists())

    def test_refresh_replaces_binary_and_digest(self):
        destination, result = self.install(existing=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((destination / 'corrosion').read_bytes(), PAYLOAD)
        self.assertEqual((destination / 'corrosion.sha256').read_text().strip(),
                         hashlib.sha256(PAYLOAD).hexdigest())
        self.assertEqual(sorted(p.name for p in destination.iterdir()), ['corrosion', 'corrosion.sha256'])

    def test_unknown_architecture_fails_before_download(self):
        _, result = self.install(arch='riscv64')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unsupported Corrosion architecture', result.stderr)
        self.assertNotIn('download ', result.stdout)


if __name__ == '__main__':
    unittest.main()
