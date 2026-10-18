# Seedbox

Run real Linux applications, inject failures, and replay the same execution.
Seedbox controls time, networking, and disk faults inside virtual machines so
you can reproduce bugs instead of hoping they happen again.

> Early, experimental software. Expect bugs and changing interfaces.

![Inject and heal an etcd network partition in the Seedbox TUI](docs/assets/seedbox-tui.gif)

## Get your first PASS and MATCH

Start with **Linux x86-64**, **Nix**, **Docker or Podman**, and access to **`/dev/kvm`**.
The first build and image downloads take a while.

```sh
git clone https://github.com/gardnervickers/seedbox.git
cd seedbox
nix develop
cargo xtask build --release
alias seedbox="$PWD/target/release/seedbox"

seedbox init ~/seedbox-quickstart
seedbox check ~/seedbox-quickstart
seedbox run ~/seedbox-quickstart
```

Accept the prompt to start a local server. The example runs a three-node etcd
cluster and checks its behavior. When it prints `PASS: <sim-id>`, repeat it:

```sh
seedbox replay <sim-id>
```

**`MATCH` means the execution reproduced.** Keep the server running between
commands. For scripts, start it explicitly with `seedbox server start`;
`seedbox server stop` shuts it down and clears the session's simulations.

No KVM? `run --backend tcg` uses a slower software CPU. Apple Silicon also
requires Apple's Command Line Tools and Docker Desktop or Podman; Mac execution
still needs hardware verification.

## Explore a failure

```sh
seedbox run ~/seedbox-quickstart --paused --detach
seedbox tui <sim-id>
```

Press **Space** to resume and **f** to inject or heal faults. Increase the oracle's
`--writes` and `--duration` in `seedbox.yaml` for longer experiments.
Use `seedbox logs <sim-id>` to read application output.

Save an application-ready simulation before a bug, then start a new run from it:

```sh
seedbox checkpoint <sim-id> -o before-bug.seedbox
seedbox run before-bug.seedbox
```

The checkpoint contains its state and inputs, so it works on a fresh server
without the original simulation or cache. Capture briefly pauses the source
and restores its previous playback state.

## Bring your application

Edit the generated `seedbox.yaml`: **machines** run your application; the
**oracle** drives the test and checks the result. Oracle exit code zero means
PASS; nonzero means FAIL. Start with the [etcd example](examples/etcd/seedbox.yaml)
or `seedbox init my-test --template redis`.

Run more seeds with `seedbox run my-test --count 100`.
Use `seedbox help` for commands and `seedbox help run` for options.

[Design](docs/design.md) · [Internals](docs/internals.md)
