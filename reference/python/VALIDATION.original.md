# Validation on 2026-10-05

Tested on macOS, Apple Silicon (`aarch64-darwin`), with the project Nix lock.
The runtime dependencies were Python 3.14.7, Unicorn 2.1.4, Capstone's Python
package 5.0.7, and pyelftools 0.32. The other declared Nix systems have not been
tested on their corresponding hosts.

| Check | Observed result |
| --- | --- |
| Execution suite | 38 tests passed |
| Clean Nix execution check | Built successfully; includes suite and 64-seed acceptance script |
| Built Nix CLI | Completed the cluster workload; report byte-identical to the development-shell run |
| Fresh-process reruns | Entire reports byte-identical |
| Different Python hash seeds | Entire reports byte-identical |
| Rerun with two additional host CPU workers | Entire report byte-identical |
| Intentional two-thread race, seeds 0–63 | 64 failures of the counter invariant; values ranged from 38 to 58 |
| Atomic fix, seeds 0–63 | Counter invariant passed in all 64 runs |
| Total UDP packet loss | Expected deterministic deadlock |
| Seed 42 cluster | Completed 130 execution steps in 1,000,016,020 ns of virtual time |

The seed 42 cluster's entered-instruction digest was
`50b00b04e3b86d0f16aafd325ed1916ea94625c9383b49086f17bd32c5651ab5`.
Its event-trace digest was
`0d9056c7c8fd6654f3009b6f4707d31e0ebc6d3b3c0e5c1ae9bf7ab0a222e38e`.
Report digests also depend on image bytes and the implementation fingerprint;
use the full report for identifying a reproducer.

The CLI help command worked within the Codex command sandbox, but guest
execution there terminated with SIGILL. The same built CLI executed successfully
outside that command sandbox, as did the Nix development and clean-check runs.
Unicorn uses executable memory for its instruction engine; compatibility with
an additional OS sandbox needs separate verification.

Reproduce the checks from the repository root:

```sh
nix develop --command python3 -m pytest -q
nix develop --command python3 scripts/verify.py --seeds 64
nix flake check
```

The acceptance script writes local reports and a summary under `artifacts/`.
These results cover the explicit prototype ABI and small native workloads.
General C/Rust runtime compatibility, container workloads, cross-host equality,
and useful throughput on real services remain future acceptance milestones.
