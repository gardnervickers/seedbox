# Application runtime examples

These optional examples exercise third-party applications in Seedbox. They are
not part of the default acceptance suite.

| Directory | Workload |
| --- | --- |
| `jvm` | Java hostname lookup |
| `nginx` | HTTP request and access logging |
| `node` | Node HTTP server/client |
| `postgres` | Database initialization, SQL query and POSIX shared memory |
| `python` | Non-root Python process pool |
| `tls` | OpenSSL-generated certificate and Python TLS exchange |

After building the release CLI and guest assets, with Docker/Podman available:

```sh
./target/release/seedbox run examples/runtimes/postgres --timebase pv --full-state
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

On Mac omit `--timebase pv`. Replace `postgres` with another directory above.
Run twice and use `seedbox diff RUN_A RUN_B` to check fresh-run determinism.
These are runnable examples, not claims of general application compatibility.
