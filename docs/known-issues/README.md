# Known issues

Defects we know about and have decided not to block on (the merge bar is in
[`../contributing/evidence/refutation-review.md`](../contributing/evidence/refutation-review.md)).
A new issue is a new row; give it its own file only if the row cannot hold what a contributor needs.

| Id | Issue | Status | Gate or follow-up |
|---|---|---|---|
| K1 | `crates/mix/src/compile.rs`: when a plug-in factory panics while another prepared instance's destructor is blocked, one queued action can dispatch before the fault publishes. | Open, rare | Reproduce with a blocking-destructor fixture, then order the publish before dispatch. |
| K2 | The retired playback path held a panic payload whose destructor and field destructor both panic, which aborts the process. | Likely gone with the removal of the old renderer (done); re-verify | Confirm no retained payload path remains in `crates/playback`; add a test if one does. |
| K3 | `crates/mix/src/lib.rs` `Strip::panic`: extra retained strip payloads whose destructors panic run through ordinary teardown without containment. | Open | Contain payload drops the way the first one is; extend `crates/mix/tests/strip_panics*.rs`. |
| K4 | `crates/midi-io` (midir): the WinMM callback may allocate on the first message and lock on the SysEx requeue path; a possible close-versus-SysEx-requeue deadlock was not reproduced. | Open, dependency | Replace or patch the dependency path; watch for stuck input on close. |
| K5 | `crates/vst3-host`, dependency `vst3` 0.3.0: `queryInterface` defects in the shared helper. Our newer host objects wrap it; older host objects still use it. | Open, dependency | Move the remaining host objects to our own implementation. |
| K6 | Stem bounces and freeze or commit instantiate every hosted plug-in in the session, including inactive and muted tracks. Audio is correct, but large instruments make it slow. | Mitigated | Offline compiles now skip inactive tracks (`skipping_inactive_instances`); muted tracks were not verified. |
| K7 | Input path names were cosmetic: recording and monitoring chose the channel from digits in the route name. | Closed | Shared input path resolution landed; see `contributing/knowledge/routing.md`. |
| K8 | `crates/mix/src/key.rs`: a post-fader sidechain tap stops ducking when the source is muted or faded to silence, because the tap follows the fader. | Open, documented | Use the pre-fader tap. Consider a UI hint. |
| K9 | `crates/mix/src/key.rs`: the early-key delay history restarts after a structural plan swap, causing a brief detector dropout on the keyed insert. | Open | Carry delay history across plan swaps. |
| K10 | A heavy plug-in's single create call still blocks the UI thread for its duration (SDK main-thread rule). | Open, unchanged | A dedicated plug-in-host thread (needs an ADR). |
| K11 | Closed: a MIDI take journal (`<Audio Files>/<session>_<stamp>.midijournal`) is appended during a pass and recovered at launch as `<Track>_recovered`. | Closed | Done. |
| K12 | Freeze uses a fixed 2 s tail (`TAIL_SECS` in `crates/engine/src/cmd/freeze.rs`); longer reverb tails are truncated. | Open | Measure or ask the plug-in for its tail. |
| K13 | The journal is deleted at Stop before the next autosave lands: a crash in that window loses the pass. | Open | Delete the journal after the autosave confirms. |
| K14 | Approximate record alignment on devices whose clock never validates: audio vs MIDI from one pass may differ by up to one input block. | Open | Surface `start_approximate` in the UI; native capture (ADR 0016 slice d). |
| K15 | Hosted plug-ins that re-emit a parameter value every block register automation touches (Touch/Latch write points) until a changed-value check lands. | Open | Touch only on a changed value. |
| Note | The MIDI capture ring is shared by all tracks: a heavily played unarmed instrument could crowd out armed events (reported as events lost). | Note | Revisit if reports appear; a per-armed-track filter would remove it. |

## Release-note caveats

- Published builds use WASAPI only on Windows; ASIO builds are local until licensing allows
  distribution (`../releasing.md`).
- Windows builds are unsigned and macOS builds are unsigned and not notarized.
- Creating a heavy plug-in (a large instrument) can block the UI for the duration of the create.
- Live MIDI input is WinMM-based and its timing may be flagged approximate when the device clock
  fit is unavailable (`../contributing/knowledge/clocks.md`).
- Sample rate and buffer-size selection depend on the backend and driver; report the device, driver
  and settings with any timing issue (`../contributing/evidence/hardware-and-ci.md`).
