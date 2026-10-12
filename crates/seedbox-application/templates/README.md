# Run this project

Start Docker or Podman, then run:

```sh
seedbox run . --paused --detach
seedbox tui <sim-id>
```

Edit `seedbox.yaml` and the oracle to change the test. Oracle exit code zero
passes; nonzero fails. After completion, `seedbox export <sim-id>` saves a portable
bundle; `seedbox import <bundle>` restores a fresh completed resource without
execution. Export rejects active simulations; uncertain imports must not be retried
blindly. Bundles preserve available bounded history, not evicted logs or frames. Use `seedbox help` for commands and options.

[Seedbox documentation](https://github.com/gardnervickers/seedbox#readme)
