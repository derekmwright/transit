# Contributing to transit

Issues are welcome: bug reports (include the log from the settings folder, see the README), feature
ideas and questions.

Pull requests should be small, focused and written in the style of the code around them.

Before you open one, read the contributor docs and run the gates:

- Start at [`docs/contributing/README.md`](docs/contributing/README.md).
- How the engine and UI are built: [`docs/contributing/knowledge/README.md`](docs/contributing/knowledge/README.md).
- What counts as evidence, and the gate commands: [`docs/contributing/evidence/README.md`](docs/contributing/evidence/README.md).
- Known issues: [`docs/known-issues/README.md`](docs/known-issues/README.md). Build notes: [`docs/building.md`](docs/building.md).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask layers
cargo xtask assets
```

Rules:

- No `unsafe` outside `crates/rt` and the plug-in host crates' FFI modules (ADR 0013: unsafe only where it
  is reviewed and tested, with safe fallbacks).
- No panics in non-test code; return errors.
- Use American English in code, comments, UI strings and docs.
- Do not add assets without a row in `ATTRIBUTION.md`.
- The `soundcraft-*` crate names stay for now.

Engine fixes in inherited code may also be
offered upstream as minimal diffs; if your change is an engine fix, say so in the PR.

By contributing you agree that your work is licensed under MIT OR Apache-2.0, like the project.
