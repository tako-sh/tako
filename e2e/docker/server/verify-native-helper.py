#!/usr/bin/env python3
"""Exercise the installed root helper and host WireGuard policy in a disposable container."""

import json
import os
import subprocess
from pathlib import Path


assert os.geteuid() == 0 and Path('/.dockerenv').is_file(), 'disposable root container required'
os.umask(0o077)


def run(*args, input=None):
    result = subprocess.run(args, input=input, text=True, capture_output=True, timeout=40)
    # Captured output can contain generated keys; never include it in an assertion.
    assert result.returncode == 0, f'{args[0]} failed'
    return result.stdout.strip()


ident = 'ab' * 16
dev = 'tako' + ident[:10]
helper = '/usr/local/libexec/tako/tako-network-helper'
config_dir = Path('/etc/wireguard/tako')
assert config_dir.stat().st_uid == 0 and config_dir.stat().st_mode & 0o777 == 0o700
network = Path('/opt/tako/networks') / ident
network.mkdir(mode=0o700)
private_key = run('wg', 'genkey')
public_key = run('wg', 'pubkey', input=private_key + '\n')
(network / 'desired.json').write_text(json.dumps({
    'network_id': ident, 'device_id': '12' * 32,
    'address': 'fd12:7461:6b6f::1/128', 'private_key': private_key, 'peers': [],
}))
run('systemctl', 'start', 'tako-network-helper.socket')
try:
    for action in ('apply', 'apply', 'isolate'):
        assert json.loads(run(helper, ident, action))['ok'], 'native helper rejected operation'
        assert run('wg', 'show', dev, 'public-key') == public_key, 'WireGuard key was not applied'
        assert run('wg', 'show', dev, 'listen-port') == '51820', 'WireGuard port was not applied'
        interfaces = json.loads(run('ip', '-j', '-6', 'address', 'show', 'dev', dev))
        assert any(a['local'] == 'fd12:7461:6b6f::1' and a['prefixlen'] == 128
                   for interface in interfaces for a in interface['addr_info']), 'IPv6 address missing'
        assert not list(config_dir.iterdir()), 'temporary WireGuard configuration was not removed'
    assert json.loads(run(helper, ident, 'remove'))['ok'], 'native helper removal failed'
    assert dev not in run('wg', 'show', 'interfaces').split(), 'WireGuard interface was not removed'
finally:
    (network / 'desired.json').unlink()
    network.rmdir()

print('PASS: installed native helper applies WireGuard keys, IPv6 and isolation with restricted service permissions')
