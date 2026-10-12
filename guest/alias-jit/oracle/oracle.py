"""Guest oracle: both publication shapes execute useful code over TCP."""
import socket
import time


def request(command):
    with socket.create_connection(("publisher", 12345), timeout=1) as connection:
        connection.settimeout(30)
        connection.sendall(command.encode() + b"\n")
        with connection.makefile("rb") as reply:
            line = reply.readline(4096)
            assert line.endswith(b"\n"), line
            assert not reply.read(1), "unexpected extra reply"
            return line.decode().strip()


for attempt in range(120):
    try:
        result = request("append 2000")
        break
    except (ConnectionError, OSError):
        time.sleep(0.05)
else:
    raise RuntimeError("alias publisher never became ready")
count = 2000
expected = 128 * (count * (count + 1) // 2 + 77 * count)
assert int(result) == expected, result
assert int(request("patch 2000")) == expected
assert request("stop") == "STOPPED"
time.sleep(0.1)
print(f"alias oracle: two checksums={expected}, publications=4000 calls=1024000 passed", flush=True)
