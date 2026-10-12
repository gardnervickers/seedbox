"""Guest-owned resolver, DNS wire, partition and reboot acceptance checks."""
import json
from pathlib import Path
import socket
import struct
import sys
import time

DNS = ("10.0.1.1", 53)
ADDRESSES = {"node": "10.0.0.1", "oracle": "10.0.0.2", "peer": "10.0.0.3"}


def bootstrap():
    own = socket.gethostname()
    assert Path("/etc/resolv.conf").read_text() == f"nameserver {DNS[0]}\n"
    lines = [line.split() for line in Path("/etc/hosts").read_text().splitlines()]
    assert lines == [["127.0.0.1", "localhost"], ["::1", "localhost"],
                     [ADDRESSES[own], own]], lines
    peer = "peer" if own == "node" else "node"
    # AF_UNSPEC exercises the libc resolver's A and AAAA query pair. The
    # absolute, mixed-case peer name cannot be satisfied by our hosts file.
    results = socket.getaddrinfo(peer.upper() + ".", 7000, type=socket.SOCK_STREAM)
    assert {result[4][0] for result in results} == {ADDRESSES[peer]}, results
    try:
        socket.getaddrinfo("missing.invalid", 7000, type=socket.SOCK_STREAM)
    except socket.gaierror as error:
        assert error.errno == socket.EAI_NONAME, error
    else:
        raise AssertionError("unknown machine resolved")


def query(name, kind=1, edns=False):
    question = b"".join(bytes([len(label)]) + label.encode() for label in name.split("."))
    question += b"\0" + struct.pack("!HH", kind, 1)
    packet = struct.pack("!6H", 0x1234, 0x0100, 1, 0, 0, int(edns)) + question
    if edns:
        packet += struct.pack("!BHHIH", 0, 41, 1232, 0, 0)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as channel:
        channel.settimeout(2)
        channel.sendto(packet, DNS)
        response, source = channel.recvfrom(1232)
    assert source == DNS, source
    ident, flags, questions, answers, authority, additional = struct.unpack("!6H", response[:12])
    assert ident == 0x1234 and flags & 0x8500 == 0x8500, response.hex()
    assert flags & 0x80 == 0, "virtual endpoint must not advertise recursion"
    assert (questions, authority, additional) == (1, 0, int(edns))
    assert response[12:12 + len(question)] == question
    known = name.lower() in ADDRESSES
    assert flags & 15 == (0 if known else 3), response.hex()
    assert answers == int(known and kind == 1), response.hex()
    if answers:
        record = response[12 + len(question):]
        assert record[:2] == b"\xc0\x0c", record.hex()
        assert struct.unpack("!HHIH", record[2:12]) == (1, 1, 5, 4), record.hex()
        assert socket.inet_ntoa(record[12:16]) == ADDRESSES[name.lower()]
    if edns:
        assert struct.unpack("!BHHIH", response[-11:]) == (0, 41, 1232, 0, 0)


def ready(target="node"):
    deadline = time.monotonic() + 5
    while True:
        try:
            with socket.create_connection((target, 7000), timeout=1) as channel:
                assert channel.recv(32) == b"dns-ready\n"
            return
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def node():
    bootstrap()
    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 7000))
        listener.listen()
        while True:
            with listener.accept()[0] as channel:
                channel.sendall(b"dns-ready\n")
                if channel.recv(32) == b"partition\n":
                    peer = "peer" if socket.gethostname() == "node" else "node"
                    query(peer, edns=True)
                    assert socket.gethostbyname(peer.upper() + ".") == ADDRESSES[peer]
                    try:
                        with socket.create_connection((ADDRESSES[peer], 7000), timeout=0.05):
                            raise AssertionError("partition allowed peer traffic")
                    except TimeoutError:
                        pass
                    channel.sendall(b"dns-partition\n")


def partition_probe(target):
    with socket.create_connection((target, 7000), timeout=2) as channel:
        assert channel.recv(32) == b"dns-ready\n"
        channel.sendall(b"partition\n")
        assert channel.recv(32) == b"dns-partition\n"


def oracle():
    from seedbox.control import World

    bootstrap()
    ready()  # Also proves the peer's libc lookup and bootstrap checks passed.
    ready("peer")
    for name in ["node", "NODE", "oracle", "peer", "missing.invalid"]:
        for kind in [1, 28, 16]:
            for edns in [False, True]:
                query(name, kind, edns)
    with World.connect() as world:
        partition = world.partition(["node"], ["peer"])
        partition_probe("node")
        partition_probe("peer")
        world.heal(partition)
        world.crash("node")
        world.reboot("node")
        query("node")  # This PR deliberately preserves addresses across reboot.
        ready()
    print(json.dumps({"libc": True, "edns": True, "ttl": 5,
                      "nxdomain": True, "partition": True, "reboot": True}, sort_keys=True))


if __name__ == "__main__":
    {"node": node, "peer": node, "oracle": oracle}[sys.argv[1]]()
