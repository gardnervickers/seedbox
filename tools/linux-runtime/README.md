# Build Seedbox

```sh
cargo xtask build-assets
cargo build --release
./target/release/seedbox examples/etcd
```

See [build instructions](../runtime/README.md) for host prerequisites and overrides.
