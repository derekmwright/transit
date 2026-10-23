# Architecture Decision Records

An ADR records one decision: the context that forced it, what was decided, the consequences, and the
alternatives that were rejected. The reasoning is the point, so read the whole record before proposing a
change to something it covers. One file per decision; newest number last.

**Status vocabulary.** `proposed` (written, not yet binding), `accepted` (binding; a date and notes on
what has been implemented may follow), `implemented` (accepted and shipped), and `superseded by ...` or
`superseded in part by ...` (kept for the history; the named ADR governs).

**Reopen-when rule.** Each ADR names the conditions that would reopen it. An ADR is not changed quietly:
if one of those conditions is met, or a decision no longer fits, raise it as an issue and the project owner
decides whether to amend the record or write a new one.

**Maintenance.** ADRs are maintained by the project owner. Contributors and agents should not add or change
one unless asked; see [CONTRIBUTING.md](../../CONTRIBUTING.md).

Spelling: American English throughout.

References such as E3 (escapes), F01 to F10 (engine audit findings) and G1 to G13 (MIDI readiness gates)
point at the project's escape record and engine audit; the relevant facts are repeated in the ADRs that use
them. Early ADRs describe the state of the repository when they were written; the SoundCraft fork was
later replaced by this repository ([ADR 0014](0014-transit-gets-its-own-repository.md)).

| # | Decision | Status |
|---|---|---|
| [0001](0001-rust-egui-for-a-from-scratch-daw.md) | Rust + egui for a from-scratch DAW | superseded by 0002 (fallback plan) |
| [0002](0002-evaluate-and-contribute-to-soundcraft-first.md) | Evaluate and contribute to SoundCraft before building | superseded in part by 0008 |
| [0003](0003-integration-branch-that-only-merges.md) | `transit` integration branch that only merges; one fix per branch | accepted |
| [0004](0004-search-open-prs-by-symptom-before-patching.md) | Search open PRs by symptom before patching | accepted |
| [0005](0005-fork-under-derekmwright.md) | Fork under derekmwright; offer fixes from the fork; `transit` stays local | superseded by 0014 |
| [0006](0006-adr-shape-for-our-decisions.md) | Run our side by the stabilization playbook; propose its footprint upstream, led by the escape record | accepted |
| [0007](0007-per-plugin-suspension-not-a-global-engine-switch.md) | Hosted plug-ins are suspended per plug-in by their own signals (CLAP status, VST3 silence flags + tail), never by a global engine switch | accepted for transit; proposed upstream |
| [0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md) | Fork SoundCraft, keep and refine its engine, build our own Logic/Ableton-style UX (one-week UI spike gates it) | accepted 2026-10-18 |
| [0009](0009-refutation-review-before-every-push.md) | A fresh-context refutation review runs before every push to an upstream PR | accepted 2026-10-18 |
| [0010](0010-typography-and-iconography.md) | Typography and iconography: Inter (UI, tabular figures frozen as "Inter Tab"), JetBrains Mono (transport and numeric readouts only), Lucide icons | accepted 2026-10-18 |
| [0011](0011-record-arm-and-input-monitor-are-runtime-state.md) | Record arm and input monitor are runtime state: never saved, off on every open, never restored by undo | accepted 2026-10-18 |
| [0012](0012-midi-device-layer.md) | MIDI device layer (midir-based `transit-midi-io`; learn, transport, clock) on a timestamped, realtime-safe engine contract, behind prerequisite gates from the engine audit | proposed 2026-10-18 |
| [0013](0013-unsafe-only-in-soundcraft-rt.md) | Unsafe lives only in `transit-rt` (safe API, per-platform modules with a compiling fallback, tested on every platform, refutation review for every change); `mix` stays unsafe-free | accepted 2026-10-18; parity retired 2026-10-21; second amendment 2026-10-21 (audio-wasapi) |
| [0014](0014-transit-gets-its-own-repository.md) | transit gets its own standalone repository (not in storytold's fork network); `derekmwright/soundcraft` stays a plain fork for upstream PRs only; move done on its trigger (an outside tester) and the project owner's go; dual MIT OR Apache-2.0 with SoundCraft credit | accepted 2026-10-19; move done 2026-10-21 (github.com/derekmwright/transit, v0.1.0-alpha.1); amendment 2026-10-21 (crates we own renamed `transit-*`) |
| [0015](0015-io-routing-abstraction.md) | I/O routing abstraction: a per-machine IoMap of named I/O paths of any width over all opened device channels (cap 32); per-session assignments by path name; any track, bus or send routes to any path or bus; every sink padded to one shared latency; sidechain key inputs as route-graph edges with dependency layering; hardware inserts with measured latency; slices 0 to (e) | accepted 2026-10-21 (review round 1 resolved) |
| [0016](0016-native-wasapi-backend.md) | Native WASAPI backend (`transit-audio-wasapi`, windows-rs) behind a platform-independent `audio-backend` trait: shared low-latency (`IAudioClient3`) default, exclusive and raw opt-in, device-position clock feeding the ADR 0012 fit, benchmark harness (loopback latency, jitter, xruns vs CPAL-shared and ASIO) before the backend, cpal kept as fallback; native backend feeds two per-stream clock domains; slices (a) to (f) | accepted 2026-10-21 (refutation review round 1 resolved); implementation after ADR 0015 slice (c) |
