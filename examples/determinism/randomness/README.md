# Randomness over a long run

Mechanism probed: the guest kernel CRNG and its reseed schedule. Linux reseeds
the base CRNG roughly 5, 10, 20, 40 and 60 s after boot and then every 60 s,
from an input pool that mixes interrupt entropy. Each interrupt sample includes
the interrupted instruction pointer and cycle counter, so if an interrupt lands
one instruction later on one run, every later `getrandom` output differs. The
app also observes consumers of that randomness: ASLR (mmap and libc addresses),
TCP ephemeral port choice, Python's hash seed, and `AT_RANDOM`/hash seed of a
freshly executed child process.

The single service runs `randomness.py` (a config file) on the stock
`python:3.12-alpine` image. For 70 s of virtual time it does, every 5 s:

- a burst of work: 32 small write+fsync+read cycles in `/tmp` (disk
  interrupts) and 200 connect/accept/close round trips to a loopback listener;
- `time.sleep` until the next 5 s boundary, so the kernel idles and ticks;
- one sample (14 in total, so the 5/10/20/40/60 s reseeds all fall inside).

Output, one block per sample prefixed `sNN`:

```
pythonhashseed: unset
samples: 14
interval_s: 5
s01 getrandom: <os.getrandom(16) hex>
s01 uuid: </proc/sys/kernel/random/uuid, dashes removed>
s01 hash_str: <hash("seedbox")>
s01 hash_tuple: <hash(("a", 1))>
s01 set_order: <iteration order of a fresh 20-string set>
s01 mmap: <addr of 2 MiB mmap>,<addr of second 2 MiB mmap>
s01 ephemeral_port: <local port of a new loopback TCP connection>
s01 child: <child os.urandom(8) hex>,<child hash("x")>
s01 dlsym_getpid: <address of getpid via ctypes.CDLL(None)>
s01 entropy_avail: <entropy_avail>
...
digest: <sha256 of all sNN lines>
```

143 lines plus the digest; the app exits 0 after sample 14.

Compare two fresh runs with the same seed (1104): their console bytes must
be identical, or use `seedbox diff` on the two run directories. The first
differing `sNN` line shows when divergence started; a `getrandom` or `uuid`
difference that begins only after a reseed boundary points at interrupt
entropy (instruction pointer at interrupt delivery), while an `s01` difference
in `mmap`/`dlsym_getpid`/`hash_*` points at boot-time seeding.

Control knob: vary `--watchdog-ms` (host interruptions must not become guest
interrupts or entropy) and `--machine-workers` between the two runs; output
must still match. Expected to MATCH on x86; ARM, where interrupt delivery
relies on the idle rendezvous, is the real test.
