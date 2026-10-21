# ADR 0002: Evaluate and contribute to SoundCraft before building

- **Status:** superseded in part by [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md) (2026-10-18): fork decided, engine kept
- **Date:** 2026-10-17

## Context
ArtCraft's SoundCraft (storytold/soundcraft) is a Rust/egui DAW under MIT OR Apache-2.0 with CLAP/VST3/AU
hosting, automation, MIDI and bounce, ten days old and moving fast. Its UX target is Pro Tools, which
the project owner dislikes. Building a competitor from zero would spend months reaching where it already is.

## Decision
Run it, find where it falls short, contribute engine fixes upstream, and decide later between
(1) contributing lightly, (2) forking with a replacement UI, (3) from-scratch using SoundCraft as a
reference, (4) becoming its delegated maintainer. Engine work is shared by every option.

## Consequences
- Every direction change is recorded in an ADR.
- The UI question is untouched by engine contributions and still points at option 2 or 3.
- Risk accepted: carrying a young, AI-generated codebase we do not yet fully understand.
