#!/usr/bin/env python3
"""Test the embedded system tunnel with real systemd-resolved in a disposable container.

Needs root, iproute2, wg, wireguard-go, dbus, systemd-resolved, and Python.
Pass the Tako binary as argv[1]. Never run on a workstation: this owns the
container's resolver, network namespace, and hosts file for the duration.
"""
import base64
import json
import os
import pwd
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import time


TAKO = sys.argv[1]
NETWORK = "00000000010000000000000000000000"
INTERFACE = "tako0000000001"
PRIVATE_A = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a"
PUBLIC_A = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
PRIVATE_B = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb"
PUBLIC_B = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.PIPE)


def key(value):
    return base64.b64encode(bytes.fromhex(value)).decode()


def helper(operation, ok=True, **kwargs):
    response = subprocess.run([TAKO, "__network-tunnel"], text=True, capture_output=True, check=True, timeout=35,
        input=json.dumps(dict(operation=operation, interface=INTERFACE, **kwargs)))
    assert not response.stderr, response.stderr
    reply = json.loads(response.stdout)
    assert (reply["error"] is None) == ok, reply
    return reply


assert os.geteuid() == 0 and Path("/.dockerenv").exists()
if not Path("/dev/net/tun").exists():
    Path("/dev/net").mkdir(exist_ok=True)
    os.mknod("/dev/net/tun", stat.S_IFCHR | 0o600, os.makedev(10, 200))
hosts = Path("/etc/hosts").read_bytes()
resolver = Path("/etc/resolv.conf").read_bytes()
Path("/run/dbus").mkdir(exist_ok=True)
command("dbus-daemon", "--system", "--fork")
resolved_user = pwd.getpwnam("systemd-resolve")
Path("/run/systemd/resolve").mkdir(parents=True, exist_ok=True)
os.chown("/run/systemd/resolve", resolved_user.pw_uid, resolved_user.pw_gid)
resolved_log = tempfile.TemporaryFile(mode="w+")
resolved = subprocess.Popen(["/usr/lib/systemd/systemd-resolved"], stdout=subprocess.DEVNULL, stderr=resolved_log)
deadline = time.monotonic() + 10
while subprocess.run(["resolvectl", "status"], capture_output=True).returncode:
    if time.monotonic() >= deadline:
        resolved_log.seek(0)
        raise AssertionError(resolved_log.read())
    time.sleep(0.1)
Path("/etc/resolv.conf").write_text("nameserver 127.0.0.53\n")
command("ip", "netns", "add", "peer")
command("ip", "link", "add", "underlay", "type", "veth", "peer", "name", "peer-underlay")
command("ip", "link", "set", "peer-underlay", "netns", "peer")
command("ip", "addr", "add", "192.0.2.1/24", "dev", "underlay")
command("ip", "link", "set", "underlay", "up")
for args in [("addr", "add", "192.0.2.2/24", "dev", "peer-underlay"), ("link", "set", "peer-underlay", "up"), ("link", "set", "lo", "up")]:
    command("ip", "-n", "peer", *args)
command("ip", "netns", "exec", "peer", "wireguard-go", "wg-peer")
with tempfile.NamedTemporaryFile() as private:
    private.write(key(PRIVATE_A).encode())
    private.flush()
    command("ip", "netns", "exec", "peer", "wg", "set", "wg-peer", "private-key", private.name, "listen-port", "51820", "peer", key(PUBLIC_B), "allowed-ips", "fd42::2/128")
command("ip", "-n", "peer", "addr", "add", "fd42::1/128", "dev", "wg-peer")
command("ip", "-n", "peer", "link", "set", "wg-peer", "up")
command("ip", "-n", "peer", "-6", "route", "add", "fd42::2/128", "dev", "wg-peer")
dns = subprocess.Popen(["ip", "netns", "exec", "peer", "python3", "-u", "-c", '''
import socket, struct
s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
s.bind(("fd42::1", 53))
print("ready", flush=True)
while True:
    data, addr = s.recvfrom(4096)
    pos = 12
    while data[pos]: pos += data[pos] + 1
    end = pos + 5
    aaaa = data[pos+1:pos+3] == b"\\x00\\x1c"
    header = data[:2] + struct.pack("!HHHHH", 0x8180, 1, int(aaaa), 0, 0)
    answer = b"\\xc0\\x0c" + struct.pack("!HHIH", 28, 1, 0, 16) + socket.inet_pton(socket.AF_INET6, "fd42::1") if aaaa else b""
    s.sendto(header + data[12:end] + answer, addr)
'''], stdout=subprocess.PIPE, text=True)
assert dns.stdout.readline().strip() == "ready"
configuration = f"[Interface]\nPrivateKey = {key(PRIVATE_B)}\nAddress = fd42::2/128\n\n[Peer]\nPublicKey = {key(PUBLIC_A)}\nAllowedIPs = fd42::1/128\nEndpoint = 192.0.2.2:51820\nPersistentKeepalive = 25\n"
request = dict(network=NETWORK, configuration=configuration, names=[dict(name="foobar", address="fd42::1")])
try:
    # A host-name conflict rolls back DNS and the interface, preserving the file.
    helper("start", ok=False, **{**request, "names": [dict(name="localhost", address="fd42::1")]})
    assert helper("inspect")["public_key"] is None
    assert Path("/etc/hosts").read_bytes() == hosts
    helper("start", **request)
    assert "fd42::1" in command("resolvectl", "dns", INTERFACE)
    assert f"{NETWORK}.tako.internal" in command("resolvectl", "domain", INTERFACE)
    assert "no" in command("resolvectl", "default-route", INTERFACE)
    for name in ["foobar", f"foobar.{NETWORK}.tako.internal", f"shop.production.{NETWORK}.tako.internal"]:
        assert socket.getaddrinfo(name, 22, socket.AF_INET6)[0][4][0] == "fd42::1", name
    helper("sync", **request)
finally:
    helper("stop", public_key=key(PUBLIC_B))
    dns.terminate()
    dns.wait(timeout=5)
    Path("/etc/resolv.conf").write_bytes(resolver)
    command("ip", "netns", "exec", "peer", "ip", "link", "delete", "wg-peer")
    command("ip", "netns", "delete", "peer")
assert Path("/etc/hosts").read_bytes() == hosts
assert INTERFACE not in command("resolvectl", "dns")
assert helper("inspect")["public_key"] is None
resolved.terminate()
resolved.wait(timeout=5)
print("Private names, real split DNS, encrypted transport, conflict rollback, and cleanup passed")
