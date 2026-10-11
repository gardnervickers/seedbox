# Spawn a machine from an oracle

Run this native `seedbox.yaml` project from the Seedbox checkout:

```sh
./target/release/seedbox run examples/spawn
./target/release/seedbox replay <sim-id>
```

The Python oracle connects to the guest control socket, checks `server`, then
calls `World.spawn("server", name="replica", env={"ROLE": "replica"})`. It uses
the returned address to reach the new machine and checks that the original
server kept its own hostname and environment. The oracle exits zero on success.

The `files` entries package the current checkout's Python control client into
the oracle. The server does not receive the control client or socket. Run this
from the checkout rather than copying this directory alone.
