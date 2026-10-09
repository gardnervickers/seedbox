# ARM exclusive-store replay reproducer

`retry.c` increments one local integer 100 million times with LDXR/STXR and
prints the number of failed stores. There are no competing application threads.
A conditional branch between load and store makes DynamoRIO split the sequence
across translated blocks. The final integer is always correct; the retry count
is observable and can differ between run and replay on Apple Silicon HVF.

Build the fixture on Linux ARM64, including an ARM64 Linux build container:

```sh
nix develop --command bash tools/dbi-spike/arm-exclusive/prepare.sh /tmp/arm-exclusive
```

Copy that directory to the Mac if built elsewhere. With this draft's release
CLI, ARM kernel and rebuilt runtime assets, run through the public interface:

```sh
seedbox run /tmp/arm-exclusive --full-state
seedbox replay RUN_DIRECTORY
seedbox logs RUN_DIRECTORY
seedbox logs REPLAY_DIRECTORY
```

Use `--kernel` and `--runtime-assets` for nondefault build locations, and pass the
same `--state` to both commands if using a nondefault content store. A successful
application exit is not a replay match. Keep generated binaries and run evidence
outside the repository. Do not replace differing retries with a looser oracle.

Controls used in the investigation:

- Remove `cbnz %0, 1f` and `nop` from the assembly (keep label `1:` or remove it
  too): the single-block version printed zero failures and matched in this trial.
- Add DynamoRIO's `-unsafe_build_ldstex` to the **supervisor's translator
  arguments**: packing reduced failures but still mismatched.
- Use the pre-instrumentation supervisor from commit `42381a5b` via `--guest-init`,
  with the same kernel and fixture: even native execution mismatched.
  Build that supervisor with its original headers in a separate source directory;
  it is a diagnostic control, not an application configuration requirement.

Observed counts, retained run IDs, Node controls, limitations and questions for
review are in [the migration findings](../../../docs/automatic-progress.md).
