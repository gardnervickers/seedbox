"""Bounded public child output; native supervisor owns each complete phase tree."""
import signal
import subprocess
import threading

LIMIT = 8 * 1024 * 1024
TAIL = 64 * 1024


def execute(root, command, directory, env, timeout=600):
    process = subprocess.Popen([str(root / "bin/unix-supervisor"), *command],
                               cwd=directory, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE)
    buffers = [bytearray(), bytearray()]
    tails = [bytearray(), bytearray()]
    sizes = [0, 0]

    def drain(pipe, index):
        with pipe:
            while chunk := pipe.read(8192):
                sizes[index] += len(chunk)
                buffers[index].extend(chunk[:max(0, LIMIT - len(buffers[index]))])
                tails[index].extend(chunk)
                del tails[index][:-TAIL]

    readers = [threading.Thread(target=drain, args=(pipe, i), daemon=True)
               for i, pipe in enumerate((process.stdout, process.stderr))]
    for reader in readers:
        reader.start()
    failure = None
    def interrupted(signum, _frame):
        raise InterruptedError(f"phase interrupted by signal {signum}")
    previous = signal.signal(signal.SIGTERM, interrupted)
    try:
        status = process.wait(timeout=timeout)
    except BaseException as error:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        failure = error
        # Never kill this Python group before it can publish the phase evidence.
        # The native owner kills/reaps even descendants in other groups.
        process.send_signal(signal.SIGTERM)
        try:
            status = process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            status = None
            failure = RuntimeError("phase supervisor failed to reap its owned tree")
    finally:
        signal.signal(signal.SIGTERM, previous)
        for reader in readers:
            reader.join(timeout=10)
        for index, name in enumerate(("stdout", "stderr")):
            print(f"phase {directory.name} {name}: {sizes[index]} bytes; "
                  f"last {len(tails[index])} bytes", flush=True)
            print(tails[index].decode("utf-8", errors="replace"), flush=True)
    if any(reader.is_alive() for reader in readers):
        raise RuntimeError("phase output pipes still owned after cleanup")
    if failure is not None:
        raise failure
    if any(size > LIMIT for size in sizes):
        raise RuntimeError(f"phase output exceeded {LIMIT} bytes: {sizes}")
    return status, *(bytes(buffer).decode("utf-8") for buffer in buffers)
