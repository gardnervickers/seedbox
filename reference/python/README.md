# Archived Python prototype

The original proof of concept is retained here for inspection and comparison.
Its modules, tests, scripts, and original documentation preserve the first
implementation. The historical commands and paths in these files refer to the
original project layout.

The active runtime, CLI, models, and acceptance suite are now Rust. This directory
is excluded from the Nix package and is not required to build or run Seedbox.
The Rust tests compare healthy seed-42 runs against the original event traces and
guest results saved under `tests/golden/` in the project root.
