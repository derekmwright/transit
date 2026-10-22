# Session format

A session is `crates/model` data (`Session` in `crates/model/src/session.rs`), saved as `.transit`
JSON plus an `Audio Files/` folder. Users' work lives in these files. Sessions saved before the
rename have the extension `.scraft` and still open; Save on one writes a `.transit` file beside it
and leaves the `.scraft` alone. The format inside is the same.

## Rules

1. **New fields use `#[serde(default)]`** (and `skip_serializing_if` where an absent value is the
   default), so an old session loads unchanged.
2. **Never `deny_unknown_fields`.** A file written by a newer build must load on the previous build,
   ignoring what it does not know.
3. **Old sessions must load.** Keep fixtures of older files and test loading them.
4. **No silent rewrites.** Unknown or unresolved values (for example route names) are preserved on
   save (`routing.md`).
5. **Hostile input:** treat file contents as untrusted; no panics, bounded allocations
   (`crates/engine/tests/hostile_sessions.rs`).
6. **Runtime state stays out.** Record arm and input monitor are runtime state, not session data
   ([ADR 0011](../../adr/0011-record-arm-and-input-monitor-are-runtime-state.md)).
7. Per-machine settings (devices, paths) belong in `ui.json`, not in the session.

A format change is reviewed like an engine change: the refutation attack list includes "old loads,
new loads on previous build" (`../evidence/refutation-review.md`).
