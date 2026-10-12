# Archived investigation tools

These are preserved experiment sources, not supported product commands. Read
[`docs/performance-handoff.md`](../../../docs/performance-handoff.md) first.
No builds or tests were run when consolidating this snapshot.

- `java-cost/`: the actual freeze, five-pair measurement, replay and offline
  audit scripts. They were copied unchanged from ignored experiment output.
  They assume companion bundles, a signed CLI and a private state directory
  beside the scripts; `freeze.py` also contains the original absolute evidence
  path. **Do not run from this archive without first adapting those paths and
  providing the retained inputs.** `results.json` is the compact measured summary,
  not the full raw evidence. The scripts start servers and execute guests.
- `cds/`: unfinished Kafka AppCDS Dockerfile, training script and original
  Dockerfile diff. Deliberately not installed into the live Kafka example.
  The experiment timed out; no Seedbox performance benefit was established.
- `safepoint/`: historical reproducer/build scripts. Paths and pinned source
  variants describe the old experiment, not the current runtime. The original
  raw blocking clean-call negative is supposed to hang under forced IC sync;
  do not confuse it with the later annotated ownership fixtures.
- `hvf-probes/`: IC/DIC probe sources and scripts; not a new PMU qualification
  campaign. Preserve provenance when adapting them.
- `kafka-harness/`: historical measurement scripts, not a replacement public
  application runner. The original Kafka goal remains paused.
- `patches/blocked-combined-stack.patch.gz`: complete unaccepted experiment delta
  from `23e651fd` to `2f44e6a5`. **Do not apply wholesale:** signal, vfork,
  cleanup and sighand ownership blockers remain unresolved.
- `patches/eager-ic-experiment.patch.gz`: earlier eager-IC experiment delta from
  `adcb1c4e` to `563008c0`. Later DR work already includes evolved IC handling;
  applying this patch on top would duplicate/conflict with it.

- `patches/performance-ledger.patch.gz` and `translator-census.patch.gz`:
  previously uncommitted diagnostic instrumentation, both based on `adcb1c4e`.
  Preserved rather than installed into the runtime. Their local preservation
  commits are `810e9a5d` and `0a4663d5`; the full source delta is in these files.

Patch archives are gzip-compressed to preserve original unified-diff context
bytes exactly (including space-only context lines). Decompress before inspecting;
applying one requires its stated base and explicit authorization.

The custom executor is preserved in `crates/seedbox-executor`, not this archive.
DR runtime patches and source-owned regressions remain in `guest/progress`.
