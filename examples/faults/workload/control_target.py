"""Small process-lifecycle target used by the API acceptance case."""
import os
from pathlib import Path
import socket
import subprocess

assert not Path("/run/seedbox/control.sock").exists(), "fault socket exposed on target"
print("TARGET_SOCKET absent", flush=True)
marker = Path("/data/io-incarnation")
marker.parent.mkdir(parents=True, exist_ok=True)
generation = int(marker.read_text()) + 1 if marker.exists() else 1
marker.write_text(str(generation))
with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", 7000))
    listener.listen()
    while True:
        channel, _ = listener.accept()
        with channel, channel.makefile("rb") as reader:
            command = reader.readline().strip()
            if command == b"EXIT":
                os._exit(17)
            assert command == b"PING", command
            # PING follows the acknowledged restart, so the negative probe
            # cannot race target completion against the restart response.
            subprocess.run(["/workload/io-check", str(generation)], check=True)
            channel.sendall(b"READY socket_absent\n")
