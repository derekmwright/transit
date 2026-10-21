# Contributing to transit

Issues are welcome: bug reports (include the log from the settings folder, see the README), feature
ideas and questions.

Pull requests should be small, focused and written in the style of the code around them.

Before you open one, run:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo xtask layers
```

Rules:

- No `unsafe` outside `crates/rt` and the plug-in host crates' FFI modules (ADR 0013: unsafe only where it
  is reviewed and tested, with safe fallbacks).
- No panics in non-test code; return errors.
- Use American English in code, comments, UI strings and docs.
- Do not add assets without a row in `ATTRIBUTION.md`.
- The `soundcraft-*` crate names stay for now.

transit is based on [SoundCraft](https://github.com/storytold/soundcraft). Engine fixes may also be
offered upstream to SoundCraft as minimal diffs; if your change is an engine fix, say so in the PR.

By contributing you agree that your work is licensed under MIT OR Apache-2.0, like the project.
