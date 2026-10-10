# Project and simulation documents

`seedbox.yaml` is the human-authored application description. The CLI translates
it to a simulation submission. The server resolves omitted defaults and image
references before execution. Direct API submissions may omit architecture and
default settings; the server chooses them. Registry images are resolved and
prepared by the server. Local build contexts and initial files currently require
explicit paths accessible on the server host; remote file upload is not yet
implemented. A resolved simulation definition identifies the
exact inputs used; resubmitting it does not read the original project again.

## Authoring a project

```yaml
name: example
machines:
  app:
    image: example/application:latest
    # Optional process settings inherit image defaults when omitted.
    entrypoint: [/usr/local/bin/application]
    command: [--config, /etc/app.yaml]
    environment:
      LOG_LEVEL: info
      REMOVE_INHERITED_VARIABLE: null
    workingDirectory: /app
    user: '1000:1000'
    # Initial contents are copied, never mounted from a live host directory.
    files:
      - source: ./app.yaml
        target: /etc/app.yaml
    # Names are machine-local. Fresh simulations start with empty volumes.
    volumes:
      data:
        target: /data
oracle:
  # Use exactly one of image or build for every machine and the oracle.
  build:
    context: ./oracle
    dockerfile: Dockerfile
  command: [./test]
  environment:
    ADDRESS: http://app:8080
network:
  # Optional baseline latency. Other network changes use the fault API.
  latency: 1ms
simulator:
  # Optional translated basic-block budget between PV progress calls.
  blocksPerProgressCall: 10000
```

`oracle` has the same launch fields as a machine and is not also declared in
`machines`. The name `oracle` is reserved. Its exit code alone determines the
verdict: zero passes, any nonzero exit fails. The oracle is authorized to use the
guest control API. It should handle application readiness and drive the test,
including any scripted faults. All declared machines start automatically.

`entrypoint: []` or `command: []` clears an image default. Arguments are passed
without implicit shell parsing. Environment values are strings; `null` removes
an inherited variable. The host environment is never copied implicitly.

Build contexts and initial file sources are relative to the project file.
`dockerfile` is relative to its build context. Initial directory inputs copy
regular files recursively; symlinks are rejected. Guest paths must be normalized
absolute paths. Files and volumes cannot overlap. Volume names are scoped to a
machine; they do not define shared storage between machines.

Network latency must be a positive duration in whole nanoseconds, using `ns`,
`us`, `ms`, `s`, `m`, or `h`. Fractional units such as `2.5ms` are accepted.
The progress budget is a positive 32-bit number of translated basic-block entries,
not a host CPU frequency or an instruction count. It affects execution and is
retained with the simulation.

No time limit is part of the authored definition. Use `--wall-time 5m` to bound
one execution, `--paused` to start paused, and playback commands to control its
speed. These are runtime choices, not experiment identity. The server's separate
preparation deadline still applies.

## Resolved definitions and recordings

The resolved simulation has a single architecture, explicit seed and initial
clock, runtime inputs, application machines, a dedicated oracle, inline world
settings, starting state, and an ordered list of external overrides. Structured
settings are inline. Binary assets such as kernels and prepared boot archives
are identified by content digest. A complete portable transfer must include the
transitive binary asset closure; YAML alone needs those assets to be available.
The server retains immutable binary assets in its state directory's `cache/`
across shutdown and abandoned-session cleanup. Saved YAML can be resubmitted
after restarting with the same state directory, while simulation IDs and run
observations remain ephemeral. Saving YAML does not export binary bytes to
another server or state directory. Definition bundle export/import is not yet
implemented.

Labels describe groups for filtering. Annotations may record provenance, such as
`derived-from`, but are not execution inputs or ownership links. Independently
submitted definitions do not depend on their source simulation remaining present.
Each resolved image records its source tag, when available, and image digest.
The machine's `bootArchive` identifies the actual prepared guest bytes, including
its runtime helpers and initial files. Resubmission uses that archive; changing
the descriptive image tag does not pull or rebuild it. Missing binary assets
produce an error rather than falling back to the current value of a tag.

The TUI maintains one editable working copy. Saving creates a new immutable
definition; submitting captures an immutable snapshot even without a save.
Subsequent editing does not change an already submitted execution. Removing a
fault that has happened cannot undo its live effects; it changes the next fresh
run. A checkpoint start preserves its already-executed prefix; only its pending
overrides can be changed.

Oracle-generated actions execute as part of the oracle program. External
CLI/TUI interventions are ordered overrides merged by the same deterministic
scheduler on every run. Recorded oracle events are observations, not instructions
to inject those actions again. Same-time actions retain a defined stable order;
timestamps alone are not deduplication identities. Repeating an edited definition
repeats the new experiment; its unchanged prefix reproduces until the first edit.

Logs, events, catalogue IDs, timing measurements, and current status are execution
observations. They are separate from the immutable definition.
