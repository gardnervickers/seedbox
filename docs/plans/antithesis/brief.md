# Planning brief: what Seedbox should build, informed by Antithesis

You are one of two independent planners (the other is a different model; you
will not see its output). Produce a product + architecture plan for Seedbox.

## Inputs

1. **Antithesis documentation** — the complete public docs, fetched today
   (2026-10-16) as raw markdown:
   a local snapshot (not committed; see README.md)
   - `INDEX.md` lists every page with title and size. Start there.
   - `pages/*.md` are the pages (~880 KB total). Read what you need; the
     highest-signal sections are introduction, product/* (writing tests, test
     templates/composer, assertions, instrumentation, faults, results, triage
     report, debugging / multiverse debugging, event sets, notebook),
     reference/* (SDK, fallback assertion schema, event logs, event-set
     reference, configuration), setup/*, workflows/*, ai/*, best_practices/*,
     and release_notes.md (shows recent product direction). The glossary,
     outage-cost and blockchain catalogue pages are low value.
   - The per-language SDK API pages (reference/sdk/<lang>/{assert,lifecycle,
     random,catalog}) 404 on the live site and are missing; the fallback
     assertion schema and SDK overview pages cover the same concepts.

2. **Seedbox** — the repository at this repository (local `main`,
   commit ec80ea9f). Read `AGENTS.md`, `README.md`, `docs/design.md` and
   `docs/internals.md` first, then the code as needed. Key areas:
   `crates/seedbox-spec/src/project.rs` (authoring format: `seedbox.yaml`,
   machines, oracle), `crates/seedbox-engine-core` (deterministic policies,
   faults, triggers, control, virtio console/block/net models, world
   coordinator), `crates/seedbox-server` (catalogue, lifecycle, execution
   service), `crates/seedbox-api`, `crates/seedbox/src/cli/`,
   `crates/seedbox-tui`, `crates/seedbox-control`, `crates/seedbox-vmm`
   (Linux KVM x86), `crates/seedbox-arm64` (Mac HVF), `harness/`.
   Seedbox is a deterministic simulator that runs real Linux guests (OCI images)
   under its own VMM with PV time, binary-instrumented application progress,
   deterministic replay (witness / MATCH), forks and checkpoints, a timeline of
   inputs/faults, an oracle/control API, and a server + CLI + TUI.

## Context from the user

- They asked whether Antithesis analyses timeouts, storage writes etc. to
  inject faults "at just the right time". Public docs say faults are random by
  default, with coverage + assertion feedback steering a branching exploration;
  no disk faults are documented; thread pausing uses instrumentation. Seedbox
  owns the virtual clock, armed guest timers and the virtio block write/flush
  stream, so event-relative fault targeting is a candidate differentiator —
  but evaluate it on the merits, don't assume it.
- Seedbox is under heavy development. Do not propose pins, allowlists,
  qualification gates or provenance markers (see AGENTS.md "Current stage").
- Respect the repo's existing architectural rules (crate boundaries,
  determinism contract, CLI conventions, no wall/sim-time limits in
  definitions, oracle-generated calls, one override timeline, etc.). If you
  think a rule must change for a feature, say so explicitly and why.
- Reusable tooling should be Rust, not Python, unless there's a real reason.

## What to produce

Write a single markdown document (aim for roughly 2,500–5,000 words; dense,
no filler) with these sections:

1. **Antithesis capability map** — a table of what Antithesis lets users do,
   grouped (environment/setup, workload authoring, properties/assertions,
   exploration & faults, results/triage, debugging, workflow/CI/AI). For each:
   one-line description, the doc page(s) you got it from, and Seedbox's current
   status (have / partial / missing) with the Seedbox file(s) that justify it.
   Ground status claims in the code; mark anything you inferred.
2. **Where Seedbox can be better, not just equal** — capabilities Seedbox's
   architecture makes possible that Antithesis doesn't document (e.g.
   event-relative faults, storage faults, local/offline replay, open
   inspection). Be skeptical; say which are real advantages.
3. **Recommended functionality, prioritized** — phased roadmap (e.g. P0/P1/P2).
   For each item: user-visible behaviour, why it matters (bugs it finds or
   workflow it unlocks), dependencies, rough size (S/M/L/XL), and what to
   deliberately not build.
4. **Required architecture** — for the top items, the concrete design: which
   crates/modules own what, new types and data flow, how it interacts with
   the world coordinator, windowed scheduler, timeline/override model,
   checkpoints/forks, replay/witness, server API, CLI/TUI. Cover at least:
   (a) an exploration engine (branching from checkpoints, guidance signal —
   coverage and/or assertions — state selection, budget), (b) a guest-side
   SDK / assertion & property model and how events get out of the guest
   deterministically, (c) fault model expansion including storage and
   event-relative/targeted timing, (d) results: findings, triage, bug
   reproduction and multiverse-style debugging. Call out determinism risks and
   how each design preserves replay MATCH.
5. **Open questions and risks** — the decisions the user needs to make.

Cite Antithesis doc pages by filename and Seedbox code by `path:line` where
practical. Do not edit any files in the repository. Return the full document
as your final answer.
