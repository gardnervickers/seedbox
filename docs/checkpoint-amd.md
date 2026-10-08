# Runtime checkpoint AMD host checks

Run from this checkout in the project's development shell on an AMD x86-64
Linux KVM host. Choose an idle allowed CPU whose SMT sibling is also idle;
set `CPU` below. CLI execution and `harness/batch.sh` acquire the existing
core/sibling leases and restore the Zen MSR workaround. Do not run a naked VMM
or test binary alongside another user of that core. These commands add no
engine/kernel pin or host-policy proof. Keep every output, including failures.

## Build and inputs

```sh
set -euo pipefail
export CPU=2                       # replace with the available host lane
export OUT="$PWD/artifacts/checkpoint-host-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
export SEEDBOX_ARTIFACTS="$PWD/artifacts"
bash kernel/build.sh
cargo build --release --locked
cargo build-vmm
cargo test-vmm
cargo test-vmm --no-run --message-format=json > "$OUT/tests.jsonl"
export TEST_BINARY=$(python3 -c 'import json,sys; print(next(x["executable"] for x in map(json.loads,open(sys.argv[1])) if x.get("reason")=="compiler-artifact" and x.get("profile",{}).get("test") and x.get("executable")))' "$OUT/tests.jsonl")
export VMM="$PWD/crates/seedbox-vmm/target/release/seedbox-vmm"
export SEEDBOX_ROOT="$OUT"
export CPU0="$CPU"
export SEEDBOX_TIMEOUT=1800
export SEEDBOX_UNDO=256
export KERNEL="$PWD/artifacts/bzImage"

# Both VMMs must see these exact same K2/ABI-3 kernel and initramfs bytes.
guest/build-initramfs.sh artifacts/smoke.cpio guest/smoke.init
guest/build-initramfs.sh artifacts/net.cpio guest/net.init
"$SEEDBOX_STATIC_CC" -O2 -static -o artifacts/rdrandprobe guest/rdrandprobe.c
guest/build-initramfs.sh artifacts/rdrand.cpio guest/rdrand.init artifacts/rdrandprobe:bin/rdrandprobe
"$SEEDBOX_STATIC_CC" -O2 -static -o artifacts/rngprobe guest/rngprobe.c
guest/build-initramfs.sh artifacts/entropy-fork.cpio guest/entropy-fork.init artifacts/rngprobe:bin/rngprobe
guest/build-disk.sh artifacts/disk.cpio artifacts/ext4.img
# Set SEEDBOX_ETCD to the host's static etcd fixture binary.
: "${SEEDBOX_ETCD:?set the static etcd fixture path}"
guest/build-client.sh artifacts/client
guest/build-initramfs.sh artifacts/multi.cpio guest/cluster.init "$SEEDBOX_ETCD:bin/etcd" artifacts/client:bin/client
"$SEEDBOX_STATIC_CC" -O2 -static -o artifacts/link-probe harness/fixtures/links/link-probe.c
guest/build-initramfs.sh artifacts/link-short.cpio harness/fixtures/links/link-short.init artifacts/link-probe:bin/link-probe

# Baseline is the main merged into this PR, not a later moving main.
git worktree add --detach "$OUT/main" 7a42b56c35bf3d263ae02aeba7b0f86f866d923b
(cd "$OUT/main" && cargo build-vmm && cargo build --release --locked)
export MAIN_VMM="$OUT/main/crates/seedbox-vmm/target/release/seedbox-vmm"
```

## Ignored KVM tests

Run every ignored VMM test in its own process, serially under one lease. The
counter currently has process-lifetime ownership; separate test processes avoid
accumulating perf descriptors across unrelated tests. This includes the existing
spin, trigger, page-capture and timed-log controls, plus checkpoint tests. The
wrapper below uses the existing batch ownership and MSR helpers; it is a test
invocation, not an application acceptance runner.

```sh
sudo -n env PATH="$PATH" CPU="$CPU" TEST_BINARY="$TEST_BINARY" python3 - <<'PY'
import os, signal, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, 'harness')
import batch, msr54
cpu=int(os.environ['CPU']); binary=os.environ['TEST_BINARY']
names=[line.removesuffix(': test') for line in subprocess.check_output(
    [binary,'--ignored','--list'],text=True).splitlines() if line.endswith(': test')]
assert names
batch.enable_subreaper()
def interrupted(sig, frame): raise batch.Interrupted(sig)
for sig in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP): signal.signal(sig,interrupted)
with batch.cpu_locks(sorted(batch.siblings(cpu))):
    state=Path(tempfile.mkdtemp(prefix='checkpoint-kvm-',dir='/tmp')); saved=state/'saved.json'
    try:
        msr54.set_workaround(sorted(batch.siblings(cpu)),saved)
        for name in names:
            child=subprocess.Popen(['taskset','-c',str(cpu),binary,name,'--exact',
                '--ignored','--nocapture','--test-threads=1'],start_new_session=True)
            try:
                deadline=time.monotonic()+600
                while (status:=batch.exit_status(child)) is None:
                    if time.monotonic()>deadline: raise TimeoutError(name)
                    time.sleep(.05)
                if status: raise RuntimeError((name,status))
            finally: batch.stop_owned_group(child)
    finally:
        for sig in (signal.SIGINT,signal.SIGTERM,signal.SIGHUP): signal.signal(sig,signal.SIG_IGN)
        if saved.exists(): msr54.restore_snapshot(sorted(batch.siblings(cpu)),saved)
        if not saved.exists(): state.rmdir()
PY
```

Expected: every ignored test passes, including runtime restore, B=P, partial
matchers/selectors and pending trigger effects, PV page/syscall speed, timed-log
cursor/order, torn disk survivors and per-machine getrandom/urandom repeat and
divergence. The latter needs the entropy fixture built above.

## Feature-off equality with main

Compare ordinary executions, including boot forks, with no runtime checkpoint
flag. Build absolute world paths once so both binaries get identical inputs.

```sh
python3 - <<'PY'
import json,os
from pathlib import Path
root=Path.cwd(); out=Path(os.environ['OUT']); (out/'worlds').mkdir()
fixtures=['smoke','rdrand','net','net-pause','net-crash','disk-nocrash',
          'disk-crash','disk-ext4-torn','cluster-fixed','cluster-fork',
          'cluster-fork-skew','cluster-fork-late','shaped-tol-short']
for name in fixtures:
    source=root/'harness/worlds'/f'{name}.json'; world=json.loads(source.read_text())
    world['kernel']=os.environ['KERNEL']
    for m in world['machines']:
        m['initrd']=str(root/'artifacts/link-short.cpio') if name=='shaped-tol-short' else str((source.parent/m['initrd']).resolve())
        if m.get('disk',{}).get('image'): m['disk']['image']=str((source.parent/m['disk']['image']).resolve())
    (out/'worlds'/f'{name}.json').write_text(json.dumps(world))
PY
for world in smoke rdrand net net-pause net-crash disk-nocrash disk-crash disk-ext4-torn cluster-fixed cluster-fork cluster-fork-skew cluster-fork-late shaped-tol-short; do
  extra=()
  case "$world" in cluster-fork*) extra=(--fork-seeds 3,4);; esac
  for seed in 1 2; do
    for lane in main candidate; do
      engine="$VMM"; test "$lane" != main || engine="$MAIN_VMM"
      VMM="$engine" bash harness/batch.sh "$OUT/worlds/$world.json" "off-$lane-$world" "$seed" --digest "${extra[@]}"
    done
  done
done
python3 - <<'PY'
import json,os
from pathlib import Path
runs=Path(os.environ['OUT'])/'runs'
# batch.sh retains runs under OUT/runs, named prefix-sSEED-r0.
def semantic(r):
    if 'seeds' in r: return [semantic(x) for x in r['seeds']]
    assert r.get('fault_error') is None
    assert all(not m.get('infrastructure_error') and m.get('guest_init_error') is None for m in r['machines'])
    return {k:r.get(k) for k in ('outcome','world_witness','network_witness','network_counters','faults','control_history','triggers')} | {
        'machines':[{k:m.get(k) for k in ('name','state','guest_exit_code','time_ns','branches','witness','state_digest','pending_io','disk','crashed_incarnations')} for m in r['machines']]}
for candidate in sorted(runs.glob('off-candidate-*-s*-r0.json')):
    main=candidate.with_name(candidate.name.replace('off-candidate-','off-main-'))
    assert candidate.with_suffix('.status').read_text().strip()=='0'
    assert main.with_suffix('.status').read_text().strip()=='0'
    assert semantic(json.loads(candidate.read_text()))==semantic(json.loads(main.read_text())),candidate
    print('MATCH',candidate.name)
PY
```

Use fresh batch prefixes on a repeated invocation; batch refuses overwrites.
Expected: identical outcome, world/network/machine witnesses, full-state digests,
network/trigger/control/fault evidence and disk results for every seed/boot branch.
Host time, PMU diagnostics and engine identities are intentionally not compared.

## Checkpoint at T with N forks, repetitions and cold reconstruction

N=3 here: parent seed 7 (continuation), plus 11 and 12. A comma-separated time
sweep covers early boot and either side of net-crash's 2 s crash/2.5 s reboot.
Each prefix is independently reconstructed; holding earlier checkpoints cannot
change later branches. Run both timebases, repeat, and disable undo.

```sh
TIMES=0,250000000,1999999999,2000000000,2000000001,2250000000,2500000000,2500000001
for timebase in counter pv; do
  for repetition in first repeat undo0; do
    undo=256; test "$repetition" != undo0 || undo=0
    SEEDBOX_UNDO="$undo" bash harness/batch.sh "$OUT/worlds/net-crash.json" "fork-$timebase-$repetition" 7 --digest --timebase "$timebase" --checkpoint-at-ns "$TIMES" --fork-seeds 11,12
  done
  for T in ${TIMES//,/ }; do
    for B in 11 12; do
      bash harness/batch.sh "$OUT/worlds/net-crash.json" "cold-$timebase-$T-$B" 7 --digest --timebase "$timebase" --checkpoint-at-ns "$T" --fork-seeds "$B"
    done
  done
done
python3 - <<'PY'
import json
from pathlib import Path
import os
runs=Path(os.environ['OUT'])/'runs'
def load(prefix):
    path=runs/f'{prefix}-s7-r0.json'
    assert path.with_suffix('.status').read_text().strip()=='0',path
    r=json.loads(path.read_text())
    entries=r.get('checkpoints',[r])
    for x in entries:
        assert x.get('fault_error') is None
        assert all(not m['infrastructure_error'] and m['guest_init_error'] is None for m in x['machines'])
    return {(x['checkpoint']['at_ns'],x['checkpoint']['branch_seed']):(x['world_witness'],[m['state_digest'] for m in x['machines']]) for x in entries}
for tb in ('counter','pv'):
    first=load(f'fork-{tb}-first')
    assert first==load(f'fork-{tb}-repeat')==load(f'fork-{tb}-undo0')
    for T,B in first:
        if B!=7: assert load(f'cold-{tb}-{T}-{B}')[T,B]==first[T,B]
    for T in {t for t,b in first}: assert first[T,11]!=first[T,12]
    print('MATCH',tb,len(first),'branches')
PY
```

For a disk/torn-write sweep, rerun the preceding command/comparison block with
`net-crash.json` replaced by `disk-ext4-torn.json`, `fork-`/`cold-` prefixes
replaced by `disk-fork-`/`disk-cold-`, and
`TIMES=0,4999999999,5000000000,5000000001,5250000000,5500000000,5500000001`.
The ignored charged-I/O and lifecycle tests cover exact internal boundaries that
cannot be inferred from a host wall-time checkpoint. B=P must preserve all streams;
its equality is against uninterrupted continuation from that captured prefix,
not a claim that adding a hard stop at T never changes an ordinary cold execution.

## Public retained branches and replay MATCH

The public CLI exposes one branch per run; the engine-only batch above shares
checkpoint RAM across N branches. Run three public branches and replay each,
including named-volume/torn-write and trigger/connection-selector workloads.
The CLI owns its CPU/MSR lease, so run these outside the unit-test wrapper.

```sh
for timebase in counter pv; do
  for project in examples/redis harness/acceptance/workloads/volume-crash.yaml harness/acceptance/workloads/triggers.yaml harness/acceptance/workloads/connections.yaml; do
    name=$(basename "$project" .yaml)
    extra=(); case "$project" in *volume-crash*) extra=(--torn-writes);; esac
    for B in 7 11 12; do
      log="$OUT/public-$name-$timebase-$B.log"
      sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" SEEDBOX_MKE2FS="$SEEDBOX_MKE2FS" target/release/seedbox run "$project" --kernel "$KERNEL" --vmm "$VMM" --cpu "$CPU" --state "$OUT/state" --seed 7 --timebase "$timebase" --full-state --checkpoint-at-ns 1000000000 --branch-seed "$B" "${extra[@]}" > "$log"
      RUN=$(sed -n 's/^[A-Z]*: //p' "$log")
      test -n "$RUN"
      sudo -n env PATH="$PATH" target/release/seedbox replay "$RUN" --vmm "$VMM" --cpu "$CPU" --state "$OUT/state" > "$log.replay"
      grep '^replay: MATCH$' "$log.replay"
    done
  done
done
```

Expected: workload PASS and replay MATCH for every retained branch. On a workload
that emits console/management bytes after T, verify `seedbox logs RUN --since-ns
1000000000` includes those bytes from the branch. Compare two RAM-divergent
`--full-state` branch attempts with `seedbox diff RUN_A RUN_B --json`: the affected
machine's `ram_pages` must contain the differing PFNs without reporting missing
page hashes. Evidence should be directly under `RUN/output/machines/`,
including `.chunks` indexes and `.pages` files, for both run and replay.

For a public feature-off comparison on Redis, run:

```sh
for lane in main candidate; do
  cli=target/release/seedbox; engine="$VMM"
  if test "$lane" = main; then cli="$OUT/main/target/release/seedbox"; engine="$MAIN_VMM"; fi
  sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" "$cli" run examples/redis --kernel "$KERNEL" --vmm "$engine" --cpu "$CPU" --state "$OUT/plain-state" --seed 7 --full-state > "$OUT/plain-$lane.log"
  RUN=$(sed -n 's/^[A-Z]*: //p' "$OUT/plain-$lane.log")
  sudo -n "$cli" inspect "$RUN" --json > "$OUT/plain-$lane.json"
done
python3 - <<'PY'
import json,os
from pathlib import Path
out=Path(os.environ['OUT'])
a=json.loads((out/'plain-main.json').read_text()); b=json.loads((out/'plain-candidate.json').read_text())
ai=a['identity']['inputs']; bi=b['identity']['inputs']
# The actual engine hashes necessarily differ. All other prepared inputs must agree.
ai['profile'].pop('engine'); bi['profile'].pop('engine')
assert ai==bi, 'prepare used different input bytes'
assert a['outcome']==b['outcome']=='PASS'
assert all(a['report'][k]==b['report'][k] for k in ('reason','workload_exit','witness','full_state','faults'))
print('MATCH public feature off')
PY
```

The low-level equality above always uses one exact input closure. No KVM/guest result in this procedure has been claimed
from the Mac checks.
