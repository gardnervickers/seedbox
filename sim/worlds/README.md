# Engine fixtures and legacy fault campaigns

Application authoring now uses [Compose examples](../examples/README.md) and the
Rust `seedbox run` / `seedbox replay` commands. World JSON is generated internally
by that adapter; new application tests should not construct guest initramfs files
or encode configuration in kernel command-line arguments.

| Previous definition | Current path / reason for retention |
| --- | --- |
| `cluster.json` | Replaced by `../examples/etcd/compose.yaml`, using the same acknowledgement-ledger checker in an OCI workload image. The acceptance template uses a fixed 160-write target rather than timed load. |
| `etcd1.json` | Retained as a low-level single-guest boot fixture. Use the tested Compose etcd cluster for application assertions. |
| `kill-3.5.2.json`, `kill-3.5.3.json`, `sanity.json` | Retained legacy fault campaigns for #3. Their seeded restarts have not been silently removed or claimed equivalent to the no-fault Compose examples. |
| `cluster-fixed.json`, `single-vm*.json` | Retained placement/network comparison fixtures for #10; Compose currently uses one VM per service. |
| `smoke.json`, `net.json`, `race.json` | Retained low-level kernel/device/scheduler tests. |

To reproduce a legacy fault hunt, build its guest artifacts and use
`sim/batch.sh` / `sim/campaign.sh` with the unchanged JSON file. From the repository
root inside the project Nix shell, with `SEEDBOX_ROOT` naming the runtime tree:

```sh
bash sim/guest/build-client.sh "$SEEDBOX_ROOT/artifacts/client"
bash sim/guest/build-initramfs.sh "$SEEDBOX_ROOT/artifacts/cluster-3.5.2.cpio" \
  sim/guest/cluster.init "$SEEDBOX_ROOT/src/etcd/v3.5.2/etcd:bin/etcd" \
  "$SEEDBOX_ROOT/artifacts/client:bin/client"
sim/batch.sh sim/worlds/kill-3.5.2.json demo "7 7"
sim/campaign.sh sim/worlds/kill-3.5.2.json hunt 1000 8
```

These commands require the legacy runtime layout, controlled kernel and etcd
binary; they are not needed for Compose runs. Both old and new
executors share the CPU/SMT lease protocol. The old workload build helper reads
the checker source now housed in `sim/examples/etcd/workload`.

Compose rejects nonempty `x-seedbox.faults` until #3 supplies qualified shared
fault operations and effect-time acknowledgements. The legacy hunts remain
available; their bug-finding performance is not established by the new examples.
