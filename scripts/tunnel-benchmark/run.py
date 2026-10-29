#!/usr/bin/env python3
"""Run paired before/after benchmarks inside a dedicated Docker container."""
import argparse
import json
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--container', required=True)
parser.add_argument('--before', required=True, help='baseline binary path inside container')
parser.add_argument('--after', required=True, help='candidate binary path inside container')
parser.add_argument('--impair', required=True, help='impair.py path inside container')
parser.add_argument('--output', required=True)
parser.add_argument('--chunk-bytes', type=int, default=8192)
parser.add_argument('--body-bytes', type=int, default=2097152)
parser.add_argument('--rounds', type=int, default=3)
args = parser.parse_args()
base = ['docker', 'exec', args.container]
chain = 'TAKO_BENCH'
results = []


def iptables(*arguments):
    subprocess.run([*base, 'iptables', *arguments], check=True)


iptables('-N', chain)
try:
    iptables('-I', 'OUTPUT', '-j', chain)
    for direction in ['-s', '-d']:
        iptables('-A', chain, direction, '127.0.0.2', '-j', 'NFQUEUE', '--queue-num', '42', '--queue-bypass')
    for label, delay, loss in [('local', 0, 0), ('100ms', 50, 0), ('100ms-loss1pct', 50, 0.01)]:
        for concurrency in [1, 8]:
            for repeat in range(args.rounds):
                # Alternate order to reduce systematic warmup/load bias.
                variants = [('before', args.before), ('after', args.after)]
                if repeat % 2: variants.reverse()
                for version, binary in variants:
                    process = subprocess.Popen([*base, 'python3', args.impair, '--delay-ms', str(delay), '--loss', str(loss), '--seed', str(repeat + 1)], stdout=subprocess.PIPE, text=True)
                    ready = process.stdout.readline().strip().split()
                    if len(ready) != 2 or ready[0] != 'READY':
                        process.wait(timeout=10)
                        raise RuntimeError('packet impairment did not start')
                    try:
                        run = subprocess.run(['docker', 'exec', '-e', f'TUNNEL_BENCH_CONCURRENCY={concurrency}', '-e', f'TUNNEL_BENCH_CHUNK_BYTES={args.chunk_bytes}', '-e', f'TUNNEL_BENCH_BODY_BYTES={args.body_bytes}', args.container, binary, 'concurrent_transport', '--ignored', '--nocapture'], capture_output=True, text=True, timeout=130)
                        if run.returncode:
                            raise RuntimeError(run.stdout + run.stderr)
                        row = json.loads(next(line.removeprefix('TUNNEL_BENCH ') for line in run.stdout.splitlines() if line.startswith('TUNNEL_BENCH ')))
                    finally:
                        subprocess.run([*base, 'kill', '-TERM', ready[1]], check=True)
                        remaining, _ = process.communicate(timeout=10)
                    stats = json.loads(remaining.strip())
                    row.update(profile=label, version=version, repeat=repeat + 1, **stats)
                    results.append(row)
                    with open(args.output, 'w') as out: json.dump(results, out, indent=2)
                    print(json.dumps(row), flush=True)
                    time.sleep(0.1)
finally:
    iptables('-D', 'OUTPUT', '-j', chain)
    iptables('-F', chain)
    iptables('-X', chain)
