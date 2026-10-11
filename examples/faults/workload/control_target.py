"""Small lifecycle target used by the API acceptance cases."""
import os
from pathlib import Path
import socket
import sys
import threading

MARK = Path("/tmp/seedbox-lifecycle-mark")

assert not Path("/run/seedbox/control.sock").exists(), "fault socket exposed on target"
print("TARGET_SOCKET absent", flush=True)


def answer(command):
    if command == b"EXIT":
        os._exit(17)
    if command == b"PING":
        return "READY socket_absent"
    if command == b"ROLE":
        # A spawned copy may override its environment and command.
        return f"ROLE {os.environ.get('ROLE', 'none')} {' '.join(sys.argv[1:2]) or '-'}"
    if command == b"MARK":
        MARK.write_text("marked\n")
        return "MARKED"
    assert command == b"BOOT", command
    # The marker lives in the image root in RAM, so only a fresh boot clears it.
    boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    return f"BOOT {boot} marked={str(MARK.exists()).lower()}"


def serve(channel):
    # One connection may carry several commands; the lifecycle case keeps it open.
    with channel, channel.makefile("rb") as reader:
        for line in reader:
            channel.sendall(answer(line.strip()).encode() + b"\n")


with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", 7000))
    listener.listen()
    while True:
        channel, _ = listener.accept()
        threading.Thread(target=serve, args=(channel,), daemon=True).start()
