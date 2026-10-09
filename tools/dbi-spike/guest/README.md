# Run the instrumentation experiment inside a PV guest

Use a Linux x86-64 KVM host with the project development shell, built guest assets,
a Docker-compatible engine API socket, and the SDK/client from the parent
directory. Applications are launched by the ordinary public CLI; no host scenario
runner is introduced.

From the checkout root, after the portable suite passes:

```sh
cargo build --release --locked
export SEEDBOX_DBI_SDK=/tmp/seedbox-dbi-spike/DynamoRIO-Linux-11.3.0-1
export SEEDBOX_DBI_BUILD=/tmp/seedbox-dbi-spike/build-native
bash tools/dbi-spike/guest/prepare-context.sh /tmp/seedbox-dbi-guest-context
# If using the existing rootless Podman API:
export DOCKER_HOST="unix:///run/user/$(id -u)/podman/podman.sock"
./target/release/seedbox run /tmp/seedbox-dbi-guest-context/static.yaml \
  --timebase pv --full-state --state /tmp/seedbox-dbi-guest-state
./target/release/seedbox replay ORIGINAL_RUN_DIRECTORY \
  --state /tmp/seedbox-dbi-guest-state
./target/release/seedbox diff ORIGINAL_RUN_DIRECTORY REPLAY_RUN_DIRECTORY --json
./target/release/seedbox logs ORIGINAL_RUN_DIRECTORY --machine app
```

Use the printed run directories. Repeat with `jit.yaml`, then `jit-default.yaml`.
The latter uses ordinary Node settings with no predictability flags. Compare each
configuration only with its own replay. Expected application output is
`73ac161296d8e255` for static and `2308187121` for either Node invocation.

The context destination must be empty to prevent accidental reuse of unrelated
files. The script copies the already-built static executable and client, plus
the minimal x86-64 release SDK libraries. The Dockerfile uses an existing Node
image and performs no application compilation. The partial SDK emits warnings
for omitted debug/32-bit libraries; tested release execution succeeds.

The original configurations only record checkpoints. `spin-progress.yaml` and
`jit-progress.yaml` enable the client `--progress` option, issuing one real
`getpid` syscall per 10,000 block entries. `spin-control.yaml` uses the same
spinlock binary without injected syscalls and intentionally has a 20-second wall
limit. See [progress results](../RESULTS.md) for the positive and
negative evidence. These require rebuilding the client and creating a fresh
context with `spinlock` included; no Seedbox engine or kernel rebuild is needed
when using the already-validated runtime from the first guest experiment.

See [PV results](../RESULTS.md) for retained evidence and limitations.
