"""Gossip node for the seedbox determinism counterexample (see README.md).

Single thread, one selector. Sends UDP datagrams to the three peers every
200 us, a 64-byte TCP record to the ring neighbour every 1 ms, logs every
received record with its local receive time, and flushes the log to /data/log
with fsync every 50 ms. After 3 s it prints counts and digests and exits 0.
"""
import errno
import hashlib
import os
import selectors
import socket
import sys
import time

NODES = ["n1", "n2", "n3", "n4"]
UDP_PORT, TCP_PORT = 9000, 9001
UDP_PERIOD, TCP_PERIOD = 200_000, 1_000_000
FLUSH_PERIOD, RETRY_PERIOD, RUN_NS = 50_000_000, 10_000_000, 3_000_000_000
REC = 64
SOFT = (errno.EAGAIN, errno.EWOULDBLOCK, errno.ECONNREFUSED, errno.EHOSTUNREACH,
        errno.ENETUNREACH, errno.ENOBUFS)
GONE = (errno.ECONNRESET, errno.EPIPE, errno.ECONNABORTED, errno.ENOTCONN)

name = sys.argv[1]
peers = [n for n in NODES if n != name]
neighbour = NODES[(NODES.index(name) + 1) % len(NODES)]
now = time.monotonic_ns


def say(line):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def resolve(host):
    while True:
        try:
            return socket.gethostbyname(host)
        except OSError:
            time.sleep(0.01)


addrs = {p: resolve(p) for p in peers}
sel = selectors.DefaultSelector()

udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind(("0.0.0.0", UDP_PORT))
udp.setblocking(False)
sel.register(udp, selectors.EVENT_READ, "udp")

lsn = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
lsn.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
lsn.bind(("0.0.0.0", TCP_PORT))
lsn.listen(4)
lsn.setblocking(False)
sel.register(lsn, selectors.EVENT_READ, "listen")

log = []            # every received record, in processing order
flushed = 0         # lines of `log` already written to /data/log
inbound = {}        # accepted socket -> partial bytes
out = None          # outgoing TCP socket to the neighbour
out_state = "idle"  # idle -> connecting -> up -> gone
out_buf = b""
counts = {"udp_received": 0, "tcp_received": 0, "udp_sent": 0, "udp_send_errors": 0,
          "tcp_sent": 0, "tcp_connect_attempts": 0}
fh = open("/data/log", "wb")


def flush_log():
    global flushed
    if flushed < len(log):
        fh.write("".join(l + "\n" for l in log[flushed:]).encode())
        flushed = len(log)
    fh.flush()
    os.fsync(fh.fileno())


def try_connect():
    global out, out_state
    counts["tcp_connect_attempts"] += 1
    out = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    out.setblocking(False)
    rc = out.connect_ex((addrs[neighbour], TCP_PORT))
    if rc in (0, errno.EINPROGRESS):
        out_state = "connecting"
        sel.register(out, selectors.EVENT_WRITE, "out")
    else:
        out.close()
        out, out_state = None, "idle"


def drop_out(state):
    global out, out_state, out_buf
    if out is not None:
        try:
            sel.unregister(out)
        except (KeyError, ValueError):
            pass
        out.close()
    out, out_state, out_buf = None, state, b""


def pump_out():
    global out_buf
    while out_buf:
        try:
            n = out.send(out_buf)
        except OSError as e:
            if e.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                break
            drop_out("gone")  # neighbour finished; do not reconnect
            return
        out_buf = out_buf[n:]
    sel.modify(out, selectors.EVENT_READ | (selectors.EVENT_WRITE if out_buf else 0), "out")


def on_udp():
    while True:
        try:
            data = udp.recv(256)
        except OSError as e:
            if e.errno in SOFT:
                return
            raise
        t = now()
        sender, seq, _ = data.decode().split()
        log.append(f"udp {sender} {seq} {t}")
        counts["udp_received"] += 1


def on_inbound(conn):
    try:
        data = conn.recv(65536)
    except OSError as e:
        if e.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
            return
        data = b""
    if not data:
        sel.unregister(conn)
        conn.close()
        inbound.pop(conn, None)
        return
    t = now()
    buf = inbound[conn] + data
    while len(buf) >= REC:
        sender, seq, _ = buf[:REC].decode().split()
        log.append(f"tcp {sender} {seq} {t}")
        counts["tcp_received"] += 1
        buf = buf[REC:]
    inbound[conn] = buf


start = now()
end = start + RUN_NS
next_udp = next_tcp = start
next_flush = start + FLUSH_PERIOD
next_retry = start
udp_seq = tcp_seq = 0

while True:
    t = now()
    if t >= end:
        break
    if out_state == "idle" and t >= next_retry:
        try_connect()
        next_retry = t + RETRY_PERIOD
    while t >= next_udp:
        msg = f"{name} {udp_seq} {t}".encode()
        for p in peers:
            try:
                udp.sendto(msg, (addrs[p], UDP_PORT))
                counts["udp_sent"] += 1
            except OSError as e:
                if e.errno not in SOFT:
                    raise
                counts["udp_send_errors"] += 1
        udp_seq += 1
        next_udp += UDP_PERIOD
    while t >= next_tcp:
        if out_state == "up":
            out_buf += f"{name} {tcp_seq} {t}".encode().ljust(REC - 1) + b"\n"
            counts["tcp_sent"] += 1
            tcp_seq += 1
        next_tcp += TCP_PERIOD
    if out_state == "up" and out_buf:
        pump_out()
    if t >= next_flush:
        flush_log()
        next_flush += FLUSH_PERIOD
    deadline = min(next_udp, next_tcp, next_flush, end)
    if out_state == "idle":
        deadline = min(deadline, next_retry)
    for key, ev in sel.select(max(0, deadline - now()) / 1e9):
        tag, sock = key.data, key.fileobj
        if tag == "udp":
            on_udp()
        elif tag == "listen":
            try:
                conn, _ = lsn.accept()
            except OSError:
                continue
            conn.setblocking(False)
            inbound[conn] = b""
            sel.register(conn, selectors.EVENT_READ, "in")
        elif tag == "in":
            on_inbound(sock)
        elif tag == "out" and out_state == "connecting":
            err = out.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
            if err == 0:
                out_state = "up"
                sel.modify(out, selectors.EVENT_READ, "out")
            else:
                drop_out("idle")
        elif tag == "out":
            try:
                gone = ev & selectors.EVENT_READ and not out.recv(64)
            except OSError as e:
                gone = e.errno in GONE or e.errno == errno.ECONNREFUSED
            if gone:
                drop_out("gone")
            elif out_buf:
                pump_out()

flush_log()
fh.close()
say(f"node: {name}")
for k, v in counts.items():
    say(f"{k}: {v}")
say(f"tcp_out_state: {out_state}")
say("first_20:")
for line in log[:20]:
    say(f"  {line}")
say("digest: " + hashlib.sha256("".join(l + "\n" for l in log).encode()).hexdigest())
with open("/data/log", "rb") as f:
    say("file_digest: " + hashlib.sha256(f.read()).hexdigest())
sys.exit(0)
