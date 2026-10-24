# ADR 0020: The External Instrument is an instrument-slot device; its compensation is plan latency in frames

- **Status:** accepted 2026-10-24 (issue #71, slice 3)
- **Date:** 2026-10-24
- **Reopen when:** the latency an External Instrument adds to the whole mix (about 20 ms on the reference chain) gets in the way of playing other instruments live along with it, which asks for sending MIDI early instead (rejected below for v1); a driver whose loop phase changes between opens makes stored figures wrong often enough to need a measure on every open; or offline render is asked to include hardware (it cannot; see decision 9).

## Context

Playing a hardware synth today takes a MIDI track with an external output (#61) and a separate audio track for the synth's return. The two are not linked and nothing compensates the round trip, so the synth sounds late by MIDI delivery, the synth's response and the audio input: 19.5 ms at buffer 64 and 21.5 ms at 128 for a Behringer Model D on a Focusrite Scarlett 2i2 at 48 kHz (`docs/benchmarks/2026-10-24-behringer-model-d-placement-with-compensation.md`). The bench showed that one stored number per chain is enough: sending each note early by the measured median put the onsets at -0.25 ms (64) and +0.05 ms (128) from the grid, standard deviation about 0.4 ms, which is the synth's own attack scatter.

What exists:

- **MIDI out (#61, `docs/contributing/knowledge/midi-output.md`).** `Track::midi_output = External { port, channel, offset_ms }`. The audio thread stamps each clip event with the host time at which its render frame is presented (`player/audio.rs`, `external_block`: the block's first render frame through the converter phase to an output frame, then `ClockMap::frame_to_host` of the output map), and the `transit-midi-out` thread sends it at that time plus the offset. Two `rtrb` rings feed it (playback, echo); it owns the ports and every note-safety rule (Stop, wrap, destination change, device loss, exit). The stamp does not include plug-in delay compensation: a bare MIDI-out track is early by the plan's Main arrival whenever the plan has latency.
- **Hardware inserts (ADR 0015 section 5).** `Insert { hardware: Some(HardwareInsert { send, ret }) }` sends to an output path and reads the return from an input path; `latency()` is the measured round trip, which PDC counts. `HwProbe` (`playback/src/hardware_probe.rs`) measures it on the plan's own timeline: a click added at plan frame `t`, the live input the plan reads while rendering frame `t` recorded at `t`. The result is stored per machine in `IoMap.hardware_latency` (`send`, `ret`, `round_trip_frames`, `rate`, `measured_at`), and Audio Devices runs it without blocking (`ui-studio/src/hardware_measure.rs`). On the Scarlett at 64 it reads 326 frames.
- **The bench (`tools/audio-bench`, `--midi-roundtrip`, `--placement`).** A warm-up note, an onset detector (`onset.rs`: 0.25 ms envelope, threshold 12 dB over the run's lowest floor, refined to 10% of the attack peak), the median over trials, and a note guard that sends note-off and CC 123 on every exit. It times in the **capture-position clock** (`fit.rs`: a line of capture-block arrival against input sample position), while the app's MIDI thread schedules on the **output presentation map**. The two differ by the device's through path, so a number measured in one cannot be used in the other.

## Decision

1. **Model: a new device kind in the Instrument track's instrument slot; the MIDI side stays `Track::midi_output`.** `Track::instrument` holds `Insert { plugin: "external.instrument", external: Some(ExternalInstrument { ret, gain_db, manual_ms }) }`, a sibling field like `hardware`:
   - `ret`: an input path name from the machine's I/O map (mono or stereo), resolved like a hardware insert's return. The track's format follows the path's width.
   - `gain_db`: input gain on the return before the inserts.
   - `manual_ms: Option<f32>`: `Some` means the user set the latency by hand; it is clamped to 0..500 ms when read, as `offset_ms` is.
   - **Port and channel are `Track::midi_output = External { port, channel, .. }`**, unchanged: one destination model, one output thread, all of #61's note safety and "device missing" states. While the device is on the track, `offset_ms` is held at 0 and the Out button's offset field reads "Latency is set on the External Instrument", so there is one timing knob.
   - **The measured value is not in the session.** It is a fact about the synth, its MIDI port, the interface and its settings, so it is stored per machine (decision 5), as hardware insert round trips are; the session names the port and the return path only.
   - One command, `instrument.external { track, port?, channel?, ret?, gain_db?, manual_ms? }`, writes the insert and `midi_output` as one undo step. An older build sees an unknown instrument id (silent) and still sends the MIDI to the port.
2. **One reference: the plan's render timeline, in frames.** The compensation C is **the number of plan frames from the render frame at which a note-on is rendered to the render frame at which its onset enters the strip from the return**, with the note sent by the unchanged #61 stamp. It is a `latency()` in exactly the sense a hardware insert's round trip is, so PDC handles it with no new mechanism. The capture-position clock and the input map are not used for it. Notation: r the plan rate, R(S) the render frame of session sample S, P(R) the host time at which R is presented (`external_block`: output map, `frame_to_host`), I the strip's inserts' latency, A the plan's Main arrival.
   - **Send:** t_send(S) = P(R(S)) + offset, offset = 0 on this track. Not early.
   - **Return:** the onset enters the strip at R(S) + C (the definition of C; Measure takes it that way).
   - **Alignment:** the strip reports latency C + I, so PDC delays its edge to Main by A - (C + I), and the onset reaches Main at R(S) + A, where every software path's sample S arrives. Sends tap after the device and are compensated the same way.
   - **Bare MIDI-out tracks (#61) get +A:** t_send(S) = P(R(S)) + A / r, so a synth heard outside the plan stays with the delayed mix. This fixes the gap above; today A is usually 0, and an External Instrument raises it by about 20 ms.
   - **Start of playback:** nothing special. The note at the start position is sent at its presentation instant, the #61 chase restarts notes sounding across the start, and the PDC delay lines (already at A) hold the other paths back. No note can be "too late to send early".
   - **Live playing:** echo goes out at once, as in #61, and the return is monitored through the strip with no added delay; PDC delays the other paths, not the latent one. Recording on an armed External Instrument track records MIDI, as with a hosted instrument.
   - **No valid output map:** the stamp falls back to callback entry (early by the output latency). The card reads "Timing approximate: device clock not settled" and Measure refuses.
   - **Relation to the bench:** C = R_hw + (MIDI delivery + synth response - DAC delay), where R_hw is the hardware insert round trip of the same interface in the same plan frames. The bench's L minus the driver's input latency estimates the bracket (15.9 ms at 64, 16.1 at 128 on the Scarlett), which is the cross-check in decision 10.
3. **Measure runs the bench's round trip in plan frames.** The onset detector and the trial summary (`onset.rs`, `summarize`) move from `tools/audio-bench` into `soundcraft-playback` (`onset.rs`), and the bench imports them, so the app and the bench share one implementation. The recorder is `HwProbe`'s: the return as the plan reads it, at plan frames. The note-on is an `OutEvent` stamped for render frame R0, the start of a block at least one block ahead, through the playback ring and the output thread (the main thread never sends MIDI). Each trial's latency is R1 - R0, with R1 the detected onset; C is the median.
   - **Sequence.** Wait for a valid output map and 100 ms of return as the reference floor. Send one unmeasured warm-up note (200 ms). Then 8 measured notes, 1500 ms apart (the Model D's release needs the gap), each held 100 ms. Note 60, velocity 100, on the track's channel (channel 1 when "as recorded"); the note and velocity can be changed in the dialog. A run takes about 14 s.
   - **What the user sees.** The device card's Measure button opens a small sheet: "Measuring: note 3 of 8", a level meter of the return, and Cancel. At the end: "22.1 ms (1061 frames), spread 0.4 ms, 8 of 8 notes. Save | Measure again". Save stores it and the card shows "Measured 2026-10-24 at 48 kHz / 64 frames".
   - **Manual.** A "Set manually..." field in ms sets `manual_ms`; the card then reads "Manual: 20.0 ms" and the measured figure is kept but unused. Clearing the field goes back to the measured value.
   - **Note safety.** Measure uses the output thread's release path: note-off and CC 123 on the channel at every exit (done, Cancel, an error, a device reopen, closing the window, app exit), as the bench's guard does. The transport cannot start while Measure runs (`REFUSED_TRANSPORT`'s pattern).
   - **Refusals before starting** (extending `hardware_ui::measure_blocker`): no open audio device; not a duplex device; transport playing ("Stop the transport to measure."); this track armed ("Disarm the track to measure: live input would play the synth."); the port missing; the return path unresolved; another measurement running.
4. **The value follows the rate in frames; a mismatch is labeled, never silent.**
   - **Lookup, in order:**
     1. `manual_ms`, converted at the current rate: round(ms × r / 1000).
     2. A measurement whose key matches exactly (decision 5).
     3. The newest measurement for the same port and return at another rate or buffer. Another rate is rescaled by time: frames × r_new / r_old, so 1061 at 48 kHz gives 975 at 44.1 kHz. A buffer difference is used as is.
     4. Otherwise 0.
   - **Labels.** Case 3 shows amber: "Measured at 48 kHz / 64, now 44.1 kHz / 128: measure again". Case 4 shows "Not measured: notes sound late by the round trip". The buffer is part of the key because C moves with it (the bench's L moved 122 samples from 64 to 128).
   - **Device reopen.** A device reopen keeps the record. On drivers whose loop phase changes between starts (the Model 12: one buffer apart), the card adds "remeasure after a reconnect", as for hardware inserts.
5. **Storage: a separate table next to the hardware insert one.** `IoMap.external_latency: Vec<ExternalLatency { port_id, port_name, ret, rate, buffer, frames, spread_frames, notes, measured_at }>` in `ui.json`, per device as the I/O map already is. The key is (port id, else name; return path; rate; buffer). It is not merged with `hardware_latency`: the meaning differs (MIDI to return, against audio round trip), the key differs (a MIDI port and the buffer), and a hardware insert's figure must never be read as a synth's. Both share the Measure plumbing (`hardware_measure.rs` gains `MeasureKind::External`) and the recorder. `HardwareLatency` has no buffer in its key, which is a known gap of hardware inserts and out of scope here.
6. **PDC ordering.** The device's latency counts at the strip head, where a latent hosted instrument's does (`process_strip_inserts`: instrument latency first, then inserts). A missing return or port, a disabled device and an unmeasured device report 0, so PDC does not move when the hardware is absent. A frozen track keeps reporting the chain latency its render replaces (`frozen_latency`), as today.
7. **Failure modes and what the UI says.** The card and the Out button carry the state; playback never stops for it.

   | Condition | Behavior | Card text |
   |---|---|---|
   | MIDI port missing | #61: nothing sent, reconnects when the port returns (Sustain off and CC 123 first) | "MODEL D missing: notes are not sent" |
   | Return path missing or device not duplex | Strip silent, latency 0 | "Return In 1 is not on this interface" / "Needs a duplex device" |
   | Audio device closed | Strip silent | "No audio device" |
   | Measure: no onset within 1 s in fewer than half the notes (or fewer than 3 found) | Refused, nothing stored | "No sound came back on In 1 within 1 s. Check the synth's audio goes to In 1, its MIDI channel and its volume." |
   | Measure: return not quiet before a note (floor above -20 dBFS, or 6 dB over the floor after one retry) | Refused | "The return is not quiet before the note: stop the synth's sound (held note, arpeggiator, drone)." |
   | Measure: any attack peak at or above -0.5 dBFS | Refused (the bench's clipped run was discarded) | "The return clipped: lower the synth or the input gain and measure again." |
   | Measure: spread above 1.0 ms | Shown, not saved unless the user presses Keep | "Spread 1.3 ms is high (expected under 1 ms): the synth's attack or the driver is unsteady." |
   | Playback: sends more than 2 ms late (#61 counter) | Counted | "n notes sent late" in the tooltip |

   The 1.0 ms bar is 2.5 times the 0.4 ms measured on the reference chain, and it passes the widest run on record (the Model 12 at 0.95 ms).
8. **Recording and printing.** Arming records MIDI (decision 2). A print of the return (freeze, bounce in place, stems) captures the track's tap in render frames, and the frame captured at R is session sample R - (C + I). This is the latency trim offline renders already apply to latent strips, so the file is aligned and carries no offset. Raw audio plus a stored offset is rejected (see Alternatives rejected).
9. **Offline render of an External Instrument is impossible**: the sound exists only when the synth plays in real time. Render therefore runs in real time, or it leaves the device out explicitly:
   - Freeze, bounce in place and stems of a range that includes an unfrozen External Instrument play the range through the live Player at speed 1 with the transport locked. A pre-roll of C + 200 ms comes first, so chase and the first notes are in place. The capture continues with the tail rule offline freeze uses, and the dialog shows progress and Cancel. Cancel and every error take the Stop path (note-offs, CC 123).
   - A capture gap in the return (a K16 discontinuity) or an output underrun fails the render: "Dropout at 1:23.4: render again". A print with a hole in it is never written.
   - Until real-time render lands (slice 3), such a render is refused: "This range has an External Instrument (Model D), which can only render in real time. Render without it | Cancel". "Render without it" is an explicit choice and the notice names the tracks left out. A silent skip is never the default.
   - A frozen External Instrument track renders offline like any frozen track. Freezing is how a hardware part becomes portable.
10. **Tests.**
    - **Arithmetic, with a fake clock** (`mix` and `playback`): a linear output map with a chosen offset, r = 48 kHz, 64-frame blocks, C = 1061. Check that:
      - the stamp of an event at S is P(R(S)), not earlier;
      - strip latency is C + I, and the Main edge delay is A - C - I;
      - a bare #61 track's stamp is P(R(S)) + A / r;
      - the lookup chain gives manual 20.0 ms → 960 at 48 kHz and 882 at 44.1 kHz, rescaled 1061 → 975, the clamp holds at 500 ms, and a buffer mismatch is labeled;
      - a printed frame maps to R - C - I.
    - **Plan:** `mix/tests/external_instrument.rs`. A fake return that echoes a note C frames after its render frame lands on Main at the same frame as a software click at S (bit exact). A missing return reports latency 0.
    - **Fake-backend Measure:** a fake MIDI port and a fake duplex input whose "synth" renders an attack D frames after the stamped note-on, with jitter. Check that:
      - the median is D ± 1 frame;
      - a spread above 1 ms needs Keep;
      - the run is refused on a clipped attack, on no onset, and on a drone;
      - note-off and CC 123 are sent on done, on Cancel, on error, on a reopen and on drop;
      - Measure is refused while playing or armed, and the transport is refused during Measure;
      - the warm-up note is not counted;
      - the stored record has the exact key.
    - **Hardware acceptance (Derek's chain):** the Model D on its "MODEL D" USB port, channel 1, note 60 velocity 100, return on Scarlett 2i2 input 1, Focusrite USB ASIO, 48 kHz, buffers 64 and 128.
      - (a) Measure finds 8 of 8 notes with spread at most 0.6 ms (bench: 0.38 to 0.51 ms). Three Measures in a row agree within 0.5 ms (bench: 937 against 955 samples).
      - (b) At 64, C - 326 (the hardware insert round trip there) is between 14.5 and 16.5 ms. The bench estimates MIDI plus synth at 15.9 ms; C itself should come out about 1040 to 1100 frames. This is an estimate and is reported, not gated.
      - (c) After slice 3, a real-time bounce in place of 8 notes at 1500 ms spacing: the onsets against the grid have a median within ±0.5 ms (bench: -0.25 and +0.05 ms), a standard deviation of at most 0.6 ms and no note beyond 1.5 ms.
      - (d) The control: a manual value 200 frames below the measured one moves the median by about +200 frames (bench: +185.5 and +193.5).
      - (e) Unplug the Model D's USB, then the return: the states in decision 7 show, and replugging recovers.

## Slices

1. **Device and arithmetic, manual latency only** (small):
   - the `external.instrument` kind and `instrument.external`;
   - the return read as live input, gain, and latency from `manual_ms` into PDC;
   - `offset_ms` held at 0, and the +A stamp for bare #61 tracks;
   - the missing states, and the render refusal with "Render without it".
   - Tests: arithmetic, plan, model round trip. Derek can play the Model D on one track and set 22 ms by ear.
2. **Measure:**
   - the onset and summary code moved into `soundcraft-playback`, with the bench importing it;
   - the `HwProbe` recorder plus the stamped note, the sheet, `IoMap.external_latency` and the lookup labels;
   - the failure texts, the fake-backend tests, and hardware acceptance (a), (b) and (e).
3. **Real-time render:** freeze, bounce in place and stems in real time with progress, Cancel and dropout failure. Hardware acceptance (c) and (d).

## Consequences

- A synth on an External Instrument track lands on the grid in playback and in prints, with one stored number per chain, rate and buffer. That number has one meaning: plan frames, the same as a hardware insert's round trip.
- Every other path in the mix is delayed by about 20 ms while an External Instrument is active. Playing a hosted instrument live along with it feels that latency; the hardware synth itself is monitored without it. This is the stated cost and the first reopen condition.
- The #61 stamp gains the Main arrival, which changes timing for existing external MIDI tracks only when the plan has latency, toward correct.
- A session moved to another machine has no measurement there and says so; the manual value travels with the session.
- Render of a session with an unfrozen External Instrument takes real time.

## Alternatives rejected

- **Reuse the hardware insert plus `midi_output`.** A hardware insert requires a send path and claims it in the conflict checks. Its latency means an audio round trip, its table has no MIDI port or buffer in the key, and every hardware insert path would need a "no send" special case.
- **A new track kind.** An External Instrument track behaves as an Instrument track in every other respect (arm, MIDI record, editors, freeze). The difference is the slot's device, which is where the model already varies.
- **Port and channel inside the device.** That duplicates #61's destination with two sources of truth for where a track's MIDI goes.
- **Send early by C, no PDC.** Notes must be known C ahead of the render (about 20 ms, beyond the render lead #61 already refuses to exceed), which needs a look-ahead MIDI scheduler. Notes in the first C after a play start or a locate can only be late, unless every start pre-rolls. Live echo cannot be sent early at all. It also leaves the return's own through path to align separately. A hybrid (send early by what the render lead allows, PDC the rest) is the path if the reopen condition is met.
- **Compensation in the capture-position clock (the bench's) or the input map.** Neither is what the plan sums in. The input map's offset changes with loopback calibration state (`BackendEstimate` against `Measured`), so the stored number would change meaning when a calibration is added.
- **One table for hardware inserts and External Instruments.** That mixes two quantities under one key.
- **Store the measured value in the session.** It is wrong on any other machine, and the key could not catch it without machine identity.
- **Record the return raw with an offset.** Every consumer (freeze playback, stems, consolidate) would have to honor the offset, and a frozen file is aligned by contract.
- **Skip External Instruments silently in an offline render.** That writes a mix missing a part with nothing in the file to show it.

## Related

[ADR 0012](0012-midi-device-layer.md) (MIDI device layer, decision 5), [ADR 0015](0015-io-routing-abstraction.md) section 5 (hardware inserts), [ADR 0017](0017-clock-contract-annex.md) (clocks), `docs/contributing/knowledge/midi-output.md`, `clocks.md`, `plan-renderer.md` (PDC), the two 2026-10-24 Model D benchmark pages, issues #61 and #71.
