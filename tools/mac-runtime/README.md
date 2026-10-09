# Apple Silicon macOS

Install Nix and Apple's Command Line Tools. For the explicit guest-asset build,
start a Docker-compatible Linux engine such as Podman or Docker Desktop:

```sh
nix develop
bash tools/build-assets.sh
cargo build --release
```

Cargo builds and signs the native CLI with the Hypervisor entitlement. The asset
command builds Linux guest inputs in a temporary container and installs them in
the OS temporary directory. See [asset builds](../runtime/README.md).

Seedbox uses Bollard to build, pull and export `linux/arm64` images through your
running Docker or Podman engine, then executes guests through Apple's Hypervisor
framework. It never starts or manages the engine's VM. The release CLI embeds
its guest helpers; pass only the kernel and Compose file. Replay needs no engine.
See the [etcd quick start](../../README.md#quick-start).

Guest memory is 64..3072 MiB per service. `explore --workers N` (1..32) runs N
seeds at once, each in its own engine process and HVF VM, with no core
reservation; results never depend on the worker count. Arbitrary ARM
snapshot/fork reuse remains unsupported. `--cpu`, `--vmm` and the `x-seedbox.machine` `page_timer` and `deferred_tx` options are Linux-only.
