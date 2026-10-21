# ADR 0007: Hosted plug-ins are suspended per plug-in by their own signals, never by a global engine switch

- **Status:** implemented in `transit` and verified 2026-10-18; **filed upstream as PR #765**
- **Date:** 2026-10-18
- **Reopen when:** upstream adopts per-plug-in suspension, or measurements show idle CPU with hosted plug-ins loaded is unacceptable on target hardware

## Context

SoundCraft does not run the mix engine while the transport is stopped unless a track is armed, input
monitoring is on, or a live note is sounding (`AudioState::fill`, the `monitoring` gate). That is a
deliberate idle-CPU saving. It has now cost three escapes in two days (escape record E3, E7, and the
"silent until preset change" interaction in E4), all with the same shape: a hosted plug-in expected
`process` calls and did not get them.

- E3: an armed instrument stayed silent because the gate needed an audio input stream.
- E7: Guitar Rig 7 and Helix Native editors on an idle audio track never finished loading; their GUIs
  initialise and sync with their DSP through `process`. Pressing Play fixed it.

Our fixes so far widen the gate (armed instruments; a hosted editor open). Each is correct for its
case and each leaves the next case to be found by a user. The recorded blind spot of the latest one:
a hosted plug-in with no editor open gets no calls while idle, so meters, tails and GUI-less state
sync stay stale.

The plug-in formats already carry the information the gate is guessing at, and SoundCraft reads none
of it (checked 2026-10-18):

| Format | What the plug-in says | SoundCraft today |
|---|---|---|
| CLAP | `clap_process_status` after every block: `CONTINUE`, `CONTINUE_IF_NOT_QUIET`, `TAIL`, `SLEEP`; `clap.tail` extension; `start_processing`/`stop_processing` | start/stop called; the status is ignored |
| VST3 | host sets `silenceFlags` per input bus; plug-in reports `getTailSamples()` (`kInfiniteTail` = keep calling me) | `silenceFlags` always 0; tail read (`Instance::tail`) but unused by the mixer |
| AU | `kAudioUnitProperty_TailTime`; `kAudioUnitRenderAction_OutputIsSilence` | not checked |

Every shipping host (Cubase, Live, Logic, REAPER, Bitwig) keeps the engine running while stopped and
suspends individual plug-ins when their own signals allow it. Cubase's "suspend VST3 plug-in
processing when no audio signals are received" is opt-in and built on exactly silence flags + tail.

## Decision

1. **The engine runs whenever the session holds a hosted (third-party) plug-in**, stopped or not.
   The global `monitoring` gate stays only for the no-hosted-plug-ins case, where it is harmless.
2. **Suspension is per plug-in, driven by the plug-in:**
   - CLAP: honor the returned status. `SLEEP` → skip `process` until an input is non-silent or an
     event arrives; `CONTINUE_IF_NOT_QUIET` → skip while all inputs are silent; `TAIL` → keep
     calling for the reported tail after the input goes silent.
   - VST3: set `silenceFlags` honestly per bus; skip `process` once inputs have been silent for the
     reported tail, except for `kInfiniteTail` and for instruments (events may arrive).
   - Built-ins: `Plugin::tail_samples()` already exists; apply the same rule.
   - Never skip a plug-in whose editor is open (keeps E7 fixed under the new rule too).
3. **Instruments are never suspended while their track is armed or monitoring**, regardless of
   silence, because their input is events, not audio.
4. The current gate widenings (`fix/instrument-armed-monitoring`, `fix/engine-runs-for-open-editors`)
   stay in `transit` as stopgaps until this lands, and are offered upstream as such.

## Measured (2026-10-18)

Guitar Rig 7 loaded on an audio track, editor open (so nothing suspended, engine running at 48 kHz /
128 samples on the Focusrite ASIO driver), transport stopped, no signal: Task Manager shows the
SoundCraft process at 0% CPU. The idle-CPU saving the global switch protected is below Task
Manager's resolution on this machine (a desktop-class CPU; a laptop may differ).

## Consequences

- Idle CPU with hosted plug-ins loaded goes up by the plug-ins' own idle cost, bounded by their
  suspension signals. This is the behavior of every other DAW; issue #405's 80% idle CPU is egui
  repaint, not the engine, and is unaffected.
- The audibility table (E3/E7 gate) changes meaning: rows become "does this plug-in get calls",
  and a new table covers suspension: (format, status/tail, input silent, editor open) → called or
  not. Both are cheap with the fake plug-ins we have; the CLAP status needs a fake that returns it.
- The `silenceFlags` change is observable by plug-ins and could change their behavior (some
  bypass internally when told the input is silent); that is the specified behavior and a win.
- Work estimate: about a day in `mix` + the two hosts, plus tests. The hosts already expose tail;
  the CLAP host needs to surface the status (one field on the instance).
- Upstream pitch: this is the ADR itself, with E3/E4/E7 as the evidence and the table as the gate.
  File after one of our fixes has merged; propose `docs/adr/` with this as the first record.

## Alternatives considered

- **Keep widening the gate** (what we did twice): each widening is correct and none is complete;
  the next plug-in finds the next hole. Rejected as the long-term shape.
- **Run everything always, no suspension**: simplest, matches hosts that offer no suspension, but
  gives away the one thing the current design actually buys. Acceptable as a first step if the
  per-plug-in logic slips.
- **Suspend by track state only** (the current design): rejected; it ignores what plug-ins say.

## Implementation, v1 (2026-10-18)

Scope decisions for the first implementation:

1. **Only hosted (third-party) effects are suspended.** Instruments are never suspended in v1. Many
   VST3 instruments report a tail of 0, so suspending one on the last note-off would cut its release.
   Their input is events, not audio.
2. **Built-in plug-ins are unchanged in v1.** They are cheap, and their tail reporting has not been
   audited.
3. **The editor-open rule is coarse in v1.** While any hosted editor is open, nothing is suspended
   (`MixEngine.no_suspend`). A per-plug-in rule would need the UI thread to pass editor state per
   slot. Deferred.
4. **The engine runs whenever the session holds a hosted plug-in**, stopped or not. This is
   `AudioState.hosted`, computed when a session snapshot arrives. Without it, suspension has nothing
   to act on.

Mechanics:

- `soundcraft_dsp::ProcessStatus` is { Continue, ContinueIfNotQuiet, Tail, Sleep }. The `Plugin`
  trait gets two methods: `process_status()` and `set_input_silent(bool)`.
- The CLAP host returns the status from its last `process` call.
- The VST3 host sets `silenceFlags` from `set_input_silent`. It derives `Tail` (finite tail) or
  `Continue` (`kInfiniteTail`) from the reported tail.
- The mixer keeps, per hosted insert slot, how long its input has been silent. It skips `process`
  by these rules:
  - `Sleep`: skip while silent.
  - `ContinueIfNotQuiet`: skip while silent.
  - `Tail`: process for `tail_samples()` after the input goes silent, then skip.
  - `Continue`: never skip.
  - A non-silent input resumes processing at once.
- Input silence means every sample of every input channel is exactly 0.0, or below about -120 dBFS.

Work packages:

| Package | Branch | What |
|---|---|---|
| WP1 | `feat/suspension-core` | dsp trait surface (`ProcessStatus`, two methods). |
| WP2 | `feat/suspension-core` | Mixer suspension per hosted slot. Suspension table test with fake plug-ins (status x tail x silence x no_suspend gives processed or not; resume on signal). |
| WP3 | `feat/suspension-hosts` | CLAP host surfaces the status. VST3 host sets silence flags and derives the status. Unit tests for the mappings. |
| WP4 | `fix/engine-runs-while-hosted` (on the `transit` stack) | Playback: `hosted` gate, `no_suspend` from the editors-open flag, audibility table updated with a render counter. |

Evidence rule for every package: tests pass, `clippy -D warnings`, and for behavior changes a
negative check (the new test fails on the branch without the change).

Implementation notes:

- The CLAP host marks silent inputs with `constant_mask`. That is slightly loose with the -120 dBFS
  threshold. Accepted for v1.
- VST3 instruments report `Tail` but are never suspended in v1. Scope item 1 above is the reason.
- The CLAP host reads `clap.tail` after each process call and re-reads it each time. An infinite or absent tail maps to `Continue`.
- `ContinueIfNotQuiet` and `Tail` consult the last output's silence, not only the input. The CLAP definition is about output, so input silence alone is the wrong signal.
- A parameter change wakes a suspended plug-in for one block (`PluginSlot.woken`), so its new value reaches the plug-in.
- Credit: all three came from the CodeRabbit review of PR #765.
