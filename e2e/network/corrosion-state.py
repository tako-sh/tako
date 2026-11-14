#!/usr/bin/env python3
"""Verify schemas and replication against the pinned Corrosion release.

Run with CORROSION_BIN=/path/to/corrosion just test corrosion.
The two disposable agents use loopback plaintext gossip only for this test;
production continues to require authenticated TLS over WireGuard.
"""

import ast
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

binary = os.environ.get("CORROSION_BIN")
if not binary:
    raise SystemExit(
        "Set CORROSION_BIN to the checksum-verified Corrosion binary installed by Tako"
    )
repository = Path(__file__).resolve().parents[2]
root = Path(tempfile.mkdtemp(prefix="corrosion-shared-"))
processes = []


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


ports = [(port(), port()) for _ in range(2)]
schema = []
for file, const in [
    ("corrosion.rs", "CORROSION_SCHEMA"),
    ("services.rs", "SERVICE_SCHEMA"),
    ("shared.rs", "SHARED_SCHEMA"),
]:
    text = (repository / "tako-network" / "src" / file).read_text()
    literal = re.search(
        r"pub const " + const + r': &str = (".*?");', text, re.DOTALL
    ).group(1)
    schema.append(ast.literal_eval(literal.replace("\\\n", "")))
http = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def post(i, path, value):
    req = urllib.request.Request(
        f"http://127.0.0.1:{ports[i][0]}{path}",
        data=json.dumps(value).encode(),
        headers={
            "Content-Type": "application/json",
            "Authorization": "Bearer local-corrosion-test-token",
        },
    )
    with http.open(req, timeout=2) as response:
        return response.read().decode()


def wait_ready(i):
    for attempt in range(100):
        if processes[i].poll() is not None:
            raise RuntimeError(f"Corrosion {i} exited")
        try:
            post(i, "/v1/queries", ["SELECT count(*) FROM tako_shared_records", []])
            return
        except Exception:
            if attempt == 99:
                raise
            time.sleep(0.1)


try:
    for i, (api, gossip) in enumerate(ports):
        d = root / str(i)
        d.mkdir()
        (d / "schema.sql").write_text("\n".join(schema))
        (d / "config.toml").write_text(f'''[db]
path = "{d}/db.sqlite"
schema_paths = ["{d}/schema.sql"]
[api]
addr = "127.0.0.1:{api}"
authz.bearer-token = "local-corrosion-test-token"
[gossip]
addr = "127.0.0.1:{gossip}"
client_addr = "127.0.0.1:0"
bootstrap = ["127.0.0.1:{ports[1 - i][1]}"]
plaintext = true
[admin]
path = "{d}/admin.sock"
''')
        with open(d / "agent.log", "w") as log:
            processes.append(
                subprocess.Popen(
                    [binary, "-c", str(d / "config.toml"), "agent"],
                    stdout=log,
                    stderr=log,
                )
            )
    for i in range(2):
        wait_ready(i)
    expected = []
    for i in range(2):
        # Preserve both variants of a conflicting operation. The application
        # rejects the conflict; the CRDT must not silently discard the evidence.
        for operation in (f"independent-{i}", "conflicting-operation"):
            value = f"ciphertext-{operation}-{i}"
            expected.append(value)
            digest = hashlib.sha256(value.encode()).hexdigest()
            result = post(
                i,
                "/v1/transactions",
                [
                    [
                        "INSERT OR IGNORE INTO tako_shared_records (kind,record_id,digest,payload) VALUES (?,?,?,?)",
                        ["secret", operation, digest, value],
                    ]
                ],
            )
            assert "error" not in result, result
    for _ in range(100):
        result = [
            post(
                i,
                "/v1/queries",
                [
                    "SELECT record_id,payload FROM tako_shared_records ORDER BY record_id",
                    [],
                ],
            )
            for i in range(2)
        ]
        if all(all(value in response for value in expected) for response in result):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError(result)

    # Membership/CA changes restart Corrosion. A spawned process is not yet an
    # HTTP-ready store; wait for reads and verify its durable records survive.
    processes[0].terminate()
    processes[0].wait(timeout=20)
    with (root / "0" / "agent.log").open("a") as log:
        processes[0] = subprocess.Popen(
            [binary, "-c", str(root / "0" / "config.toml"), "agent"],
            stdout=log,
            stderr=log,
        )
    wait_ready(0)
    restored = post(
        0,
        "/v1/queries",
        ["SELECT record_id,payload FROM tako_shared_records ORDER BY record_id", []],
    )
    assert all(value in restored for value in expected), restored

    # Replacing an observed value inserts the winner before deleting only the
    # observed candidates. An unseen concurrent candidate on the other server
    # must survive until the next reconciliation, including across restart.
    winner = "current-secret-ciphertext"
    concurrent = "unseen-concurrent-ciphertext"
    for i, operation, value in [(0, "winner", winner), (1, "concurrent", concurrent)]:
        digest = hashlib.sha256(value.encode()).hexdigest()
        statements = [[
            "INSERT OR IGNORE INTO tako_shared_records (kind,record_id,digest,payload) VALUES (?,?,?,?)",
            ["secret", operation, digest, value],
        ]]
        if i == 0:
            for obsolete in expected:
                statements.append([
                    "DELETE FROM tako_shared_records WHERE kind=? AND digest=?",
                    ["secret", hashlib.sha256(obsolete.encode()).hexdigest()],
                ])
        result = post(i, "/v1/transactions", statements)
        assert "error" not in result, result
    for _ in range(100):
        result = [post(i, "/v1/queries", ["SELECT payload FROM tako_shared_records", []]) for i in range(2)]
        if all(winner in response and concurrent in response and
               not any(value in response for value in expected) for response in result):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError(result)
    # Once both versions are observed, retire the losing candidate on one
    # server and verify the remote SQL view has just the winning payload.
    post(1, "/v1/transactions", [["DELETE FROM tako_shared_records WHERE record_id=?", ["concurrent"]]])
    for _ in range(100):
        result = [post(i, "/v1/queries", ["SELECT payload FROM tako_shared_records", []]) for i in range(2)]
        if all(winner in response and concurrent not in response for response in result):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError(result)
    print(
        "PASS: actual Corrosion accepts schemas, replicates conflicting operations, recovers after restart, and retires observed ciphertext without losing concurrent candidates"
    )
except Exception:
    for i in range(len(processes)):
        print(
            f"--- Corrosion {i} ---\n"
            + (root / str(i) / "agent.log").read_text()[-4000:]
        )
    raise
finally:
    for p in processes:
        p.terminate()
    for p in processes:
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    shutil.rmtree(root)
