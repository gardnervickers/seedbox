# Frozen interfaces

All major interfaces in this crate are frozen:

- Machine (`Machine`)
- Clock (`Clock`)
- RNG (`RngStream` and `RngStreamExt`)
- Driver (`Driver` and its public supporting types)

An interface is not limited to a Rust trait. Public structs, enums, constructors,
inherent methods and their documented behavior are interfaces too. This includes
`Barrier`, `RunContext`, machine definitions, backend factories, configuration,
identities, queues, messages and execution outcomes that support these boundaries.
Changing ownership, ordering, timing, lifecycle, completion, failure or snapshot
semantics requires approval even when the Rust signature stays unchanged.

For the seedbox-nu completion, the user has delegated interface decisions to the
coordinating agent. Subagents must obtain that agent's explicit authorization
before changing these contracts. This includes adding, removing, or changing
public methods, types, fields, variants, constants, bounds, or default trait
implementations. An implementation task is not blanket interface authorization.

The coordinator must first inspect the existing contracts and establish why an
implementation-only correction cannot satisfy the requirement. Prefer clean,
backend-neutral ownership boundaries and replaceable implementations. Do not
expand a contract merely because one backend or test would be easier to write.

Every authorized interface addition or contract change must carry an adjacent
`TODO: Re-evaluate` note, at most 3–4 lines, explaining why this is the right
boundary and why an implementation-only alternative is insufficient. Put the
normative contract in rustdoc, not only in a proposal or implementation report.

Changes to error variants are exempt from this approval requirement. All
non-error public interfaces and their behavior remain frozen.

Permission to implement a feature, fix a bug, or refactor an implementation does
not authorize changing an interface. If a task requires an additional interface
change, explain the exact change and the implementation-only alternatives to the
coordinator before making it. Outside this delegated completion, explicit user
authorization remains required.

# Test layout

Private unit tests live beside their owning code under `src/`, using ordinary
Rust module declarations. Integration targets belong under `tests/` and exercise
public APIs; no source inclusion or visibility expansion may bypass that boundary.
Cross-component helpers stay local to this crate's `tests/support/`. Preserve
portable/native separation and caller-owned shared-resource coordination. Shared guest C sources and guest fixture inputs remain canonical in
`../seedbox-machine/`. Consume common generic specification bodies through its
opt-in `test-support` feature. Backend/common test dependencies must never point
at Driver, including dev-dependencies. Keep native construction and stopped
assertion callbacks local; do not expose native handles or mutable state.
