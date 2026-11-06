#!/usr/bin/env python3
"""Offline native CLI checks. Run with the built CLI path as the sole argument."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="tako-native-client-") as temporary:
    root = Path(temporary)
    home = root / "owner"
    home.mkdir(mode=0o700)

    def run(*args, directory=home, ok=True):
        result = subprocess.run(
            [binary, "--ci", "--json", *args], cwd=root,
            env={**os.environ, "TAKO_HOME": str(directory)},
            capture_output=True, text=True, timeout=20,
        )
        assert (result.returncode == 0) == ok, (args, result.returncode, result.stdout, result.stderr)
        return result

    def tree():
        return {str(p.relative_to(home)): p.read_bytes() for p in home.rglob("*") if p.is_file()}

    run("network", "create", "office")
    saved = next(home.glob("networks/*/*/profile.json"))
    profile = json.loads(saved.read_text())
    keys_path = next(home.glob("network-keys/*.json"))
    keys = json.loads(keys_path.read_text())
    exported = root / "public.json"
    run("network", "export", "office", "--output", str(exported))
    assert json.loads(exported.read_text()) == profile
    for field in ("wg_private_key", "ssh_private_key"):
        assert field not in exported.read_text()
        assert keys[field] not in exported.read_text()
    run("network", "import", str(exported), "--name", "renamed")
    listed = run("network", "list").stdout
    assert "renamed" in listed and profile["snapshot"]["network"]["id"] in listed
    foreign = root / "foreign"
    foreign.mkdir(mode=0o700)
    run("network", "import", str(exported), directory=foreign, ok=False)
    assert not list(foreign.glob("networks/*/*/profile.json"))
    before = tree()
    request = root / "request.json"
    for args in [
        ("network", "create", "dry"),
        ("network", "request", "--name", "laptop", "--output", str(request)),
        ("network", "export", "renamed", "--output", str(root / "dry-export.json")),
        ("network", "import", str(exported), "--name", "dry-import"),
        ("connect", "renamed"), ("disconnect", "renamed"),
    ]:
        run("--dry-run", *args)
        assert tree() == before, args
    assert not request.exists() and not (root / "dry-export.json").exists()
    run("network", "request", "--name", "laptop", "--output", str(request))
    public_request = json.loads(request.read_text())
    request_keys = home / "network-keys" / (public_request["request_id"] + ".json")
    assert request_keys.exists()
    assert "PRIVATE KEY" not in request.read_text() and "wg_private_key" not in request.read_text()
    before = tree()
    run("network", "request", "--name", "laptop", "--output", str(request), ok=False)
    assert tree() == before
    run("network", "export", "renamed", "--output", str(exported), ok=False)
    for hard in (False, True):
        linked_profile = root / ("hard-profile.json" if hard else "symlink-profile.json")
        if hard:
            os.link(exported, linked_profile)
        else:
            linked_profile.symlink_to(exported)
        run("network", "import", str(linked_profile), "--dry-run", ok=False)
        linked_profile.unlink()
    linked_directory = root / "linked-directory"
    linked_directory.symlink_to(home, target_is_directory=True)
    run("network", "request", "--name", "linked", "--output", str(root / "linked-request.json"), directory=linked_directory, ok=False)
    assert not (root / "linked-request.json").exists()
    original_keys = keys_path.read_bytes()
    for link in ("symlink", "hardlink"):
        keys_path.unlink()
        other = root / (link + "-keys.json")
        other.write_bytes(original_keys)
        other.chmod(0o600)
        if link == "symlink":
            keys_path.symlink_to(other)
        else:
            os.link(other, keys_path)
        run("network", "export", "renamed", "--output", str(root / (link + ".json")), ok=False)
        assert other.read_bytes() == original_keys
    keys_path.unlink()
    keys_path.write_bytes(original_keys)
    keys_path.chmod(0o600)
    bad_project = root / "tako.toml"
    bad_project.write_text("not valid toml [")
    run("--dry-run", "connect", ok=False)
    run("--dry-run", "--config", str(root / "missing.toml"), "connect", ok=False)
    bad_project.unlink()
    config = home / "config.toml"
    config.write_text("not valid toml [")
    run("network", "list", ok=False)
    print("PASS native create/request/export/import/list/dry-run, own-key transfer, key links, corrupt config")
