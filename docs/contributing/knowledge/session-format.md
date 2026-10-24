# Session format

A session is `crates/model` data (`Session` in `crates/model/src/session.rs`), saved as `.transit`
JSON plus an `Audio Files/` folder. Users' work lives in these files. The browser has no folders,
so it saves a session as one zip bundle, `.transitz`, holding the `.transit` and its `Audio Files/`.

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

## The `needs` marker

`format_version` only gates files this build is too old for (it refuses them). A feature that an
older build would silently drop instead is listed by `Session::to_json` under a top-level
`needs` array (`Session::needs`; today `["clip_loop"]` when any clip loops, ADR 0019). The key is
absent when nothing needs it, so those files are unchanged. Builds that predate the marker ignore
it (rule 2); builds that have it compare each entry with `Session::KNOWN_NEEDS` on load and post one
status message for unknown entries (`Session::load_notes`, shown by `open_session`), then load the
file as usual. It cannot warn from builds that shipped before it existed: an older build still
drops a loop on save (known issue K40). Add an entry to `needs()` and `KNOWN_NEEDS` together when
a new feature would be lost by an older reader, and test a round-trip with and without it.

A format change is reviewed like an engine change: the refutation attack list includes "old loads,
new loads on previous build" (`../evidence/refutation-review.md`).
