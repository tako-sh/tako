#!/usr/bin/env python3
import hashlib
import io
import os
import platform
from pathlib import Path
import subprocess
import sys
import tarfile
import urllib.request

VERSION = "0.5.17"
REVISION = "ccd326c175b583b5e82893a6078f06dcef5fba3f"
ROOT = Path(__file__).resolve().parent.parent / ".upstream"
CHECKSUMS = {
    ("darwin", "arm64"): "e211f68ff7658d19f193f2914417afdf8f89a053ff8f263e5d6b3b1d3bbc7b08",
    ("darwin", "x86_64"): "891875af09db152e93a4b31a8a79f538ce7ce702c132803cfe0a831e7cb1b7db",
    ("linux", "arm64"): "f8ca4a050095c1efbda2c4365172e61bf9d955ea0d9ac42f448b52e51819baa5",
    ("linux", "x86_64"): "cfb371176d164437ae869f8351cfde49bd1804ae71c61923f75c9cba9c9c006d",
}


def binary():
    override = os.environ.get("LITESTREAM_BINARY")
    if override:
        path = Path(override).resolve()
    else:
        system = platform.system().lower()
        arch = {"aarch64": "arm64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
        expected = CHECKSUMS[(system, arch)]
        archive = ROOT / f"litestream-{VERSION}-{system}-{arch}.tar.gz"
        ROOT.mkdir(parents=True, exist_ok=True)
        if not archive.exists():
            url = f"https://github.com/benbjohnson/litestream/releases/download/v{VERSION}/{archive.name}"
            with urllib.request.urlopen(url, timeout=120) as response:
                data = response.read()
            if hashlib.sha256(data).hexdigest() != expected:
                raise RuntimeError("upstream archive checksum mismatch")
            archive.write_bytes(data)
        data = archive.read_bytes()
        if hashlib.sha256(data).hexdigest() != expected:
            raise RuntimeError("cached upstream archive checksum mismatch")
        path = ROOT / "litestream"
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            for name, target in [("litestream", path), ("LICENSE", ROOT / "LICENSE.litestream")]:
                member = tar.getmember(name)
                if not member.isfile():
                    raise RuntimeError(f"unexpected archive member: {name}")
                target.write_bytes(tar.extractfile(member).read())
        path.chmod(0o755)
    version = subprocess.check_output([str(path), "version"], text=True).strip()
    if version != VERSION:
        raise RuntimeError(f"expected upstream {VERSION}, got {version}")
    return path


def source():
    path = Path(os.environ.get("LITESTREAM_SOURCE", ROOT / "source")).resolve()
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", "--depth", "1", "--branch", f"v{VERSION}", "https://github.com/benbjohnson/litestream", str(path)], check=True, stdout=sys.stderr)
    revision = subprocess.check_output(["git", "-C", str(path), "rev-parse", "HEAD"], text=True).strip()
    if revision != REVISION:
        raise RuntimeError(f"upstream source revision mismatch: {revision}")
    subprocess.run(["git", "-C", str(path), "diff", "--exit-code", "HEAD", "--"], check=True, stdout=sys.stderr)
    return path


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in {"binary", "source"}:
        raise SystemExit("usage: scripts/upstream.py binary|source")
    print(binary() if sys.argv[1] == "binary" else source())
