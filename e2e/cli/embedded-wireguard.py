#!/usr/bin/env python3
"""Exercise the embedded tunnel in a disposable Linux network namespace.

Requires root, /dev/net/tun, iproute2 and ping. Pass the Tako binary as argv[1].
Do not run against a workstation's network namespace.
"""

import base64
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import time


TAKO = sys.argv[1]
INTERFACE_A = "tako0000000001"
INTERFACE_B = "tako0000000002"
NAMESPACE = {INTERFACE_A: "tako-test-a", INTERFACE_B: "tako-test-b"}
PRIVATE_A = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a"
PUBLIC_A = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
PRIVATE_B = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb"
PUBLIC_B = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"


def key(value):
    return base64.b64encode(bytes.fromhex(value)).decode()


def helper(operation, interface, ok=True, **kwargs):
    if operation in ("start", "sync"):
        kwargs["network"] = interface[4:] + "0" * 22
        kwargs["names"] = [{"name": "server-b" if interface == INTERFACE_A else "server-a",
                            "address": "fd42::2" if interface == INTERFACE_A else "fd42::1"}]
    response = subprocess.run(
        ["ip", "netns", "exec", NAMESPACE[interface], TAKO, "__network-tunnel"],
        input=json.dumps(dict(operation=operation, interface=interface, **kwargs)),
        text=True,
        capture_output=True,
        timeout=35,
        check=True,
    )
    assert response.stderr == "", response.stderr
    reply = json.loads(response.stdout)
    assert (reply["error"] is None) == ok, reply
    return reply


def configuration(private_key, address, peer_key, peer_address, port):
    return (
        f"[Interface]\nPrivateKey = {key(private_key)}\nAddress = {address}/128\n"
        f"\n[Peer]\nPublicKey = {key(peer_key)}\nAllowedIPs = {peer_address}/128\n"
        f"Endpoint = {'192.0.2.2' if address == 'fd42::1' else '192.0.2.1'}:{port}\nPersistentKeepalive = 25\n"
    )


def status(interface):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(f"/var/run/wireguard/{interface}.sock")
        stream.sendall(b"get=1\n\n")
        data = b""
        while not data.endswith(b"\n\n"):
            chunk = stream.recv(65536)
            assert chunk, "incomplete response"
            data += chunk
        return dict(line.split("=", 1) for line in data.decode().splitlines() if line)


def command(*args):
    return subprocess.run(args, text=True, capture_output=True, check=True).stdout


def wait_removed(interface):
    deadline = time.monotonic() + 10
    while subprocess.run(["ip", "-n", NAMESPACE[interface], "link", "show", "dev", interface], capture_output=True).returncode == 0:
        assert time.monotonic() < deadline, f"interface leaked: {interface}"
        time.sleep(0.05)


assert sys.platform == "linux" and os.geteuid() == 0 and Path("/.dockerenv").exists()
# Resolver integration is exercised by private-dns.py. These independent network
# namespaces use a stub adapter because the host D-Bus cannot see their links.
adapter = tempfile.TemporaryDirectory(prefix="tako-resolver-adapter-")
Path(adapter.name, "resolvectl").write_text("#!/bin/sh\nexit 0\n")
Path(adapter.name, "resolvectl").chmod(0o700)
os.environ["PATH"] = adapter.name + ":" + os.environ["PATH"]
resolver = Path("/etc/resolv.conf").read_bytes()
hosts = Path("/etc/hosts").read_bytes()
Path("/etc/resolv.conf").write_text("nameserver 127.0.0.53\n")
if not Path("/dev/net/tun").exists():
    Path("/dev/net").mkdir(exist_ok=True)
    os.mknod("/dev/net/tun", stat.S_IFCHR | 0o600, os.makedev(10, 200))
for namespace in NAMESPACE.values():
    assert namespace not in command("ip", "netns", "list")
    command("ip", "netns", "add", namespace)
    command("ip", "-n", namespace, "link", "set", "lo", "up")
command("ip", "link", "add", "underlay-a", "type", "veth", "peer", "name", "underlay-b")
for suffix, address in [("a", "192.0.2.1/24"), ("b", "192.0.2.2/24")]:
    namespace = f"tako-test-{suffix}"
    interface = f"underlay-{suffix}"
    command("ip", "link", "set", interface, "netns", namespace)
    command("ip", "-n", namespace, "addr", "add", address, "dev", interface)
    command("ip", "-n", namespace, "link", "set", interface, "up")

# Route conflict must fail startup and remove the partially configured tunnel.
command("ip", "-n", NAMESPACE[INTERFACE_A], "-6", "route", "add", "fd42::2/128", "dev", "lo")
config_a = configuration(PRIVATE_A, "fd42::1", PUBLIC_B, "fd42::2", 9)
config_b = configuration(PRIVATE_B, "fd42::2", PUBLIC_A, "fd42::1", 9)
helper("start", INTERFACE_A, ok=False, configuration=config_a)
wait_removed(INTERFACE_A)
assert helper("inspect", INTERFACE_A)["public_key"] is None
command("ip", "-n", NAMESPACE[INTERFACE_A], "-6", "route", "del", "fd42::2/128", "dev", "lo")

try:
    helper("start", INTERFACE_A, configuration=config_a)
    helper("start", INTERFACE_B, configuration=config_b)
    assert helper("inspect", INTERFACE_A)["public_key"] == key(PUBLIC_A)
    assert helper("inspect", INTERFACE_B)["public_key"] == key(PUBLIC_B)
    config_a = configuration(PRIVATE_A, "fd42::1", PUBLIC_B, "fd42::2", status(INTERFACE_B)["listen_port"])
    config_b = configuration(PRIVATE_B, "fd42::2", PUBLIC_A, "fd42::1", status(INTERFACE_A)["listen_port"])
    helper("sync", INTERFACE_A, configuration=config_a)
    helper("sync", INTERFACE_B, configuration=config_b)

    # Reject a mismatched identity without disturbing the working tunnel.
    helper("stop", INTERFACE_A, ok=False, public_key=key(PUBLIC_B))
    helper("sync", INTERFACE_A, ok=False, configuration=config_b)
    assert helper("inspect", INTERFACE_A)["public_key"] == key(PUBLIC_A)

    # These endpoints are separate hosts: successful pings must cross the encrypted tunnel.
    for interface, address in [(INTERFACE_A, "fd42::2"), (INTERFACE_B, "fd42::1")]:
        command("ip", "netns", "exec", NAMESPACE[interface], "ping", "-6", "-c", "2", "-w", "10", address)
    a, b = status(INTERFACE_A), status(INTERFACE_B)
    assert int(a["tx_bytes"]) > 0 and int(b["rx_bytes"]) > 0
    for interface, peer in [(INTERFACE_A, "fd42::2"), (INTERFACE_B, "fd42::1")]:
        routes = command("ip", "-n", NAMESPACE[interface], "-6", "route", "show")
        assert f"{peer} dev {interface}" in routes, routes
        assert "default" not in routes
    # A second sync preserves the same device and listening port.
    index = json.loads(command("ip", "-n", NAMESPACE[INTERFACE_A], "-j", "link", "show", "dev", INTERFACE_A))[0]["ifindex"]
    port = status(INTERFACE_A)["listen_port"]
    helper("sync", INTERFACE_A, configuration=config_a)
    assert json.loads(command("ip", "-n", NAMESPACE[INTERFACE_A], "-j", "link", "show", "dev", INTERFACE_A))[0]["ifindex"] == index
    assert status(INTERFACE_A)["listen_port"] == port
    assert int(status(INTERFACE_A)["tx_bytes"]) >= int(a["tx_bytes"])
finally:
    for interface, public in [(INTERFACE_A, PUBLIC_A), (INTERFACE_B, PUBLIC_B)]:
        helper("stop", interface, public_key=key(public))
        wait_removed(interface)
        assert helper("inspect", interface)["public_key"] is None

for interface in (INTERFACE_A, INTERFACE_B):
    assert interface not in command("ip", "-n", NAMESPACE[interface], "-6", "route", "show")
    command("ip", "netns", "delete", NAMESPACE[interface])
assert Path("/etc/hosts").read_bytes() == hosts
Path("/etc/resolv.conf").write_bytes(resolver)
print("Embedded tunnel lifecycle, encrypted traffic, ownership, and rollback passed")
