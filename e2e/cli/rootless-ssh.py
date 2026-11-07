#!/usr/bin/env python3
"""Run the ignored userspace transport test against a reference WireGuard peer.

Run only inside a disposable privileged container with iproute2, wireguard-tools,
Python, and libvips (wireguard-go when the kernel lacks WireGuard). argv[1] is the Tako Rust test binary. The server setup needs
root; the actual TCP client runs as an unprivileged user, without a TUN device or local route.
"""
import base64
import os
import pwd
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time


def command(*args):
    return subprocess.check_output(args, text=True)


assert sys.platform == "linux" and os.geteuid() == 0 and Path("/.dockerenv").exists()
if not Path("/dev/net/tun").exists():
    Path("/dev/net").mkdir(exist_ok=True)
    os.mknod("/dev/net/tun", stat.S_IFCHR | 0o600, os.makedev(10, 200))
private = bytes.fromhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a")
peer = bytes.fromhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
with tempfile.NamedTemporaryFile() as key:
    key.write(base64.b64encode(private))
    key.flush()
    if subprocess.run(["ip", "link", "add", "wg-test", "type", "wireguard"], capture_output=True).returncode:
        command("wireguard-go", "wg-test")
    try:
        command("wg", "set", "wg-test", "private-key", key.name, "peer", base64.b64encode(peer).decode(), "allowed-ips", "fd42::2/128")
        command("ip", "addr", "add", "fd42::1/128", "dev", "wg-test")
        command("ip", "link", "set", "wg-test", "up")
        command("ip", "-6", "route", "add", "fd42::2/128", "dev", "wg-test")
        port = command("wg", "show", "wg-test", "listen-port").strip()
        with socket.socket(socket.AF_INET6) as listener:
            listener.bind(("fd42::1", 2222))
            listener.listen()

            def echo():
                with listener.accept()[0] as stream:
                    while data := stream.recv(8192):
                        stream.sendall(data)

            worker = threading.Thread(target=echo, daemon=True)
            worker.start()
            # Only the reference server owns a link; the client cannot open TUN.
            if Path("/dev/net/tun").exists():
                os.chmod("/dev/net/tun", 0)
            interfaces = command("ip", "-j", "link")
            routes = command("ip", "-6", "route")
            hosts = Path("/etc/hosts").read_bytes()
            resolver = Path("/etc/resolv.conf").read_bytes()

            def unprivileged():
                # Build artifacts may be private to the non-root build account.
                uid = os.stat(sys.argv[1]).st_uid or 65534
                os.setgroups([])
                os.setgid(uid)
                os.setuid(uid)

            subprocess.run([sys.argv[1], "--exact", "network::ssh_tunnel::tests::rootless_tcp_roundtrip", "--ignored", "--nocapture"],
                env={**os.environ, "TAKO_TEST_WG_ENDPOINT": f"127.0.0.1:{port}"}, preexec_fn=unprivileged, check=True, timeout=40)
            worker.join(timeout=2)
            assert not worker.is_alive()
            assert interfaces == command("ip", "-j", "link")
            assert routes == command("ip", "-6", "route")
            assert hosts == Path("/etc/hosts").read_bytes()
            assert resolver == Path("/etc/resolv.conf").read_bytes()

            # Verify actual OpenSSH multiplexing and the connect/session lock.
            with tempfile.TemporaryDirectory(prefix="tako-ssh-test-") as ssh_root:
                uid = os.stat(sys.argv[1]).st_uid or 65534
                os.chown(ssh_root, uid, uid)
                try:
                    pwd.getpwuid(uid)
                except KeyError:
                    command("useradd", "--uid", str(uid), "--no-create-home", "--home-dir", ssh_root, "tako-test-client")
                client_key = Path(ssh_root, "client")
                host_key = Path(ssh_root, "host")
                known_hosts = Path(ssh_root, "known_hosts")
                for path in [client_key, host_key]:
                    command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(path))
                known_hosts.write_text("[127.0.0.1]:2223 " + host_key.with_suffix(".pub").read_text())
                for path in Path(ssh_root).iterdir():
                    os.chown(path, uid, uid)
                Path("/run/sshd").mkdir(exist_ok=True)
                command("usermod", "-p", "invalid-password-hash", "root")
                sshd = subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", "/dev/null", "-h", str(host_key),
                    "-p", "2223", "-o", "ListenAddress=127.0.0.1", "-o", "PasswordAuthentication=no",
                    "-o", "PermitRootLogin=prohibit-password", "-o", "StrictModes=no", "-o", "UsePAM=no",
                    "-o", f"AuthorizedKeysFile={client_key}.pub"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                try:
                    deadline = time.monotonic() + 5
                    while True:
                        try:
                            with socket.create_connection(("127.0.0.1", 2223), timeout=0.2):
                                break
                        except OSError:
                            assert time.monotonic() < deadline
                            time.sleep(0.05)
                    subprocess.run([sys.argv[1], "--exact", "network::ssh_session::tests::multiplex_and_connect_exclusion", "--ignored", "--nocapture"],
                        env={**os.environ, "TAKO_HOME": ssh_root, "TAKO_TEST_SSH_KEY": str(client_key), "TAKO_TEST_SSH_HOSTS": str(known_hosts)},
                        preexec_fn=unprivileged, check=True, timeout=30)
                finally:
                    sshd.terminate()
                    sshd.wait(timeout=5)
    finally:
        command("ip", "link", "delete", "wg-test")
print("Unprivileged TCP over embedded WireGuard passed; system networking unchanged")
