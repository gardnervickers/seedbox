# Frozen interfaces

All major interfaces in this crate are frozen:

- Machine (`Machine`)
- Clock (`Clock`)
- RNG (`RngStream` and `RngStreamExt`)
- Driver (`Driver` and its public supporting types)

Agents must obtain explicit authorization from the user before changing these
interfaces or their documented contracts. This includes adding, removing, or
changing public methods, types, fields, variants, constants, bounds, or default
trait implementations.

Permission to implement a feature, fix a bug, or refactor an implementation does
not authorize changing an interface. If a task requires an interface change, explain the
specific proposed change and ask the user before making it.
