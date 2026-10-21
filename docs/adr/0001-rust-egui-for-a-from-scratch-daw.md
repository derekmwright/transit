# ADR 0001: Rust + egui for a from-scratch DAW

- **Status:** superseded by [ADR 0002](0002-evaluate-and-contribute-to-soundcraft-first.md) (kept as the fallback plan)
- **Date:** 2026-10-17

## Context
Goal: an open-source multitrack DAW with Logic/Ableton ergonomics and a low-latency Linux story. Language
choice between Rust and C++; UI toolkit choice; where to develop first.

## Decision
Rust (memory safety on the audio thread, one toolchain on three OSes, cpal/clack/symphonia ecosystem),
egui on wgpu for the UI, Windows-first development with Linux CI from day one. MVP = playback plus a
built-in FX chain, no plugin hosting.

## Consequences
- If we pivot back, we restart from the from-scratch plan, informed by what SoundCraft got right (layered
  workspace, "everything is a command") and wrong (parameter push over plugin state, UI-thread
  assumptions on plugin teardown).
