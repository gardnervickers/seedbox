import os, struct, sys, json
MSR = 0xc0011020; BIT = 1 << 54
cpus = [int(c) for c in sys.argv[2].split(",")]
# One save file per core set, so concurrent batches on other cores do not clash.
SAVED = f"/home/gvickers/seedbox-sim/msr54-saved-{sys.argv[2]}.json"
def rd(c):
    fd = os.open(f"/dev/cpu/{c}/msr", os.O_RDONLY); v = struct.unpack("<Q", os.pread(fd, 8, MSR))[0]; os.close(fd); return v
def wr(c, v):
    fd = os.open(f"/dev/cpu/{c}/msr", os.O_WRONLY); os.pwrite(fd, struct.pack("<Q", v), MSR); os.close(fd)
cmd = sys.argv[1]
if cmd == "show":
    print({c: hex(rd(c)) for c in cpus})
elif cmd == "set":
    # All or nothing: on any failure, put back what was changed and exit 1.
    saved = {c: rd(c) for c in cpus}
    json.dump(saved, open(SAVED, "w"))
    done = []
    try:
        for c in cpus:
            wr(c, saved[c] | BIT)
            done.append(c)
            if not rd(c) & BIT:
                raise OSError(f"cpu {c}: bit 54 did not stick")
    except OSError as e:
        for c in done:
            wr(c, saved[c])
        sys.exit(f"msr54 set failed: {e}")
    print("set", {c: hex(rd(c)) for c in cpus})
elif cmd == "clear":
    for c in cpus: wr(c, rd(c) & ~BIT)
    print("cleared", {c: hex(rd(c)) for c in cpus})
elif cmd == "restore":
    saved = {int(k): v for k, v in json.load(open(SAVED)).items()}
    for c in cpus: wr(c, saved[c])
    print("restored", {c: hex(rd(c)) for c in cpus})
