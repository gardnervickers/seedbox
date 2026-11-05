# Spawn a machine from the oracle

Run this project from the Seedbox checkout:

```sh
seedbox run examples/spawn
seedbox replay <sim-id>
```

The Python oracle checks `server`, then calls
`World.spawn("replica", {...})` with the same entry a `machines` section of
`seedbox.yaml` takes. The simulation waits while Seedbox builds the image, then
boots `replica`. The oracle confirms a second spawn of the name is rejected and
that both machines answer with their own hostname and role, then exits zero.

The replay boots `replica` from the machine the first run kept, without
building it again.

The `files` entries copy the checkout's Python control client into the oracle,
so run this from the checkout rather than copying this directory alone.
