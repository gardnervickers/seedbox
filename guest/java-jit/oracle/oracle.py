"""Independent integer oracle for normal Java/JFR work over real guest TCP."""
import socket
import time

MASK = (1 << 64) - 1


def expected(seed):
    total = 0
    for i in range(20_000):
        x = ((i + seed) * 0x9E3779B97F4A7C15) & MASK
        rotated = ((x << 17) | (x >> 47)) & MASK
        total = (total + (rotated ^ (x >> 13))) & MASK
    return (total * 100) & MASK


def request(command):
    with socket.create_connection(("java", 12345), timeout=1) as connection:
        connection.settimeout(30)
        connection.sendall(command.encode() + b"\n")
        with connection.makefile("rb") as reply:
            line = reply.readline(4096)
            if not line.endswith(b"\n"):
                raise RuntimeError("incomplete Java reply")
            if reply.read(1):
                raise RuntimeError("unexpected extra Java output")
            return line.decode().strip()


print("java oracle: waiting for service", flush=True)
for attempt in range(1200):
    try:
        first = request("1")
        break
    except (ConnectionError, OSError):
        time.sleep(0.05)
else:
    raise RuntimeError("Java never became ready")
assert int(first) == expected(1), first
for seed in (7, 123):
    actual = int(request(str(seed)))
    assert actual == expected(seed), (seed, actual, expected(seed))
    print(f"java oracle: seed={seed} checksum={actual}", flush=True)
result = request("stop")
assert result.startswith("JFR STOPPED bytes="), result
assert int(result.split("=")[1]) > 0, result
# Let the ordinary process-exit observation settle before ending the oracle.
# Host acceptance also checks retained exit/console/worker evidence for failures.
time.sleep(0.1)
print("java oracle: three checksums and real JFR dump passed", flush=True)
