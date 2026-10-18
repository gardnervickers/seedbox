# Seedbox

> **⚠️ Early, buggy software.** 
> I vibe-coded the vast majority of this so temper your expectations. Expect slop, bugs, and dead code. Nothing api-wise is stable yet. 

Seedbox runs Linux applications in virtual machines with controlled time,
networking, and disk faults. A seed and recorded inputs let you repeat a
simulation and compare its results.

![Inject and heal an etcd network partition in the Seedbox TUI](docs/assets/seedbox-tui.gif)

## Try it

Start on Linux x86-64 with Nix and Docker or Podman running. The default PV
backend needs access to `/dev/kvm`; the software TCG backend does not.
Apple Silicon builds also need Apple's Command Line Tools and Docker Desktop
or Podman. Mac execution and replay still need hardware verification.

```sh
git clone --recurse-submodules https://github.com/gardnervickers/seedbox.git
cd seedbox
nix develop "git+file://$PWD?submodules=1"
cargo xtask build-assets
cargo build --release
./target/release/seedbox doctor
./target/release/seedbox init ~/seedbox-quickstart --template etcd
./target/release/seedbox run ~/seedbox-quickstart
./target/release/seedbox replay <sim-id>
```

For an existing clone or a new Git worktree, run
`git submodule update --init --recursive vendor/dynamorio` before entering the
development shell. The explicit Git flake reference includes local tracked DR
edits even when Seedbox itself is clean; direnv and `cargo xtask build-assets`
use it too. Add new DR files to its Git index before building. Nix's source
accessor traverses DR's three nested dependencies even though Seedbox does not
build drsyms. See [translator development](docs/internals.md#editing-dynamorio).

`run` prints `PASS: <sim-id>` when the oracle finishes. Use that ID in the
replay command; it should print `MATCH: <sim-id>`. The etcd example runs three
members and a short consistency check with eight writers and 20 writes per
writer. Allow extra time for the initial image downloads and build.

The build embeds the runtime: guest kernels, init, progress client and helpers.
`doctor` checks the local runtime inputs and host access before any application
images are prepared. Running Seedbox does not invoke Nix or a compiler. Docker
or Podman prepares application images; replay uses retained inputs without the
image engine or source project.

To use a separate complete runtime package, set `SEEDBOX_ASSET_DIR` to the
directory containing its host-architecture subdirectory (for example,
`x86_64/driver-runtime.tar.gz`).

If no server is running, the CLI asks before starting a persistent local one.
For scripts, start `seedbox serve` explicitly. `seedbox shutdown` stops the server
and its simulations. Keep the server running between `run` and `replay`.

## Explore in the TUI

The animation above shows the [etcd example](examples/etcd/seedbox.yaml).
For longer fault experiments, increase the oracle's `--writes` and `--duration`
arguments in your generated `seedbox.yaml` before starting. The short defaults
are intended for the first PASS and replay, not an extended fault session.

```sh
./target/release/seedbox run ~/seedbox-quickstart --paused --detach
./target/release/seedbox tui <sim-id>
```

In the TUI, press **Space** to resume. Press **f**, choose **Partition network**,
put `n1` on one side and `n2` / `n3` on the other, review the expected targets
and effects, and submit with **Enter**. The form shows the action category and
highlights previewed machines and links on the map.
Choose **Heal fault** to restore communication. Fault menus pause the simulation
and restore its previous playback state when closed. Click a pane to focus it;
use the mouse wheel to scroll logs. The **3 timeline** tab lets you edit recorded
faults and submit a new simulation.

## Run your application

Start with [the etcd example](examples/etcd/seedbox.yaml), or generate a project:

```sh
./target/release/seedbox init my-test --template etcd
./target/release/seedbox run my-test
```

`seedbox.yaml` defines machines and a separate **oracle**: the program that drives
the test and checks its results. Oracle exit code zero means pass; nonzero means
fail. An execution that ends without an oracle exit has no completed test verdict.
Set `memoryMiB` on a machine or the oracle to choose its RAM independently.
Omission uses 1024 MiB; supported sizes are 64–3072 MiB for ARM64 guests and
64–65536 MiB for x86-64 guests. Preparation checks the image archive’s boot memory
requirement; applications may need more.

Images must match the guest architecture: PV uses `linux/arm64` on Apple
Silicon and `linux/amd64` on Linux x86-64; TCG uses `linux/amd64` on either
host. Pull private images with Docker or Podman first; Seedbox does not integrate
registry credential helpers.

## Inspect and repeat

Using `target/release/seedbox` as `seedbox`:

```sh
seedbox get simulations
seedbox get capacity                    # Occupied slots and queue reasons.
seedbox describe simulation <sim-id>
seedbox logs <sim-id>                    # Recent application output, all machines.
seedbox logs <sim-id> n1 --follow         # Stream one machine's output.
seedbox events <sim-id> --follow          # Structured simulator events.
seedbox replay <sim-id>                  # Rerun and compare results.
seedbox diagnostics <sim-id>             # List completed raw worker/console evidence.
seedbox export <sim-id> -o saved.seedbox.tar
seedbox import saved.seedbox.tar          # Fresh completed resource; no execution.
seedbox describe simulation <sim-id> > simulation.yaml
seedbox run simulation.yaml              # Submit the saved definition.
```

An omitted seed is chosen randomly. `rerun` repeats the oracle test; `replay`
also compares retained results and reports `MATCH`, `MISMATCH`, or
`NOT_COMPARABLE`.

`run` and `explore` use `--backend pv` by default. On Linux x86-64, select the
software CPU with `seedbox run ~/seedbox-quickstart --backend tcg`; it is slower
but does not require KVM. The run records its backend. `rerun`, `replay`, and saved
resolved definitions keep that recorded backend when the flag is omitted;
a conflicting explicit `--backend` is rejected. To choose a different backend,
run the original project again. There is no backend environment variable or
`seedbox.yaml` setting. Use `seedbox help` or `seedbox help COMMAND` for options.

Simulation IDs, observations, and bounded log buffers last for one server
session. Binary assets survive restarts in the server's cache. Saved YAML needs
those assets; it is not a portable bundle. Export a completed, resolved simulation
to save its inputs, checkpoints, results, available raw diagnostics and bounded
log/event history independently of the session. Import registers a fresh independent
completed resource; use its ID to describe, compare, rerun, replay or branch from a
retained checkpoint. Export rejects active simulations. Imports are non-idempotent:
a cancelled or disconnected upload may still complete, so do not retry blindly.
Frames and missing/evicted observations are not recovered; omissions remain explicit.
`SEEDBOX_STATE` selects the server storage directory. The cache is not automatically pruned.

[Design](docs/design.md) explains the architecture and principles.
[Technical details](docs/internals.md) covers determinism, fault ordering,
checkpoints, and development checks.
