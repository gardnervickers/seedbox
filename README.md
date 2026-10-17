# Seedbox

> **⚠️ Early, buggy software.** 
> I vibe-coded the vast majority of this so temper your expectations. Expect slop, bugs, and dead code. Nothing api-wise is stable yet. 

Seedbox runs Linux applications in virtual machines with controlled time,
networking, and disk faults. A seed and recorded inputs let you repeat a
simulation and compare its results.

![Inject and heal an etcd network partition in the Seedbox TUI](docs/assets/seedbox-tui.gif)

## Try it

Use an Apple Silicon Mac or Linux x86-64 host. The default PV backend needs
`/dev/kvm` on Linux; software TCG on Linux x86-64 does not.
Install Nix and start Docker or Podman. Macs also need Apple's Command Line Tools.

```sh
nix develop
cargo xtask build-assets
cargo build --release
./target/release/seedbox doctor
./target/release/seedbox run examples/etcd --paused --detach
./target/release/seedbox tui <sim-id>
```

The build embeds the Driver runtime, guest kernel, and helpers. `doctor` checks
the local runtime inputs and host access before any application images are prepared.
Running Seedbox does not invoke Nix or a compiler. Docker or Podman prepares
application images.

If no server is running, the CLI asks before starting a persistent local one.
For scripts, start `seedbox serve` explicitly. `seedbox shutdown` stops the server
and its simulations.

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
Images must match the guest architecture: `linux/arm64` on Apple Silicon,
`linux/amd64` on Linux x86-64. Pull private images with Docker or Podman first;
Seedbox does not integrate registry credential helpers.

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
seedbox describe simulation <sim-id> > simulation.yaml
seedbox run simulation.yaml              # Submit the saved definition.
```

An omitted seed is chosen randomly. `rerun` repeats the oracle test; `replay`
also compares retained results and reports `MATCH`, `MISMATCH`, or
`NOT_COMPARABLE`.

`run` and `explore` use `--backend pv` by default. On Linux x86-64, select the
software CPU with `seedbox run examples/etcd --backend tcg`; it is slower but
does not require KVM. The run records its backend. `rerun`, `replay`, and saved
resolved definitions keep that recorded backend when the flag is omitted;
a conflicting explicit `--backend` is rejected. To choose a different backend,
run the original project again. There is no backend environment variable or
`seedbox.yaml` setting. Use `seedbox help` or `seedbox help COMMAND` for options.

Simulation IDs, observations, and bounded log buffers last for one server
session. Binary assets survive restarts in the server's cache. Saved YAML needs
those assets; it is not a portable bundle. `SEEDBOX_STATE` selects the server
storage directory. The cache is not automatically pruned.

[Design](docs/design.md) explains the architecture and principles.
[Technical details](docs/internals.md) covers determinism, fault ordering,
checkpoints, and development checks.
