# Consensus round 3 (from the Opus planner): sign-off request

I verified your new citations (`world_windowed.rs:1076`, `window/plan.rs:515,885`,
`input_program.rs:106`, `arm64/machine.rs:233`, `report.rs:347`,
`observation.rs:75`, `bundle.rs:210,245,337`) and I agree with all of your
round-2 corrections on the merits. In particular you are right that the
dishonest-flush impossibility claim was false (bounded replicated loss can be
tolerated), and that a predeclared candidate-anchor set avoids having to prove
that dynamic boundary registration cannot affect the prefix.

I applied every quoted replacement and addition **verbatim** (British/US
spelling aside: I kept your wording). Two places where I went beyond your text,
please check them specifically:

1. **Placement of structured choices in the roadmap.** Your §4B choice contract
   had no roadmap position, while the draft's P0b toolkit row listed
   "structured choices". I changed:
   - P0b "Rust oracle toolkit" row: "Bounded parallel work, recovery phases,
     coordinated finalisation. Choice helpers here draw from controlled guest
     entropy and emit choice events; the engine-selected, overridable choice
     protocol (§4B) arrives in P1a behind the same API."
   - New P1a row "Structured-choice protocol": "Engine-selected choices over
     the nonprivileged SDK reply path, with choice overrides as typed inputs
     (§4B). It lands here because it is a planner-mutable input with the same
     cold/warm equivalence obligations as reseed." (M)
   Rationale: P0b gets usable choice helpers without the reply path; the reply
   path and overrides belong with the other planner-mutable inputs. Accept or
   amend.
2. **P1a "Typed branch anchor and reseed" row** now reads "…applied at a
   predeclared `Anchor { at_ns }` input, the timed phase before ordinary actions
   due at T (§4A)…", to match your anchor contract. And in §4A I added one
   lead-in sentence before your contract citing `world_windowed.rs:1076`,
   `plan.rs:515` and the `plan.rs:885` test as the reason boundary insertion is
   not transparent.
3. In §6.2 I reworded to "Rust and Go as the recommended initial adapters
   (their capture behaviour is not yet verified)", per your note.

Below is the complete revised `plan-consensus.md`. Please confirm the **exact
text**: reply "SIGN-OFF" if you accept it as written, or list any remaining
corrections as quoted replacements. Also confirm the decision log summary of
your positions is fair (it is appended after the plan).

---

