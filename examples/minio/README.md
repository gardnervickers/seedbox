# One-service MinIO conditional writes

This example carries forward issue [#48's reviewed revision 2 fixture](https://github.com/gardnervickers/seedbox/issues/48#issuecomment-6036319661), index tree `af8aa8ac73ffe4c731a78d6123bf4b14d2768330` on PR #54 commit `d6647dd2bd29cf0340ddbc5e01a0911f1817630c`. It contributes the MinIO example to [#35](https://github.com/gardnervickers/seedbox/issues/35). **Source and portable controls are tested; real MinIO guest execution and replay remain unqualified.**

There is one real MinIO server and one client/check VM, following the generated-project pattern in `examples/faults/`. The qualification case exercises conditional create and If-Match. Broken and fixed cases deliberately differ in client behavior. No faults or engine changes are needed for this first control.

The client conditionally stores A and writes its acknowledgement file only after HTTP 200. A second conditional create with B must receive 412. The broken client drops the condition and overwrites with B. A separate SDK client GETs the stored bytes and compares them with acknowledged A, producing exit 1 / Seedbox FAIL. The fixed client preserves the conflict and A, producing exit 0 / PASS. This demonstrates a planted client bug, not a MinIO defect, storage durability failure or seed-discovered race. Replay uses the original broken inputs and must MATCH while reproducing FAIL.

Seedbox uses only the configured client exit status. `MINIO_HTTP`, `MINIO_GET` and `MINIO_RESULT` are this application's opaque diagnostic receipts, not a Seedbox format. They retain request conditions, actual HTTP statuses, opaque ETags and tiny body bytes. `verify.py` independently checks those receipts against the retained verdict, workload exit, memory profile, machine placement and replay comparison. Readiness exhaustion prints `infrastructure error: MinIO not ready within 30 s of simulated time` with the last HTTP status or transport error, exits 2 and never starts the workload or emits an application result. Application setup/qualification errors also exit 2. The configured check classifies these exits as FAIL; the verifier explicitly refuses to count them as the planted failure. Use `seedbox inspect` to validate the manifest before this application-specific verifier.

## Inputs and preparation

`inputs.lock.json` and `client/requirements.lock` retain the reviewed pins without repinning:

- Official static linux-amd64 MinIO `RELEASE.2025-09-07T16-13-09Z`, 110,989,496 bytes, SHA-256 `7c5bd8512c6e966455b1d198209358b2d191c77a83ab377c4073281065fb855f`.
- Official Python 3.12.12-slim-bookworm linux-amd64 manifest `sha256:2986c55feb36e6cae00fa1fefb454283e4b33f35e75ff8bdd123b134130be301`.
- Seven hash-pinned wheels: boto3/botocore 1.40.45, s3transfer 0.14.0, jmespath 1.0.1, python-dateutil 2.9.0.post0, six 1.17.0 and urllib3 2.5.0.

The MinIO image is scratch plus the exact binary and `/data`, `/tmp`, `/root`. The client installs only staged wheels using `--no-index --require-hashes`; its Dockerfile does not download dependencies. Binary and wheel bytes remain outside tracked source. The Python base's layers/config and complete built OCI closure are captured by ordinary `seedbox run` preparation, not by these source locks alone. Independently rebuilt OCI images need not be byte-identical; replay consumes captured inputs.

The shared MinIO image sets `HOME=/root` and `MC_CONFIG_DIR=/root/.mc`. This release imports Console and its pinned mc package even with the browser disabled. mc's [global flag initializer](https://github.com/minio/mc/blob/cf909e1063a9/cmd/flags.go) calls `mustGetMcConfigDir()` before parsing `MC_CONFIG_DIR`; [go-homedir v1.1.0](https://github.com/mitchellh/go-homedir/blob/v1.1.0/homedir.go) invokes `getent passwd <uid>` when `HOME` is absent. `HOME` is therefore essential; `MC_CONFIG_DIR` alone cannot bypass that initializer. MinIO and Console's default certificate directories use the same home helper. The remaining server `user.Current()` call only formats a disk-permission error, and mc's user/group lookups describe local filesystem objects. These inspected paths introduce no other startup requirement for `getent`, so the image keeps no shell, BusyBox or `getent`.

Use the project's Python from its development shell. Acquire once into a fresh directory, or reuse the already acquired #48 inputs with the same verification:

```sh
python3 examples/minio/fetch_inputs.py /absolute/minio-inputs
python3 examples/minio/generate.py /absolute/minio-projects \
  --minio /absolute/minio-inputs/minio \
  --wheel-directory /absolute/minio-inputs/wheels
```

The fetcher uses only locked official HTTPS URLs, checks exact size/SHA-256, never retries and preserves incomplete downloads on failure. Stop on drift/unavailable inputs; do not substitute a mirror, latest release or mock. Generation verifies all inputs before each copy, checks copied hashes, refuses existing output, and creates `qualification`, `broken`, `fixed`, each with a self-contained `compose.yaml`. Source `compose.yaml` defaults to qualification; the broken/fixed templates share its build contexts. `stage_inputs.py` can instead stage local verified inputs directly into the source contexts.

This deliberately retains #48's legacy fixture release with optional STS/service-account facilities unused. Earlier ELF inspection reported no interpreter/dynamic section, Go 1.24.6, CGO_ENABLED=0, GOAMD64=v1, source revision `07c3a429bfed433e49018cb0f78a52145d4bedeb`, and embedded `vcs.modified=true/+dirty`. Those provenance limits and the independently unchecked detached binary signature remain; checksum identity is not a signature claim.

## Checks and runtime contract

Portable tests acquire no MinIO substitute and execute no binary or container. They exercise the actual pinned SDK through a fake HTTP transport, with socket creation forbidden:

```sh
# If only client controls are needed on a Mac, download the wheels alone:
python3 examples/minio/fetch_inputs.py /absolute/client-inputs --client-only
python3 examples/minio/portable.py --wheel-directory /absolute/client-inputs/wheels
cargo run --locked -- validate examples/minio
```

The controls verify public header serialization, finite SDK deadlines and no automatic PUT retry, fixed PASS/broken FAIL, acknowledgement after success only, false conflicts, omitted conditions, wrong stored winner, both independent client constructions/barrier arrivals, missing or sequentialized rendezvous and setup failure. Evidence negative controls reject incomplete/duplicate receipts, wrong status/condition/bytes/ETag/verdict, setup exit 2, wrong memory/placement, unexpected faults and missing/mismatching replay. Staging controls reject corruption and overwrite.

The SDK uses explicit fixture credentials and endpoint, path-style SigV4, connect/read timeouts of 1/2 seconds and `total_max_attempts=1`. Readiness explicitly retries ListBuckets on HTTP 500/503 (including server-not-initialized responses), refused/unreachable connections (`EndpointConnectionError`), reset/closed connections (`ConnectionClosedError`), connect timeouts and read timeouts. A 30-second guest monotonic deadline includes SDK call time, with sleeps of at most 0.25 seconds and at most 120 attempts. An in-flight request can finish after the deadline under the SDK's finite timeouts, but cannot establish readiness after it. Other HTTP statuses and unexpected errors fail immediately. PUT ambiguity stops the application. Two independently constructed clients join a two-party barrier with a two-second deadline before conditional create; setup failure aborts it. This coordinates starts, without claiming server-side overlap or a particular winner.

Qualification expects the following real responses and independent GET checks:

| Operation | Required result |
| --- | --- |
| Create absent key with If-None-Match:* | 200, A and returned ETag stored |
| Create same key conditionally | 412, A unchanged |
| Update with exact returned If-Match ETag | 200, B and changed ETag stored |
| Update using stale ETag | 412, B unchanged |
| If-Match PUT to missing key | 404, GET also 404 |
| Coordinated creates of one fresh key | One 200, one 412; GET bytes/ETag equal acknowledged winner |

The initial allocation is **2048 MiB per VM**, 4 GiB configured guest RAM total plus host/preparation overhead. MinIO uses one node/drive, `MINIO_CI_CD=1`, `MINIO_API_REQUESTS_MAX=4`, browser off and the supervisor's online-CPU `GOMAXPROCS` default (currently 1). This is a trial allocation, not a measured minimum or RSS cap. All variants freeze seed 0, 120 seconds simulated time, 1 ms latency/200 us jitter and 2 ms stop tolerance. Execution wall caps are 90 seconds for qualification and 60 seconds for broken/fixed (including their replay). Preparation is outside these execution caps; measure cold preparation and complete command time separately. No multipart, TLS, encryption, versioning, replication or storage faults are exercised. RAM-backed `/data` does not establish fresh-boot durability.

## AMD validation and remaining work

[AMD.md](AMD.md) lists exact public CLI commands, fixtures, exit codes, stop conditions and the fresh offline export/import/replay demonstration. It requires five serial guest jobs: qualification PASS, broken FAIL, retained replay MATCH of FAIL, fixed PASS, offline imported replay MATCH of FAIL. Nothing here was run on KVM. The Linux-only VMM build/test aliases cannot execute on this Mac. [ACCEPTANCE.md](ACCEPTANCE.md) records the source evidence and outstanding hardware evidence.

Follow-up work remains: three independent regions and a user-owned two-of-three ledger; bounded seed exploration and its measured demo; startup/peak memory and real guest compatibility; integrated AMD and fresh-install qualification. Return-loss ambiguity is also deferred. A dedicated eligible connection may use return loss **armed before a known PUT**, followed by independent stored-data verification and observed timeout/ambiguity. That is pre-armed return loss, not a strictly post-commit trigger or transparent selection of an arbitrary HTTP response. Broader connection/causal-trigger work belongs to #61/#62. This example changes no engine, kernel, guest or release policy and does not establish V0 completion.
