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
./build/current/seedbox run examples/runtimes/postgres
./build/current/seedbox replay SIMULATION_ID
```

Replace `postgres` with another directory above.
Run twice and use `seedbox compare SIMULATION_A SIMULATION_B` to check fresh-run determinism.
These are runnable examples, not claims of general application compatibility.
