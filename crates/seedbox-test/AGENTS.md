# Test layout

The library owns shared process test utilities; `exec_coordination` tests their
public boundary. Integration tests for another crate belong in that crate's
`tests/`, with native factory entrypoints owned by each backend and generic
Machine scenarios in `seedbox-machine`'s opt-in `test-support` feature.

This crate also owns these portable, cross-component harness scenarios, which
have no single production Rust crate boundary:

- `crate_boundaries`: the complete workspace dependency graph, including build,
  dev, optional and target-specific edges.
- `guest_oracles`: real harness oracle programs, example application behavior
  and the Python control SDK composed without a VM.
- `minio`: example guest/client programs, host staging/generation, console and
  retained-result verification, and the pinned third-party Python SDK together.

Their language adapters stay in `tests/support/`. Do not put Driver, backend,
artifact-store or control-protocol boundary tests here merely to share helpers;
keep their integration entrypoints in the owning crates.
