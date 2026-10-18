# Machine contracts and tests

- Contracts live in rustdoc beside Machine, RunContext, definitions, clocks and RNG;
  internal APIs are not frozen. Driver scheduling and backend implementations live
  in their own crates, not this contract crate.
- Reusable feature-gated scenarios live in `src/test_support/`; integration-only
  helpers in `tests/support/`. Backend entrypoints supply machines/factories.
  Do not expose internals or include another crate's source to share tests.
- Do not add `tests/main.rs`: Cargo treats it as another integration target.
- Guest C/headers and fixtures stay in `src/pv/`: `guest.nix` collects them and tests
  compile them by path. Follow root test rules: no sleeps or annotations.
