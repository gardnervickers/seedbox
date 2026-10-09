# Automatic binary instrumentation experiment

Linux x86-64 feasibility experiment, first on the host and then inside a PV guest.
The initial binary-instrumentation experiment needed no runtime or kernel changes;
the later watchdog follow-up adds a kernel relaxation hook. A shared
world-code Clippy cleanup was required to pass the portable suite before guest
execution. Application executables contain no counter or
yield hooks; DynamoRIO inserts instrumentation at runtime. Building the small
fixture creates a controlled input, not a requirement to rebuild real software.
Existing container applications would need a launcher/runtime supplied inside
the guest, not application recompilation. The draft automatic integration now lives in `guest/progress/` and the
normal preparation/supervisor path; ARM correctness is still blocked. See
[the current migration findings](../../docs/automatic-progress.md). Existing etcd and Node binaries have
since passed the bounded tests linked below. The wrapper-image guest experiment and
its successful static/Node run-replay checks are described in
[guest/README.md](guest/README.md) and [findings](RESULTS.md).

`budget.c` injects a per-thread decrement and conditional callback at each dynamic
block entry. Every 10,000 entries it records a checkpoint and resets the budget.
By default the callback returns immediately. With the client option `--progress`,
it issues a raw Linux x86-64 `getpid` syscall. The existing PV syscall hook charges
time and exits when its published budget is reached. The budget
counts block entries, not retired instructions. Exceptions, translation changes
and callbacks need further semantics before this could be a Seedbox timebase.

The first four checkpoint offsets and a rolling hash of all checkpoint offsets
are reported. Module-relative offsets remove ASLR for the static fixture, but
the hash has no module identity and anonymous/JIT offsets are deliberately zero.
It is a diagnostic for this fixture, not a general replay witness. Per-thread
reports have no stable thread identity. Equal stdout does not prove equal state.

The experimental `--fast` client option uses a directly addressed raw TLS counter,
avoids reserving an extra application register for a state pointer, and omits
module lookup, checkpoint-offset hashing and point printing. It retains the
10,000-block budget, arithmetic-flag preservation and optional progress syscall.
Thread-exit reports include checkpoint counts, not partial-block counts or hashes.
The original diagnostic path remains available for comparison.

## Reproduce

Use the Linux project `nix develop` shell. Obtain the official SDK outside the
checkout:

```sh
mkdir -p /tmp/seedbox-dbi-spike
curl -fL -o /tmp/seedbox-dbi-spike/dynamorio.tar.gz \
  https://github.com/DynamoRIO/dynamorio/releases/download/release_11.3.0-1/DynamoRIO-Linux-11.3.0.tar.gz
echo 'ae02049df5c4daeb82d4087aaa0d38bf58cdd26b38fb6042dccf27cbc32ceed9  /tmp/seedbox-dbi-spike/dynamorio.tar.gz' | sha256sum -c -
tar -xzf /tmp/seedbox-dbi-spike/dynamorio.tar.gz -C /tmp/seedbox-dbi-spike
export SEEDBOX_DBI_SDK=/tmp/seedbox-dbi-spike/DynamoRIO-Linux-11.3.0-1
bash tools/dbi-spike/build.sh
python3 tools/dbi-spike/measure.py \
  --drrun "$SEEDBOX_DBI_SDK/bin64/drrun" \
  --client /tmp/seedbox-dbi-spike/build-native/libbudget.so \
  --output /tmp/seedbox-dbi-spike/static-results.json \
  -- /tmp/seedbox-dbi-spike/build-native/workload 200000000
```

The build script resolves extra tools from the project's existing locked nixpkgs.
It prints the Node executable for the subsequent JIT test. Substitute that path:

```sh
python3 tools/dbi-spike/measure.py \
  --drrun "$SEEDBOX_DBI_SDK/bin64/drrun" \
  --client /tmp/seedbox-dbi-spike/build-native/libbudget.so \
  --output /tmp/seedbox-dbi-spike/jit-results.json \
  -- /path/printed/by/build/node --single-threaded --predictable --random-seed=1 \
  tools/dbi-spike/workload.js 100000000
```

Use `--trace-opt` in a separate diagnostic run to confirm V8 optimizes `work`.
It changes stdout and should not be used in timing runs. The JIT experiment uses
explicit predictability flags to reduce concurrency; later results below also
cover ordinary Node defaults. Arbitrary application runtimes remain untested.
Node still creates helper threads.

The runner interleaves native, empty DynamoRIO and budget-instrumented executions,
reversing order every other trial. Each subprocess has a 60-second timeout. It
saves raw output, exit status and whole-process timings, and checks output
agreement and instrumentation presence. `budget_stderr_identical` reports
repeatability separately; a false value must not be interpreted as replay success.

## References

- [DynamoRIO execution model](https://dynamorio.org/overview.html)
- [Instrumentation API](https://dynamorio.org/API_BT.html)
- [Client transparency requirements](https://dynamorio.org/transparency.html)

See [findings](RESULTS.md) for performance, replay and progress-control results.
For etcd, `etcd/prepare.py` emits native, empty-translator, original-client and
optimized controls; `--writes 2000` requests 16,000 total writes. Build its
checker in the project Nix shell with `CGO_ENABLED=0 go build -buildvcs=false
-trimpath -o client .` from the generated client directory. The optimized entrypoint
uses `-opt_speed -disable_traces -c /experiment/libbudget.so --progress --fast`.
