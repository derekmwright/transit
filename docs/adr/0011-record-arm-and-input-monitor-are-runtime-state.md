# ADR 0011: Record arm and input monitor are runtime state

- **Status:** accepted 2026-10-18
- **Date:** 2026-10-18
- **Implementation:** merged into `transit` at 220e2ce (2026-10-18), rebased onto 9bea6e8 and fast-forwarded. Branch `fix/arm-monitor-runtime-only` (080ad5f before the rebase). `#[serde(skip)]` on both fields (`crates/model/src/mixer.rs`); engine `keep_runtime_state` on undo and redo (`crates/engine/src/lib.rs`); `mix.record_arm` and `mix.input_monitor` are noundo commands; `same_document` exempts both fields. A refutation review found nothing blocking.
- **Reopen when:** a recording workflow needs the armed or monitored state to survive a reopen (for example, a session template that must arm tracks on load), and the user-visible cost of re-arming is judged worse than the risk of opening a live input path unasked. Any such change needs an explicit opt-in per track, not a file-level default.

## Context

SoundCraft serializes `Mixer::record_arm` and `Mixer::input_monitor` into the session file. On open, the UI detects armed and monitored tracks and opens the audio input automatically. So opening a session can start live input monitoring with no user action. With speakers and a microphone or instrument connected, that produces feedback and loud transients at the moment the user has not yet touched anything.

This is the E10 scenario ("loading an armed session"). It exposed the ASIO input-open deadlock, which is why the scenario is in the escape record. The fix for the deadlock and the fix for this behavior are separate; this ADR decides the second.

Upstream contributor SlyLaM raised the behavior on PR #769 (https://github.com/storytold/soundcraft/pull/769#issuecomment-6255702109) and is preparing a separate issue and PR for it. Credit SlyLaM for the observation.

The project owner decided on 2026-10-18 that the armed state is never serialized into the save.

## Decision

1. `record_arm` and `input_monitor` are **runtime state**. They are available for the running application session, and they are never written to the session file.
2. On every session open, both flags start as `false`, including for files that still contain the fields. Old files keep loading; the fields are read and ignored.
3. Opening a session never activates an input path by itself. No track becomes armed or monitored, and the audio input is not opened, as a side effect of loading.
4. Undo and redo must not restore a `true` value from a document snapshot. Arm and monitor changes are not undoable document edits.

## Consequences

- The "open an armed session" path changes expectation: a loaded session has no armed or monitored tracks. Its tests are updated to assert `false` after open, and a test asserts that a file containing `record_arm: true` loads with `false`.
- Recording workflows re-arm explicitly after open. This is acceptable: Logic and Ableton behave the same way.
- The E10 reproduction now starts from a disarmed session. The ASIO deadlock fix is still needed for arming during playback, and is tracked separately.
- Implementation lives in `crates/model` on branch `fix/arm-monitor-runtime-only`. Upstream, SlyLaM's PR carries the same change. Ours either converges with theirs or we cherry-pick theirs, whichever lands first. We do not offer a competing PR.
- Undo of a deleted track restores its last-seen arm state; accepted for now. A deleted track that comes back through undo keeps the arm and monitor state it had when the snapshot was taken. If a different rule is wanted (always disarmed on restore), add a line here and a test.
- Post-merge cleanups (not blocking):
  - `same_document` zips tracks by index. Harmless today; key the comparison by `TrackId`.
  - Arming still bumps the document revision, so autosave rewrites an identical file. Minor churn; runtime-only changes should not bump it.
- Upstream: offer as two commits when SlyLaM's PR appears: the model-only change (converge with theirs), then the engine undo follow-up.

## Related

- [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md): we fork SoundCraft and keep its model layer. This decision changes model behavior in that layer, so it stays additive where it can.
- Escape record E10 (loading an armed session).
- PR #769 (withdrawn draft). The thread is where the observation was made.
