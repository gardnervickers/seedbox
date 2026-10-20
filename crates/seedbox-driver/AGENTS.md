# Test layout

Private unit tests live alongside the implementation they test under `src/`,
using ordinary Rust module declarations. Tests spanning components exercise their
public APIs from the owning crate's `tests/` integration targets. Do not expose
internals or include another crate's source to make an integration test work.
Reusable feature-gated specification and test-support libraries live under
`src/test_support/`; integration-only helpers live in `tests/support/`.
Shared PV/TCG scenarios take a machine or factory from a thin backend entrypoint;
keep architecture-specific behavior in its backend. Never add a `main.rs` below
`tests/`, because Cargo treats it as another test target. Shared guest C sources and guest fixture inputs remain canonical in
`../seedbox-machine/`. Consume common generic specification bodies through its
opt-in `test-support` feature. Backend/common test dependencies must never point
at Driver, including dev-dependencies. Keep native construction and stopped
assertion callbacks local; do not expose native handles or mutable state.
