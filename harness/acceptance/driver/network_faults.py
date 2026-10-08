"""Guest-owned dynamic network faults, packet receipts and actual TCP reset."""
import errno
import json
import socket
import sys
import threading
import time


def connect(host, port):
    deadline = time.monotonic() + 10
    while True:
        channel = socket.socket()
        channel.settimeout(3)
        try:
            channel.connect((host, port))
            return channel
        except OSError:
            channel.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def send(channel, value):
    channel.sendall(json.dumps(value).encode() + b"\n")


def server():
    received = {}
    lock = threading.Lock()

    def udp():
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as channel:
            channel.bind(("0.0.0.0", 8000))
            while True:
                data, address = channel.recvfrom(2048)
                message = json.loads(data)
                with lock:
                    received.setdefault(message["phase"], []).append(message["seq"])
                if message["phase"] in ("ready", "spike", "healed"):
                    channel.sendto(data, address)

    def stats():
        with socket.socket() as listener:
            listener.bind(("0.0.0.0", 9100))
            listener.listen()
            with listener.accept()[0] as channel:
                for line in channel.makefile("rb"):
                    phase = json.loads(line)["phase"]
                    with lock:
                        send(channel, {"received": received.get(phase, [])})

    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 8001))
        listener.listen()
        threading.Thread(target=udp, daemon=True).start()
        threading.Thread(target=stats, daemon=True).start()
        while True:
            with listener.accept()[0] as channel:
                try:
                    while data := channel.recv(2048):
                        channel.sendall(data)
                except OSError:
                    pass  # An injected reset closes this incarnation.


def client():
    with socket.socket() as listener, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        listener.bind(("0.0.0.0", 9000))
        listener.listen()
        udp.settimeout(3)
        udp.connect(("server", 8000))
        tcp = connect("server", 8001)
        try:
            with listener.accept()[0] as channel:
                for line in channel.makefile("rb"):
                    command = json.loads(line)
                    phase = command["phase"]
                    if phase == "done":
                        return
                    if phase in ("ready", "spike", "healed"):
                        marker = json.dumps({"phase": phase, "seq": 0}).encode()
                        start = time.monotonic_ns()
                        udp.send(marker)
                        assert udp.recv(2048) == marker
                        result = {"elapsed_ns": time.monotonic_ns() - start}
                    elif phase == "reset":
                        reset = False
                        try:
                            tcp.sendall(b"reset-probe")
                            assert tcp.recv(2048) == b"reset-probe", "unexpected EOF"
                        except OSError as error:
                            if error.errno != errno.ECONNRESET:
                                raise
                            reset = True
                        result = {"reset": reset}
                    elif phase == "reconnect":
                        tcp.close()
                        tcp = connect("server", 8001)
                        tcp.sendall(b"healed-tcp")
                        assert tcp.recv(2048) == b"healed-tcp"
                        result = {"reconnected": True}
                    else:
                        for seq in range(command["count"]):
                            udp.send(json.dumps({"phase": phase, "seq": seq}).encode())
                        result = {"sent": command["count"]}
                    send(channel, result)
        finally:
            tcp.close()


def scenario():
    from seedbox import World
    with connect("client", 9000) as channel, connect("server", 9100) as stats, World.connect() as world:
        reader, stats_reader = channel.makefile("rb"), stats.makefile("rb")

        def phase(name, **arguments):
            send(channel, {"phase": name, **arguments})
            return json.loads(reader.readline())

        def receipts(name):
            send(stats, {"phase": name})
            return json.loads(stats_reader.readline())["received"]

        phase("ready")  # Warm ARP and establish the TCP incarnation before shaping.
        evidence = {}
        for name, count, settings, counter in [
            ("reorder", 128, {"reorder_ppm": 1_000_000, "reorder_delay": 100_000_000}, "reordered"),
            ("duplicate", 32, {"duplicate_ppm": 1_000_000}, "duplicated"),
            ("burst", 128, {"burst_loss_ppm": 100_000, "burst_length": 4}, "burst_dropped"),
        ]:
            handle = world.shape(["client"], ["server"], latency=1_000_000, **settings)
            assert phase(name, count=count) == {"sent": count}
            world.sleep(0.25)  # All bounded extra delays have passed in simulated time.
            counts = world.heal(handle)["network_fault_counts"]
            observed = receipts(name)
            assert counts[counter] > 0, counts
            if name == "reorder":
                assert sorted(observed) == list(range(count)), observed
                assert any(b < a for a, b in zip(observed, observed[1:])), observed
            elif name == "duplicate":
                assert observed == [seq for seq in range(count) for _ in range(2)], observed
            else:
                missing = set(range(count)) - set(observed)
                assert 0 < len(missing) == counts["burst_dropped"] < count, (missing, counts)
                assert any(all(n + i in missing for i in range(4)) for n in missing), missing
            evidence[name] = {"counts": counts, "received": observed}
            phase("healed")

        handle = world.shape(["client"], ["server"], latency=1_000_000,
                             latency_spike_ppm=1_000_000, latency_spike=50_000_000)
        measurement = phase("spike")
        assert measurement["elapsed_ns"] >= 50_000_000, measurement
        counts = world.heal(handle)["network_fault_counts"]
        assert counts["spiked"] > 0 and counts["delayed_max_ns"] == 50_000_000, counts
        evidence["spike"] = {"counts": counts, **measurement}
        phase("healed")

        handle = world.shape(["server"], ["client"], latency=1_000_000, tcp_reset_ppm=1_000_000)
        assert phase("reset") == {"reset": True}, "socket did not observe a TCP reset"
        counts = world.heal(handle)["network_fault_counts"]
        assert counts["tcp_reset"] > 0, counts
        assert phase("reconnect") == {"reconnected": True}
        evidence["reset"] = {"counts": counts, "socket_reset": True, "healed_reconnect": True}
        assert not world.inspect()["active_handles"]
        send(channel, {"phase": "done"})
        print(json.dumps(evidence, sort_keys=True))


if __name__ == "__main__":
    {"server": server, "client": client, "scenario": scenario}[sys.argv[1]]()
