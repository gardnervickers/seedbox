"""Guest lookups through the simulated nameserver, from glibc and musl.

Each peer answers a TCP connection with what its own C library resolved:
its own name (from /etc/hosts), the other peer and the oracle by name (A and
AAAA queries to the nameserver), the other peer's address back to its name
(PTR), and a name no machine has (NXDOMAIN). The oracle makes the same
lookups with musl and checks every answer agrees.
"""
import json
import socket
import sys
import time

PEERS = ("glibc", "musl")
PORT = 7000


def address(name):
    """Every address getaddrinfo returns for `name`, as A and AAAA ask."""
    infos = socket.getaddrinfo(name, PORT, type=socket.SOCK_STREAM)
    return sorted({info[4][0] for info in infos})


def lookups(me):
    other = next(peer for peer in PEERS if peer != me) if me in PEERS else None
    result = {"self": address(me), "oracle": address("oracle")}
    for peer in PEERS:
        result[peer] = address(peer)
    for peer in PEERS:
        if peer != me:
            result[f"reverse:{peer}"] = socket.gethostbyaddr(result[peer][0])[0]
    try:
        socket.getaddrinfo("no-such-machine", PORT)
        result["unknown"] = "resolved"
    except socket.gaierror as error:
        result["unknown"] = error.errno == socket.EAI_NONAME
    result["other"] = other
    return result


def serve(me):
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("0.0.0.0", PORT))
        listener.listen()
        while True:
            connection, _ = listener.accept()
            with connection:
                connection.sendall(json.dumps(lookups(me)).encode() + b"\n")


def ask(peer):
    deadline = time.monotonic() + 30
    while True:
        try:
            with socket.create_connection((peer, PORT), timeout=5) as connection:
                with connection.makefile("r") as reply:
                    return json.loads(reply.readline())
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def scenario():
    mine = lookups("oracle")
    print("DNS oracle " + json.dumps(mine, sort_keys=True), flush=True)
    addresses = {name: mine[name] for name in ("oracle",) + PEERS}
    for name, found in addresses.items():
        assert len(found) == 1 and found[0].startswith("10.0."), (name, found)
    assert len(set(map(tuple, addresses.values()))) == 3, addresses
    assert mine["self"] == addresses["oracle"], mine
    assert mine["unknown"] is True, mine
    for peer in PEERS:
        assert mine[f"reverse:{peer}"] == peer, mine
    for peer in PEERS:
        theirs = ask(peer)
        print(f"DNS {peer} " + json.dumps(theirs, sort_keys=True), flush=True)
        # Each peer finds itself in its hosts file and everyone else through
        # the nameserver, at the addresses the oracle sees.
        for name, found in addresses.items():
            key = "self" if name == peer else name
            assert theirs[key] == found, (peer, name, theirs)
        assert theirs[f"reverse:{theirs['other']}"] == theirs["other"], theirs
        assert theirs["unknown"] is True, theirs
    print("DNS_OK", flush=True)
    return 0


if __name__ == "__main__":
    if sys.argv[1] == "scenario":
        raise SystemExit(scenario())
    serve(sys.argv[1])
