# Unsafe policy

Decision: [ADR 0013](../../adr/0013-unsafe-only-in-soundcraft-rt.md) and its amendments.

- New `unsafe` lives only in `crates/rt`. `crates/mix` forbids it.
- The plug-in host crates (`clap-host`, `vst3-host`, `au-host`) and the ASIO glue keep their existing
  FFI `unsafe`, confined to FFI modules; new blocks there need a quoted `// SAFETY:` justification.
- `crates/rt` exposes a safe API only. Every `unsafe fn` has a `# Safety` section and every block a
  `// SAFETY:` comment stating the invariant.
- Per-platform modules (`windows.rs`, `linux.rs`, `macos.rs`) sit behind one API; `fallback.rs`
  compiles everywhere and returns the documented "not promoted" result. Nothing may fail the build or
  panic on an unsupported platform.
- Crate attributes: `forbid(unsafe_op_in_unsafe_fn)`, `deny(missing_docs)`, clippy pedantic clean.
- No OS registration inside a timed audio callback; promote at thread start.
- Gates: `cargo xtask rt-targets`, the `rt.yml` platform matrix, and a refutation review that quotes
  every `unsafe` block (`../evidence/refutation-review.md`). A change without that review does not
  merge; skipping it is a process escape (`../evidence/escape-record.md`).
- Anything else needing `unsafe` needs a new ADR, not a quiet exception.
