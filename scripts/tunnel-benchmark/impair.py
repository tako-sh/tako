#!/usr/bin/env python3
"""Delay/drop actual IP packets using NFQUEUE in an isolated benchmark namespace.

Requires libnetfilter_queue, CAP_NET_ADMIN, and OUTPUT rules for queue 42.
Only use with the loopback-only rules documented in README.md.
"""
import argparse
import ctypes as c
import heapq
import json
import os
import random
import select
import signal
import socket
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--delay-ms', type=float, default=50)
parser.add_argument('--loss', type=float, default=0)
parser.add_argument('--seed', type=int, default=1)
args = parser.parse_args()
assert args.delay_ms >= 0 and 0 <= args.loss < 1
rng = random.Random(args.seed)
lib = c.CDLL('libnetfilter_queue.so.1', use_errno=True)
callback_type = c.CFUNCTYPE(c.c_int, c.c_void_p, c.c_void_p, c.c_void_p, c.c_void_p)


def signature(name, result, *arguments):
    fn = getattr(lib, name)
    fn.restype = result
    fn.argtypes = arguments
    return fn


open_queue = signature('nfq_open', c.c_void_p)
bind = signature('nfq_bind_pf', c.c_int, c.c_void_p, c.c_uint16)
create = signature('nfq_create_queue', c.c_void_p, c.c_void_p, c.c_uint16, callback_type, c.c_void_p)
mode = signature('nfq_set_mode', c.c_int, c.c_void_p, c.c_uint8, c.c_uint32)
maxlen = signature('nfq_set_queue_maxlen', c.c_int, c.c_void_p, c.c_uint32)
fd = signature('nfq_fd', c.c_int, c.c_void_p)
handle = signature('nfq_handle_packet', c.c_int, c.c_void_p, c.c_char_p, c.c_int)
header = signature('nfq_get_msg_packet_hdr', c.POINTER(c.c_uint32), c.c_void_p)
verdict = signature('nfq_set_verdict', c.c_int, c.c_void_p, c.c_uint32, c.c_uint32, c.c_uint32, c.c_void_p)
destroy = signature('nfq_destroy_queue', c.c_int, c.c_void_p)
close = signature('nfq_close', c.c_int, c.c_void_p)
pending = []
stats = {'packets': 0, 'dropped': 0}
running = True


@callback_type
def packet(queue, _message, data, _context):
    packet_id = socket.ntohl(header(data)[0])
    stats['packets'] += 1
    if rng.random() < args.loss:
        stats['dropped'] += 1
        verdict(queue, packet_id, 0, 0, None)  # NF_DROP; TCP must retransmit.
    else:
        heapq.heappush(pending, (time.monotonic() + args.delay_ms / 1000, packet_id))
    return 0


def stop(_signal, _frame):
    global running
    running = False


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
h = open_queue()
assert h and bind(h, socket.AF_INET) == 0, 'NFQUEUE unavailable'
q = create(h, 42, packet, None)
assert q and mode(q, 2, 65535) == 0 and maxlen(q, 16384) == 0, 'NFQUEUE setup failed'
sock = socket.socket(fileno=fd(h))
sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
print(f'READY {os.getpid()}', flush=True)
try:
    while running:
        now = time.monotonic()
        while pending and pending[0][0] <= now:
            _, packet_id = heapq.heappop(pending)
            assert verdict(q, packet_id, 1, 0, None) >= 0
        timeout = max(0, pending[0][0] - now) if pending else 0.1
        if select.select([sock], [], [], min(timeout, 0.1))[0]:
            data = sock.recv(262144)
            assert handle(h, data, len(data)) >= 0
finally:
    for _, packet_id in pending:
        verdict(q, packet_id, 1, 0, None)
    sock.detach()
    destroy(q)
    close(h)
    print(json.dumps(stats), flush=True)
