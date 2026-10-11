# Run this project

Start Docker or Podman, then run:

```sh
seedbox run . --paused --detach
seedbox tui <sim-id>
```

Edit `seedbox.yaml` and the oracle to change the test. Oracle exit code zero
passes; nonzero fails. Use `seedbox help` for commands and options.

[Seedbox documentation](https://github.com/gardnervickers/seedbox#readme)
