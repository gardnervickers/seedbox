# PV runs on a throwaway DigitalOcean droplet

`pv-run.sh` tests a commit's PV behaviour on a fresh DigitalOcean droplet, then
deletes the droplet. Use it for host runs that only need `--timebase pv`, so they
don't queue behind the AMD workstation. Droplets have nested KVM but no guest
PMU, so counter-timebase runs still need the workstation.

```sh
tools/droplet/pv-run.sh --ref my-branch --world examples/redis --seeds "1 2"
```

For each seed it runs the world twice, compares the pair with `seedbox diff`,
and replays the first run. It prints a summary and exits 0 only when every run
passed, every pair matched and every replay matched. Raw output, the summary and
each run's `vmm.json` are kept under `target/droplet-pv/<sha>-<time>/`.

The commit is shipped from the local checkout with `git archive`, so it doesn't
need to be pushed. The droplet installs Nix and podman and builds the CLI and
engine inside `nix develop`, which takes about 7 minutes. The guest kernel is
copied from `$SEEDBOX_KERNEL` or `~/seedbox-sim/artifacts/bzImage`. Pass
`--build-kernel` when the commit changes `kernel/`.

## Requirements

- `doctl`, authenticated (`--context NAME` or `SEEDBOX_DO_CONTEXT` picks a context).
- An SSH key on the DigitalOcean account that this machine can use. By default
  every account key is added to the droplet; `--ssh-keys` or
  `SEEDBOX_DO_SSH_KEYS` narrows that, and `--identity` picks the local key.

The default size is `s-4vcpu-8gb-240gb-intel` (shared CPU, about $0.10/hour);
a run usually costs a few cents. The droplet is deleted on success, failure or
Ctrl-C, and its deletion is confirmed. Droplets are tagged `seedbox-pv`, so
`doctl compute droplet list --tag-name seedbox-pv` shows any left behind by
`--keep` or a killed script.

Results on a droplet are evidence for that droplet's CPU only; same-seed
equality across hosts is not established.
