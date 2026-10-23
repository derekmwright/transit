# Engineering principles

These principles decide what gets built first; decisions and their reasons are in the [ADRs](adr/README.md), and defects we know about are in the [known issues](known-issues/README.md).

## 1. Engineering mantras

transit is an audio engine first and a DAW interface second. We are not pursuing feature parity with Pro Tools, Cubase, or Ableton at the expense of the underlying architecture.

**Core principles:**

1. **Performance is continuously measured, validated, and protected, never an afterthought.**
   - Establish measurable baselines.
   - Maintain performance regression gates.
   - Benchmark changes under realistic audio workloads.

2. **Stability and audio correctness take precedence over feature and menu parity.**
   - Features must be backed by complete engine semantics.
   - A visible UI capability must not conceal an incomplete implementation.
   - Preserve correctness across recording, playback, monitoring, and offline rendering.

3. **The real-time audio callback must never block, allocate unexpectedly, or perform I/O.**
   - Prepare processing resources outside the callback.
   - Use bounded, predictable operations.
   - Preserve deterministic state transitions and safe resource ownership.

4. **Recordings must preserve timeline alignment, even when input discontinuities occur.**
   - Never silently hide lost audio behind zero-filled samples.
   - Preserve recording length where appropriate, but report dropout locations, durations, and confidence.
   - Make compromised takes identifiable.

5. **Routing changes must preserve signal correctness.**
   - Detect and reject cycles where feedback is unsupported.
   - Maintain latency alignment across parallel paths.
   - Never silently discard a requested route or substitute a different signal.
   - Preserve real-time safety during graph changes.

6. **Evidence precedes implementation claims.**
   - Distinguish implemented, tested, benchmarked, and hardware-validated capabilities.
   - Keep observed evidence separate from inference.
   - Do not declare performance improvements without reproducible measurements.

**Guiding statement:**

> Build the engine capabilities first. Expose them through the UI only when their correctness, stability, and performance are demonstrable.

## 2. Feature area: professional audio backend and hardware I/O

### Objective

Provide professional-quality, low-latency, multichannel audio without requiring ASIO whenever native operating-system audio is capable of meeting the requirements.

ASIO remains fully supported, but it should not be an architectural dependency.

### Existing capabilities to verify

- Native event-driven WASAPI backend using `IAudioClient3`.
- CPAL and optional ASIO integration.
- Input/output paths named and configured per machine.
- Multichannel hardware output routing independent of session main output width.
- Paired native WASAPI duplex input/output.
- `IAudioClock2` timing information.
- Loopback-based round-trip latency calibration.
- `audio-bench` performance instrumentation.
- Latency compensation across hardware sends and outputs.

### Tracking priorities

- Benchmark native WASAPI against ASIO on the Scarlett 2i2 and TASCAM Model 12.
- Measure actual round-trip latency, callback jitter, CPU overhead, dropouts, and hardware channel exposure.
- Evaluate event-driven exclusive-mode WASAPI as a separate experimental capability.
- Ensure device negotiation reports actual sample rates, buffer periods, channel counts, and conversion modes.
- Invalidate or revalidate calibration when hardware configuration changes.
- Record dropout metadata instead of silently presenting zero-filled recordings as intact audio.

### Acceptance expectations

Hardware I/O must be discoverable, correctly routed, measurable, and reliable. Performance claims require results from actual hardware.

## 3. Feature area: unified audio routing and execution graph

### Objective

Make arbitrary supported signal paths a first-class engine capability rather than relying on fixed track-processing phases.

This enables flexible routing comparable to REAPER while maintaining an approachable Cubase-style workflow.

### Existing capabilities to verify

- Explicit routing graph.
- Compiled render plans.
- Dependency-aware scheduling.
- Track and bus routing.
- Pre/post-fader sends.
- Plugin sidechains.
- Hardware input and output paths.
- Latency compensation.
- Deterministic cycle detection.
- Real-time-safe plan publication and adoption.

### Known architectural limitation

The current compiler separates ordinary tracks from auxes/folders during execution. Some otherwise valid sidechain dependencies, such as a bus feeding a compressor sidechain on an ordinary track, cannot execute because the source is scheduled after the destination. This is tracked in [issue #1](https://github.com/transit-audio/transit/issues/1).

### Tracking priorities

- Evolve scheduling toward unified dependency ordering.
- Permit valid bus/aux-to-track sidechains.
- Retain deterministic cycle handling.
- Preserve latency alignment through mixed routing paths.
- Validate routing behavior with signal-level regression tests.
- Preserve existing compiled-plan lifecycle and real-time guarantees.

### Acceptance expectations

Every accepted routing configuration must execute as specified. Unsupported routes must fail explicitly rather than silently changing behavior.

## 4. Feature area: native hardware control surfaces

### Objective

Support deeply integrated hardware controllers through optional, device-specific native integrations rather than limiting them to generic MIDI mappings.

Initial reference device: **Native Instruments Maschine Mikro MK2**.

### Ownership requirements

- transit must never claim a device merely because it is connected.
- Native communication starts only when the user explicitly selects the device as a transit control surface.
- When not selected, transit must leave the hardware available to other applications, including Maschine VST3.
- Disabling the control surface releases transit's native device handles.
- Device handoff must be tested against NI services and the Maschine plugin on Windows.

### Proposed capabilities

- Native USB/HID communication.
- Velocity-sensitive pad events.
- Pad colors and LED feedback.
- Display integration.
- Encoder and transport mappings.
- Drum-rack and sampler integration.
- Context-sensitive control of selected devices and plugins.
- Generic MIDI fallback for unsupported controllers.

### Architecture

Implement a device-independent `ControlSurface` API with optional native drivers.

Keep hardware-control integration separate from the audio graph, MIDI tracks, and VST3/CLAP hosting.

### Acceptance expectations

Controllers must be opt-in, safely claimable and releasable, and must not interfere with normal third-party plugin operation when unselected.

## 5. Product direction

transit combines three influences:

- **Cubase:** traditional multitrack recording, editing, mixing, and arrangement.
- **Ableton:** intuitive device chains at the bottom of the interface, supporting plugin addition, grouping, and reordering.
- **REAPER:** flexible audio routing without unnecessarily restrictive track types.

Traditional timeline-based production remains the primary workflow. Ableton-style session launching is not a current objective.

Device chains should support grouping and reordering without requiring parallel effect racks in the initial implementation.
