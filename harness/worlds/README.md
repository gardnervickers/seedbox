# Engine fixtures and legacy fault campaigns

Application authoring now uses [Compose examples](../../examples/README.md) and the
Rust `seedbox run` / `seedbox replay` commands. World JSON is generated internally
by that adapter; new application tests should not construct guest initramfs files
or encode configuration in kernel command-line arguments.

| Previous definition | Current path / reason for retention |
| --- | --- |
| `cluster.json` | Replaced by the [etcd Compose example](../../examples/etcd/compose.yaml), using the same acknowledgement-ledger checker in an OCI workload image. The acceptance template uses a fixed 160-write target rather than timed load. |
| `kill-3.5.2.json`, `kill-3.5.3.json`, `sanity.json` | Retained legacy fault campaigns for #3. Their seeded restarts have not been silently removed or claimed equivalent to the no-fault Compose examples. |
| `cluster-fixed.json`, `single-vm*.json` | Retained placement/network comparison fixtures for #10; Compose currently uses one VM per service. |
| `smoke.json`, `net.json`, `race.json` | Retained low-level kernel/device/scheduler tests. |

To reproduce a legacy fault hunt, build its guest artifacts and use
`harness/batch.sh` / `harness/campaign.sh` with the unchanged JSON file. From the repository
root inside the project Nix shell, with `SEEDBOX_ROOT` naming the runtime tree:

```sh
bash guest/build-client.sh "$SEEDBOX_ROOT/artifacts/client"
bash guest/build-initramfs.sh "$SEEDBOX_ROOT/artifacts/cluster-3.5.2.cpio" \
  guest/cluster.init "$SEEDBOX_ROOT/src/etcd/v3.5.2/etcd:bin/etcd" \
  "$SEEDBOX_ROOT/artifacts/client:bin/client"
harness/batch.sh harness/worlds/kill-3.5.2.json demo "7 7"
harness/campaign.sh harness/worlds/kill-3.5.2.json hunt 1000 8
```

These commands require the legacy runtime layout, controlled kernel and etcd
binary; they are not needed for Compose runs. Both old and new
executors share the CPU/SMT lease protocol. The old workload build helper reads
the checker source now housed in `examples/etcd/workload`.

Compose controllers use the [runtime fault API](../../crates/seedbox/README.md#structured-faults)
with correlated process acknowledgments and overlapping partition handles.
The [recovery examples](../../examples/faults/README.md) retain their own inputs and
results. The legacy fixtures remain available with their separate run identities.
