# Run this project

The `etcd` template is a short three-member consistency check with eight writers
and 20 writes per writer. The `redis` template checks 32 acknowledged writes and
exact read-back values. Both use a separate oracle to determine the verdict.
For longer etcd fault experiments, increase the oracle's `--writes` and
`--duration` arguments in `seedbox.yaml`: 20,000 writes per writer with
`--duration 100000s` took about 21 minutes on a Linux x86-64 PV test host;
TCG can take longer.

Start Docker or Podman, then run from this project directory:

```sh
seedbox run .
seedbox replay <sim-id>
```

`run` prints `PASS: <sim-id>` when the oracle finishes. Use that ID in the replay
command; it should print `MATCH: <sim-id>`. The CLI asks before starting a local
server if needed. Keep it running between the commands; `seedbox shutdown` stops
it and its simulations. Use `seedbox tui <sim-id>` to inspect a simulation.

The default backend is `pv`. On Linux x86-64, `seedbox run . --backend tcg`
selects a slower software CPU without requiring KVM. The backend is recorded;
`seedbox rerun <sim-id>` and `seedbox replay <sim-id>` reuse it when the flag
is omitted. Select a different backend by running the original project again,
not by changing `seedbox.yaml` or an environment variable.

Edit `seedbox.yaml` and the oracle to change the test. Oracle exit code zero
passes; nonzero fails. After completion, `seedbox export <sim-id>` saves a portable
bundle; `seedbox import <bundle>` restores a fresh completed resource without
execution. Export rejects active simulations; uncertain imports must not be retried
blindly. Bundles preserve available bounded history, not evicted logs or frames. Use `seedbox help` for commands and options.

[Seedbox documentation](https://github.com/gardnervickers/seedbox#readme)
