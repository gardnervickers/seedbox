# AMD KVM example checks

Run from the checkout inside `nix develop` on a Linux x86-64 KVM host with a
running Docker-compatible engine. Build matching CLI and guest inputs with
`python3 tools/build.py`; workers use normal host scheduling without CPU leases or pinning.

Acquire and generate the projects into fresh directories:

```sh
python3 examples/minio/fetch_inputs.py /absolute/minio-inputs
python3 examples/minio/portable.py --wheel-directory /absolute/minio-inputs/wheels
python3 examples/minio/generate.py /absolute/minio-projects \
  --minio /absolute/minio-inputs/minio --wheel-directory /absolute/minio-inputs/wheels
build/current/seedbox doctor --json
build/current/seedbox validate /absolute/minio-projects/qualification
build/current/seedbox validate /absolute/minio-projects/broken
build/current/seedbox validate /absolute/minio-projects/fixed
```

Run each project and save its printed simulation ID:

```sh
build/current/seedbox run /absolute/minio-projects/qualification --seed 0
build/current/seedbox run /absolute/minio-projects/broken --seed 0
build/current/seedbox replay BROKEN_SIMULATION_ID
build/current/seedbox run /absolute/minio-projects/fixed --seed 0
build/current/seedbox describe simulation BROKEN_SIMULATION_ID --json
```

The expected oracle results are PASS, FAIL and PASS, with a MATCH for replay of
the broken simulation. Guest assertions own those results. Attach
`seedbox logs ID [MACHINE]` and `seedbox events ID` while a run is active to
collect client-owned diagnostic evidence. New simulations use exact stop
boundaries; removing the previous 2 ms tolerance changes those worlds' results.
Do not silently retry failures or change memory/deadlines to obtain a pass.

Runtime input overrides belong to `seedbox serve`, not run/replay. Start a
private foreground server with `SEEDBOX_STATE=/absolute/private-state` and any
needed `serve --kernel`, `--vmm` or `--guest-init` overrides, then use the same
`SEEDBOX_STATE` for its clients. Full-state capture is `serve --full-state`.

The former `verify.py` workflow reads historical offline manifests and archived
console output; live server simulations do not create those stream archives.
Offline bundle import does not register simulations in the server catalogue,
so imported run directories cannot be submitted to `replay ID`. Historical
export/import readers remain available, with `SEEDBOX_ARTIFACT_STATE` selecting
their store independently from the server.

Native MinIO acceptance of this current workflow remains pending. Record actual
receipts and cleanup evidence before claiming compatibility or determinism.
