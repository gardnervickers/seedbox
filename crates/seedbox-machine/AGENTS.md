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

Agents must obtain explicit authorization from the user before changing these
interfaces or their documented contracts. This includes adding, removing, or
changing public methods, types, fields, variants, constants, bounds, or default
trait implementations.

Changes to error variants are exempt from this approval requirement. All
non-error public interfaces and their behavior remain frozen.

Permission to implement a feature, fix a bug, or refactor an implementation does
not authorize changing an interface. If a task requires an interface change, explain the
specific proposed change and ask the user before making it.
