#!/usr/bin/env python3
"""Real TCP key/value service and independent acknowledgement checker."""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import socketserver
import time

TOKEN = re.compile(r"[A-Za-z0-9/_-]+\Z")
BATCH = 32
PROBES = 4
LOAD_TIMEOUT = 10
CONTROL_TIMEOUT = LOAD_TIMEOUT + 2
PUT_TIMEOUT = 0.2
RETRY_DELAY = 0.02


def remaining(deadline):
    budget = deadline - time.monotonic()
    if budget <= 0:
        raise TimeoutError("operation deadline expired")
    return budget


def resolve(address):
    host, port = address.rsplit(":", 1)
    return socket.getaddrinfo(host, int(port), type=socket.SOCK_STREAM)


def bounded_exchange(endpoints, message, timeout, deadline):
    # One attempt budget covers connect, send and every receive, including trickle.
    end = min(deadline, time.monotonic() + timeout)
    last_error = None
    for family, kind, protocol, _, endpoint in endpoints:
        with socket.socket(family, kind, protocol) as channel:
            try:
                channel.settimeout(remaining(end))
                channel.connect(endpoint)
            except OSError as error:
                last_error = error
                continue
            channel.settimeout(remaining(end))
            channel.sendall(message.encode() + b"\n")
            line = bytearray()
            while b"\n" not in line:
                channel.settimeout(remaining(end))
                data = channel.recv(8193 - len(line))
                if not data:
                    raise ConnectionError("peer disconnected before its response")
                line.extend(data)
                if b"\n" not in line and len(line) >= 8193:
                    raise ValueError("missing or oversized protocol line")
            line = line[:line.index(b"\n") + 1]
            if len(line) > 8192:
                raise ValueError("missing or oversized protocol line")
            remaining(end)
            return line.decode().strip()
    if last_error is not None:
        raise last_error
    raise ConnectionError("no service endpoints")


def exchange(address, message, timeout=PUT_TIMEOUT, *, deadline=None, endpoints=None):
    if deadline is not None:
        return bounded_exchange(endpoints, message, timeout, deadline)
    host, port = address.rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout) as channel:
        channel.settimeout(timeout)
        channel.sendall(message.encode() + b"\n")
        with channel.makefile("rb") as reader:
            line = reader.readline(8193)
            if not line:
                raise ConnectionError("peer disconnected before its response")
            if not line.endswith(b"\n") or len(line) > 8192:
                raise ValueError("missing or oversized protocol line")
            return line.decode().strip()


class Store:
    def __init__(self, path, broken):
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        epoch = path.with_suffix(".epoch")
        self.generation = int(epoch.read_text()) + 1 if epoch.exists() else 1
        temporary = epoch.with_suffix(".epoch-next")
        with temporary.open("w") as target:
            target.write(f"{self.generation}\n")
            target.flush()
            os.fsync(target.fileno())
        temporary.replace(epoch)
        self.values = {}
        self.log = None
        if not broken:
            data = path.read_bytes() if path.exists() else b""
            complete = data.rfind(b"\n") + 1
            for record in data[:complete].decode().splitlines():
                key, value = record.split(" ")
                if not TOKEN.fullmatch(key) or not TOKEN.fullmatch(value):
                    raise ValueError("invalid saved record")
                self.values[key] = value
            self.log = path.open("ab")
            self.log.truncate(complete)
        print(f"SERVER_READY generation={self.generation} recovered={len(self.values)} broken={str(broken).lower()}", flush=True)

    def reply(self, request):
        parts = request.split(" ")
        if parts == ["PING"]:
            return f"PONG {self.generation}"
        if len(parts) == 3 and parts[0] == "PUT" and all(TOKEN.fullmatch(p) for p in parts[1:]):
            _, key, value = parts
            if self.log:
                self.log.write(f"{key} {value}\n".encode())
                self.log.flush()
                os.fsync(self.log.fileno())
            self.values[key] = value
            return f"OK {self.generation}"
        if len(parts) == 2 and parts[0] == "GET" and TOKEN.fullmatch(parts[1]):
            value = self.values.get(parts[1])
            return f"VALUE {self.generation} {value}" if value is not None else f"MISSING {self.generation}"
        raise ValueError("invalid request")


class Listener(socketserver.TCPServer):
    allow_reuse_address = True


def serve(address, reply):
    class Handler(socketserver.StreamRequestHandler):
        def handle(self):
            self.connection.settimeout(15)
            try:
                line = self.rfile.readline(8193)
                if not line.endswith(b"\n") or len(line) > 8192:
                    return
                response = reply(line.decode().strip())
                self.wfile.write(response.encode() + b"\n")
            except (OSError, ValueError) as error:
                print(f"CONNECTION_ERROR {error}", flush=True)
    host, port = address.rsplit(":", 1)
    with Listener((host, int(port)), Handler) as listener:
        listener.serve_forever()


class Client:
    """The ledger contains only PUT replies actually received from the server."""
    def __init__(self, address, batch=BATCH):
        if not 1 <= batch <= BATCH:
            raise ValueError("batch must be between 1 and 32")
        self.batch = batch
        self.address = address
        # Resolve before admitting commands; LOAD never performs name resolution.
        self.endpoints = resolve(address)
        self.acks = []
        self.sequence = 0
        self.failed = 0
        self.result = None

    def put(self, expected=None, deadline=None):
        sequence = self.sequence
        key, value = f"k/{sequence:06}", f"v{sequence:06}"
        if deadline is not None:
            remaining(deadline)
        response = exchange(self.address, f"PUT {key} {value}",
                            deadline=deadline, endpoints=self.endpoints)
        prefix, generation = response.split(" ")
        if prefix != "OK":
            raise ValueError(f"unexpected PUT reply {response}")
        generation = int(generation)
        if expected is not None and generation != expected:
            raise ValueError(f"expected generation {expected}, received {generation}")
        if deadline is not None:
            remaining(deadline)
        ack = (key, value, generation)
        self.acks.append(ack)
        self.sequence += 1
        return ack

    def retry(self, command, deadline):
        while True:
            try:
                return exchange(self.address, command)
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.02)

    def reply(self, command):
        if command == "STATUS":
            return json.dumps({"acked": len(self.acks), "failed": self.failed})
        if self.result is not None:
            if command == "VERIFY":
                return json.dumps(self.result)
            raise ValueError("workload already verified")
        if command.startswith("LOAD "):
            expected = int(command.split()[1])
            before = len(self.acks)
            deadline = time.monotonic() + LOAD_TIMEOUT
            while len(self.acks) - before < self.batch:
                remaining(deadline)
                try:
                    self.put(expected, deadline)
                except OSError:
                    self.failed += 1
                    time.sleep(min(RETRY_DELAY, remaining(deadline)))
                remaining(deadline)
            print(f"ACK_PHASE generation={expected} acked={self.batch} total={len(self.acks)}", flush=True)
            return json.dumps({"generation": expected, "acked": self.batch, "total": len(self.acks)})
        if command == "PROBE":
            before, failed = len(self.acks), 0
            for _ in range(PROBES):
                try:
                    self.put()
                except OSError:
                    self.failed += 1
                    failed += 1
            result = {"attempts": PROBES, "failed": failed, "acked": len(self.acks) - before}
            print("PARTITION_PROBE " + json.dumps(result, sort_keys=True), flush=True)
            return json.dumps(result)
        if command == "VERIFY":
            deadline = time.monotonic() + 10
            response = self.retry("PING", deadline)
            prefix, generation = response.split(" ")
            if prefix != "PONG":
                raise ValueError(f"unexpected PING reply {response}")
            generation = int(generation)
            missing = 0
            for key, value, acknowledged_generation in self.acks:
                response = self.retry(f"GET {key}", deadline)
                if response == f"MISSING {generation}":
                    missing += 1
                    print(f"MISSING_ACK key={key} expected={value} acknowledged_generation={acknowledged_generation}", flush=True)
                elif response != f"VALUE {generation} {value}":
                    raise ValueError(f"unexpected GET reply {response}")
            pre = sum(ack[2] == 1 for ack in self.acks)
            post = sum(ack[2] == generation for ack in self.acks)
            status = "fail" if missing else "pass"
            if not self.acks or not pre or not post:
                raise ValueError("empty acknowledgement phase")
            self.result = dict(status=status, acked=len(self.acks), pre_restart=pre,
                               post_restart=post, generation=generation, missing=missing)
            print(f"RESULT {status} acked={len(self.acks)} pre_restart={pre} post_restart={post} generation={generation} missing={missing}", flush=True)
            return json.dumps(self.result)
        raise ValueError("expected LOAD, PROBE or VERIFY")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["server", "client", "scenario"])
    parser.add_argument("--listen", default="0.0.0.0:7000")
    parser.add_argument("--state", default="/data/kv.log")
    parser.add_argument("--address", default="server:7000")
    parser.add_argument("--control-address", default="client:7001")
    parser.add_argument("--broken", action="store_true")
    parser.add_argument("--no-faults", action="store_true")
    parser.add_argument("--batch", type=int, choices=range(1, BATCH + 1), default=BATCH,
                        help="Acknowledged writes per LOAD (default: 32).")
    args = parser.parse_args()
    if args.mode == "server":
        store = Store(args.state, args.broken)
        serve(args.listen, store.reply)
    elif args.mode == "client":
        client = Client(args.address, args.batch)
        serve(args.listen, client.reply)
    else:
        from scenario import run
        return run(args.control_address, args.no_faults, args.batch)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
