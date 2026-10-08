"""Guest-owned bandwidth/queue evidence over ordinary UDP and TCP traffic."""
import json
import socket
import sys
import time


def connect():
    deadline = time.monotonic() + 10
    while True:
        channel = socket.socket()
        channel.settimeout(2)
        try:
            channel.connect(("client", 9000))
            return channel
        except OSError:
            channel.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def client():
    with socket.socket() as listener, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        listener.bind(("0.0.0.0", 9000))
        listener.listen()
        udp.settimeout(2)
        udp.connect(("server", 8000))
        with listener.accept()[0] as channel:
            for line in channel.makefile("rb"):
                op = json.loads(line)["op"]
                if op == "done":
                    return
                if op == "burst":
                    for _ in range(256):
                        udp.send(b"b" * 1400)
                    result = {"sent": 256}
                else:
                    marker = op.encode() + bytes(1400 - len(op))
                    start = time.monotonic_ns()
                    udp.send(marker)
                    while udp.recv(2048) != marker:
                        pass  # Drain earlier burst echoes after healing.
                    result = {"elapsed_ns": time.monotonic_ns() - start}
                channel.sendall(json.dumps(result).encode() + b"\n")


def scenario():
    from seedbox import World
    with connect() as channel, World.connect() as world:
        channel.settimeout(10)
        reader = channel.makefile("rb")

        def phase(op):
            channel.sendall(json.dumps({"op": op}).encode() + b"\n")
            return json.loads(reader.readline())

        phase("ready")  # Establish both ARP directions before applying the shape.
        handle = world.shape(["client"], ["server"], latency=1_000_000,
                             bandwidth_bps=1_000_000, queue_bound=12_000_000)
        measurement = phase("measure")
        assert measurement["elapsed_ns"] >= 1400 * 8 * 1000, measurement
        assert phase("burst") == {"sent": 256}
        world.sleep(0.05)
        evidence = world.heal(handle)["queue_counts"]
        assert evidence["matched"] >= 256, evidence
        assert evidence["queue_dropped"] > 0, evidence
        assert evidence["delayed_max_ns"] > 12_000_000, evidence
        phase("healed")
        inspection = world.inspect()
        assert not inspection["active_handles"], inspection
        pair = f'{inspection["services"]["client"]}->{inspection["services"]["server"]}'
        assert inspection["network_counters"][pair]["queue_dropped"] == evidence["queue_dropped"], inspection
        channel.sendall(b'{"op":"done"}\n')
        print(json.dumps({"measurement": measurement, "queue_counts": evidence,
                          "healed_exchange": True}, sort_keys=True))


if __name__ == "__main__":
    role = sys.argv[1]
    if role == "server":
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            udp.bind(("0.0.0.0", 8000))
            while True:
                data, address = udp.recvfrom(2048)
                udp.sendto(data, address)
    elif role == "client":
        client()
    elif role == "scenario":
        scenario()
    else:
        raise ValueError(role)
