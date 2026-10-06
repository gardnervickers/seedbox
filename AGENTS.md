# Working on Seedbox

Seedbox runs Compose applications as a deterministic world of real Linux
machines on KVM. Read [README.md](README.md) first; it has the layout table.

## Workspace

- `crates/seedbox` is the default package: `cargo run -- <command>` runs the CLI.
- `crates/seedbox-vmm` is Linux-only; build it with `cargo build -p seedbox-vmm`
  or `cargo build --workspace`.
- New crates go in `crates/<name>/` and inherit `version`, `edition` and
  `publish` from `[workspace.package]`. Put a dependency used by two or more
  crates in `[workspace.dependencies]`. There is one `Cargo.lock`; commit it.
- `research/` is frozen evidence: standalone, version-pinned crates and records
  of past runs, excluded from the workspace. Don't edit it to fit code changes
  elsewhere, and don't add it to the workspace.
- `harness/` holds the Python acceptance suite and legacy world-file runners;
  `kernel/` and `guest/` hold guest build inputs.

## Identity and determinism

- The VMM binary's hash, the guest kernel, the guest supervisor
  (`guest/compose-init.c`, embedded by `crates/seedbox/build.rs`) and the
  semantic parameters together make up a run's identity. Any change to
  `crates/seedbox-vmm`, including reformatting (it moves panic line numbers),
  or to `[profile.release]` produces a new engine. Don't reformat or tidy the
  VMM as a side effect of other work.
- Performance settings (`--margin`, `--undo`) must never change results.

## Checks

Run these before committing:

```sh
cargo fmt -p seedbox --check
cargo clippy -p seedbox --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash harness/acceptance.sh portable
```

The `seedbox-vmm` crate has existing clippy findings. Don't fix them in an
unrelated change (see above).

## CLI

The CLI lives in `crates/seedbox/src/cli/`:

- `mod.rs`: argument definitions, dispatch and output.
- `defaults.rs`: how each runtime input is found when no flag is given.
- `signals.rs`: cancellation.

`src/main.rs` only calls `cli::main()`. The library (everything else in `src/`)
never parses arguments, reads environment variables or prints. The CLI is
that boundary.

Follow these rules when you change the CLI, and update this section if you
deliberately change one.

### Arguments

- Required flags are a last resort. Every runtime input needs a sensible
  default or a search, so `seedbox run` works with no arguments on a set-up
  host. The only required inputs are those with no possible default (`init
  DIRECTORY`, `prepare --output`).
- A command's main object is positional: the Compose path (default `.`,
  and a directory resolves to its `compose.yaml`) or the run directory.
- Flags are long kebab-case. Don't add short flags except `-o`/`-h`/`-V`.
  Boolean flags are presence-only (`--json`, `--full-state`).
- A choice from a fixed set uses `#[derive(ValueEnum)]` with a doc comment on
  each variant. Never use string matching followed by `unreachable!`.
- Validate ranges and formats in a `value_parser`, so a bad value is a
  clap usage error that names the flag, not a runtime error later.
- Reuse the shared `#[derive(Args)]` groups (`ComposeArg`, `CpuArg`,
  `StateArg`, `RuntimeInputs`, `Performance`) instead of redeclaring flags.
  When several commands need a new option, add a group.
- Environment variables (`env = "SEEDBOX_<NAME>"`) say where things are: paths,
  CPU, state directory. Never back a parameter that changes results (seed,
  memory, full-state, faults, schedule) with an environment variable; it would
  change a run's identity without appearing on the command line.

### Defaults for a new input

1. Declare it as `Option<PathBuf>` with `env = "SEEDBOX_<NAME>"` in the
   relevant group.
2. Add a resolver in `defaults.rs` that returns `existing("--flag", path)` for
   an explicit value and otherwise searches in a fixed, documented order.
3. On failure, return `missing(what, &searched, fix)`, which lists every
   location tried and one way to fix it.
4. Resolve it in `RuntimeInputs::backends` or earlier, before any slow work
   (image pulls, builds, CPU leases). Fail fast.
5. Add it to the defaults table in README.md's quick start.

### Help text

- Every argument and subcommand has a doc comment. Write it as a short phrase
  starting with a capital letter and ending with a period. A subcommand's line
  is imperative ("Run ...", "Show ...") and names any host requirement in
  parentheses: "(Linux KVM host, root)".
- When clap can't show a default (an `Option` with a computed default), end the
  doc with `[default: ...]` in clap's format.
- Group flags under the existing `next_help_heading` sections: the default
  options, "Runtime inputs", and "Performance (never changes results)". A flag
  takes the heading of the group declared before it, so flatten
  general-purpose groups (`CpuArg`, `StateArg`) before `RuntimeInputs`.
- `run` and `replay` show `EXIT_CODES` in `after_help`.
- Check the result with `cargo run -- help <command>`.

### Output

- stdout carries results only. A command prints one status line
  `WORD: detail`, where `WORD` is the SCREAMING_SNAKE_CASE serde name
  produced by `word()` (`PASS`, `MATCH`, `VALID`, `CREATED`, ...). Any
  further lines are lowercase `key: value` (`replay: MATCH`, `error: ...`).
- Scripts find the run directory with `sed -n 's/^[A-Z]*: //p'` over both
  output streams of `run` and `replay`. Exactly one line may match
  `^[A-Z]*: `, and it must be `OUTCOME: run-directory`.
- `--json` prints exactly one pretty JSON document to stdout and nothing else.
  Its field names come from the library's serde types; don't rename them in
  the CLI.
- Progress and diagnostics go to stderr: lowercase, plain text, no colour,
  no emoji.

### Errors and exit status

- CLI code returns `seedbox::Error`. It never panics, calls `unwrap`, or calls
  `process::exit`. `main` prints `error: <message>` and exits 2.
- Use `Error::Setup` for a problem the user can fix (a missing file, a missing
  tool, the wrong host, a permission). It prints without a category prefix.
  Its message starts lowercase, names the path, flag or value involved, and
  says how to fix it: `--kernel /x/bzImage: not found`, or `skopeo not found;
  install it or enter nix develop`.
- Never let a bare `io::Error` from a user-supplied path or a spawned program
  reach the user. Check the path first (`existing`), or add context as
  `oci::spawn_error` does.
- Exit codes are a fixed contract (`EXIT_CODES`, README.md, docs/runtime.md,
  and the acceptance scripts): 0 PASS/MATCH, 1 FAIL/MISMATCH, 2
  ERROR/NOT_COMPARABLE/usage/setup, 3 INCOMPLETE, 124 TIMEOUT. Don't change
  what a code means. A new code must be added to all three places.

### Tests and docs

- Every new flag, default and error path gets a case in
  `crates/seedbox/tests/cli.rs`. Those tests run `CARGO_BIN_EXE_seedbox` with
  the `SEEDBOX_*`/`HOME`/`SUDO_USER` environment scrubbed. Check exact stderr
  for errors.
- Pure resolution logic gets unit tests in `defaults.rs`.
- When behaviour changes, update `seedbox help`, README.md (quick start and
  defaults table), docs/runtime.md, and `crates/seedbox/templates/README.md`.
  The last one is copied into every project `seedbox init` creates.
