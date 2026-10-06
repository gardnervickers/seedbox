import os, struct, sys, json
MSR = 0xc0011020; BIT = 1 << 54
cpus = [int(c) for c in sys.argv[2].split(",")]
def rd(c):
    fd = os.open(f"/dev/cpu/{c}/msr", os.O_RDONLY); v = struct.unpack("<Q", os.pread(fd, 8, MSR))[0]; os.close(fd); return v
def wr(c, v):
    fd = os.open(f"/dev/cpu/{c}/msr", os.O_WRONLY); os.pwrite(fd, struct.pack("<Q", v), MSR); os.close(fd)
cmd = sys.argv[1]
if cmd == "show":
    print({c: hex(rd(c)) for c in cpus})
elif cmd == "set":
    saved = {c: rd(c) for c in cpus}
    json.dump(saved, open("/home/gvickers/seedbox-sim/msr54-saved.json", "w"))
    for c in cpus: wr(c, saved[c] | BIT)
    print("set", {c: hex(rd(c)) for c in cpus})
elif cmd == "clear":
    for c in cpus: wr(c, rd(c) & ~BIT)
    print("cleared", {c: hex(rd(c)) for c in cpus})
elif cmd == "restore":
    saved = {int(k): v for k, v in json.load(open("/home/gvickers/seedbox-sim/msr54-saved.json")).items()}
    for c in cpus: wr(c, saved[c])
    print("restored", {c: hex(rd(c)) for c in cpus})
