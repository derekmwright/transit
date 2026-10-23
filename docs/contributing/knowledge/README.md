# Knowledge

What to know before touching the engine or the UI. Each file is short and points into the code and
the ADRs. A new topic gets a new file and a row here.

| File | Read it when you touch |
|---|---|
| [`layering.md`](layering.md) | Any crate or dependency. Always read. |
| [`audio-thread.md`](audio-thread.md) | Anything the audio callback runs. Always read. |
| [`clocks.md`](clocks.md) | Device timing, timestamps, MIDI timing, latency. |
| [`wasapi-modes.md`](wasapi-modes.md) | The native WASAPI backend's shared and exclusive opens. |
| [`asio-backend.md`](asio-backend.md) | The native ASIO backend: host thread, bindings, driver listing, streaming, stamps, the open-order rule. |
| [`plan-renderer.md`](plan-renderer.md) | `crates/mix`: compile, render, PDC, sinks. |
| [`routing.md`](routing.md) | Outputs, inputs, device channel maps, the route graph. |
| [`sidechain.md`](sidechain.md) | Insert keys and key taps. |
| [`midi.md`](midi.md) | Live MIDI input, step entry, note delivery. |
| [`midi-output.md`](midi-output.md) | External MIDI destinations: the output thread, send timing, note safety. |
| [`recording.md`](recording.md) | Record start, takes, input capture. |
| [`session-format.md`](session-format.md) | Anything saved in a `.transit` file. |
| [`commands.md`](commands.md) | User actions, undo, the control channel. |
| [`ui.md`](ui.md) | `crates/ui-studio`: shortcuts, icons, theme, tests. |
| [`unsafe.md`](unsafe.md) | Any `unsafe`, FFI or OS call. |

Decisions live in [`../../adr/README.md`](../../adr/README.md); the overview is
[`../../architecture.md`](../../architecture.md). Defects we already know are in
[`../../known-issues/README.md`](../../known-issues/README.md).

American English in code, comments, UI strings and docs.
