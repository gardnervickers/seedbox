# PV runs on a throwaway DigitalOcean droplet

`pv-run.sh` tests PV behaviour for one or more commits on a fresh DigitalOcean
droplet, then deletes the droplet. Droplets have nested KVM but no guest PMU, so
they only help with `--timebase pv` runs; counter-timebase runs still need the
AMD workstation.

## When to use it

A droplet spends about 7 minutes installing Nix and building before its first
run, and each further commit adds a build of a few minutes. Use a droplet only
when the workstation is busy and at least 10 minutes of PV runs are queued
(on the droplet a warm Redis run takes about 30 seconds, plus as long again
for its replay). Batch it: collect every branch, world and seed that is
waiting and pass them all to one invocation. Anything smaller waits for the
workstation.

```sh
tools/droplet/pv-run.sh --ref branch-a --ref branch-b \
  --world examples/redis --world examples/other --seeds "1 2 3"
```

For each commit, world and seed it runs the world twice, compares the pair with
`seedbox diff`, and replays the first run. It prints one summary and exits 0
only when every build succeeded, every run passed, every pair matched and every
replay matched. Raw output, build logs, the summary and each run's `vmm.json`
are kept under `target/droplet-pv/<time>/`, one directory per commit.

Commits are shipped from the local checkout with `git archive`, so they don't
need to be pushed. The guest kernel is copied from `$SEEDBOX_KERNEL` or
`~/seedbox-sim/artifacts/bzImage`. Pass `--build-kernel` when a commit changes
`kernel/`.

## Requirements

- `doctl`, authenticated (`--context NAME` or `SEEDBOX_DO_CONTEXT` picks a context).
- An SSH key on the DigitalOcean account that this machine can use. By default
  every account key is added to the droplet; `--ssh-keys` or
  `SEEDBOX_DO_SSH_KEYS` narrows that, and `--identity` picks the local key.

The default size is `s-4vcpu-8gb-240gb-intel` (shared CPU, about $0.10/hour);
a batch usually costs a few cents. The droplet is deleted on success, failure or
Ctrl-C, and its deletion is confirmed. Droplets are tagged `seedbox-pv`, so
`doctl compute droplet list --tag-name seedbox-pv` shows any left behind by
`--keep` or a killed script.

Results on a droplet are evidence for that droplet's CPU only; same-seed
equality across hosts is not established.
