# Spawn a standby machine

Run this project from the Seedbox checkout:

```sh
seedbox run examples/spawn
seedbox replay <sim-id>
```

`replica` sets `standby: true`, so it is prepared with the other machines but
does not boot. The Python oracle checks `server`, confirms nothing answers at
`replica`, then calls `World.spawn("replica")` and checks both machines answer
with their own hostname and role. The oracle exits zero on success.

The `files` entries copy the checkout's Python control client into the oracle,
so run this from the checkout rather than copying this directory alone.
