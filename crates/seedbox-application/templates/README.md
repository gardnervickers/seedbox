# Run this project

Start Docker or Podman, then run:

```sh
seedbox run . --paused --detach
seedbox tui <sim-id>
```

The default backend is `pv`. On Linux x86-64, `seedbox run . --backend tcg`
selects a slower software CPU without requiring KVM. The backend is recorded;
`seedbox rerun <sim-id>` and `seedbox replay <sim-id>` reuse it when the flag
is omitted. Select a different backend by running the original project again,
not by changing `seedbox.yaml` or an environment variable.

Edit `seedbox.yaml` and the oracle to change the test. Oracle exit code zero
passes; nonzero fails. Use `seedbox help` for commands and options.

[Seedbox documentation](https://github.com/gardnervickers/seedbox#readme)
