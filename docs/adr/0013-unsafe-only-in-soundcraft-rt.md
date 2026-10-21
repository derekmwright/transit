# ADR 0013: Unsafe lives only in `soundcraft-rt`, tested, reviewed, cross-platform

- **Status:** accepted 2026-10-18; implemented: soundcraft-rt merged into transit at 23ad6fd (2026-10-18); CI legs all four green on run 39191937448 (2026-10-18), §5 satisfied; local gate `cargo xtask rt-targets`
- **Date:** 2026-10-18
- **Parity:** the upstream-parity motivation is retired as of 2026-10-21 (see the amendment at the end); the unsafe discipline stands.
- **Reopen when:** a required OS or realtime call cannot be expressed behind a safe API in `soundcraft-rt`, or the per-platform code grows past what one reviewer can audit in a single pass. Either case needs a new ADR, not a quiet exception.

## Context

`crates/mix` forbids `unsafe` (`unsafe_code = "forbid"`). This is an upstream SoundCraft policy and we keep it. The render path is the part of the engine where unsafe would matter most, so the policy is worth more there, not less.

Two residuals from the F01a render pool (merged at `ae655ff`, see F01a of the engine audit triage) need code that `mix` cannot hold:

1. **Worker thread priority.** Pool workers run at normal priority. Under full load in a debug build a block took 15–34 ms, against 0.2 ms inline. The fix is MMCSS "Pro Audio" (`AvSetMmThreadCharacteristics`, with `SetThreadPriority`) on Windows, SCHED_FIFO or rtkit on Linux, and the time-constraint policy on macOS. All of them are OS calls, so they are `unsafe` at the FFI boundary.
2. **A preallocated lock-free fan-out** to replace Rayon's job queue. Rayon allocates about 1.5 KB every ~63 dispatches on any pool, and that is a documented G1 exception above the 4-strip threshold. Removing it needs raw shared buffers and a hand-rolled dispatch protocol, which is `unsafe`.

The plug-in host crates (`clap-host`, `vst3-host`) already contain FFI `unsafe`. That code is outside this decision and stays as it is.

The project owner requires that this code be heavily tested and its changes scrutinized, and that it stay solid and platform independent so cross-platform compatibility is maintained.

## Decision

1. **All new `unsafe` in our tree lives in one crate, `soundcraft-rt`.** It sits below `mix` in the layer order, and is registered in the `xtask` `LAYERS` table so the layer check enforces the placement.
2. **Safe public API.** The crate exposes safe functions only. The first two are `promote_audio_thread()` and `demote()`. The lock-free fan-out is added later under the same rule. Every `unsafe fn` and every `unsafe` block carries a `# Safety` section (for functions) or a `// SAFETY:` comment (for blocks) that states the invariant it relies on.
3. **One API, per-platform modules.** Platform code sits in separate modules (`windows`, `linux`, `macos`) behind one API. A **fallback module** compiles on every target. On an unsupported platform, the safe API returns the documented "not promoted" result and logs once. It never fails the build and never panics.
4. **Crate attributes:** `#![forbid(unsafe_op_in_unsafe_fn)]`, `#![deny(missing_docs)]`, and clippy `pedantic` clean.
5. **Tests on every platform.** Windows is covered now. Linux and macOS are added to the CI matrix before the crate is used in a merged change. A test checks that the fallback path compiles and returns the documented result, and it runs on every platform, not only the unsupported ones.
6. **Refutation review for every change to the crate.** Each change to `soundcraft-rt` requires an ADR 0009 refutation review, and the review report quotes each `unsafe` block in full. A change without that report does not merge.
7. **Nothing else adds `unsafe`.** No other crate may add `unsafe` without a new ADR. `mix` stays `unsafe`-free and calls only the safe API, and only from the pool's worker start handler.
8. **Gate class.** The escape record gets a new gate class: **"unsafe change without refutation review"**. An escape of that class is a process defect: the review gate for `soundcraft-rt` was skipped or not quoted.
9. **Promotion rule (learned in review 1, 2026-10-18).** No OS registration inside a timed audio callback. Promotion happens on thread start (pool workers) or in the audio backend before the first data callback (device thread). A render-time hook was removed from `mix` before merge for this reason.

## Consequences

- **F01 priority follow-up can proceed.** The worker start handler can call `promote_audio_thread()`, once the crate and its tests land. The priority change still needs a release-build load measurement before it is claimed as a signoff default (F01 stays active until then).
- **Cross-platform builds stay green.** The fallback path lets Linux and macOS compile and run the same tests without the platform code. Platform code is exercised in CI on each target.
- **Upstream can take the crate as is.** It is small, self-contained, and separate from `mix`, so the upstream `mix` policy does not change.
- **Review cost.** Every `soundcraft-rt` change costs a refutation review. We accept that cost: the crate should change rarely, and each change should be small.
- **Scope limit.** This ADR does not approve any `unsafe` outside `soundcraft-rt`, in `mix`, or in the plug-in hosts beyond what already exists.

## Review 1 (2026-10-18)

Refutation review (ADR 0009). **20 `unsafe` blocks verified** (`windows.rs` 9, `linux.rs` 4, `macos.rs` 7), each with a SAFETY verdict of OK. **Two platform lints and the CI legs are required before merge:** Linux `clippy::doc_markdown` on `PREEMPT_RT` (`linux.rs:16`), and the macOS `#[allow(deprecated)]` placement (`macos.rs:47`, `:52`, `:55`). The Linux and macOS CI legs of §5 must be in place before the crate is used in a merged change. **A local cross-target clippy gate is adopted:** `cargo clippy --target` for the three std targets, before push. **Serial default decision referenced:** the audio thread renders serially by default, with the pool opt-in via `SOUNDCRAFT_RENDER_POOL=1`; see F01a of the engine audit triage.

**Review 1 fixes landed** (3245687, f98605b, b444d36). The cross-target clippy gate is `cargo xtask rt-targets` (part of `ci`), and the CI workflow is `.github/workflows/rt.yml`. First CI run: https://github.com/derekmwright/soundcraft/actions/runs/39186770410. The merge waits for a green matrix.

Clarification (2026-10-18): the rule governs unsafe in our own crates; third-party crates with internal unsafe (cpal, rtrb) are allowed when pinned to an exact version, covered by our own hermetic contract tests, and refutation-reviewed on every version bump.

CI: Intel macOS leg runs on macos-15-intel (macos-13 retired). CI legs: all four green on run https://github.com/derekmwright/soundcraft/actions/runs/39191937448 (2026-10-18); §5 satisfied.

## Related

- [ADR 0007](0007-per-plugin-suspension-not-a-global-engine-switch.md): plug-in host threading and suspension; the host crates' FFI `unsafe` is unchanged by this ADR.
- [ADR 0009](0009-refutation-review-before-every-push.md): the refutation review that every `soundcraft-rt` change must pass, with each unsafe block quoted.
- [ADR 0012](0012-midi-device-layer.md): the realtime-safe engine contract and gate G1 that the render pool serves.
- F01a of the engine audit triage: the residuals this decision unblocks.

## Amendment (2026-10-19 12:49): plug-in SDK interface implementations in the host crates

The project owner approved this after the Ozone 11 crash (fix/ozone-crash): our VST3 host returned null from IHostApplication::createInstance for IMessage, and Ozone dereferenced it. The fix implements the SDK's IMessage and IAttributeList host objects, which must be COM-style objects with raw vtables inside `vst3-host`; they cannot live in `soundcraft-rt`.

Rule: new `unsafe` is allowed in `clap-host` and `vst3-host` **only** where it implements or calls the plug-in SDK's own interfaces (host objects, vtables, FFI calls into plug-ins). Each such change gets an ADR 0009 refutation review whose report quotes every new `unsafe` block with its `// SAFETY:` comment and checks reference counting, interface queries, ABI and SDK semantics against the SDK's reference host. Every other new `unsafe` still goes in `soundcraft-rt`. `mix` keeps `unsafe_code = "forbid"`.

## Amendment 2026-10-21: parity retired, discipline kept

The project owner decided on 2026-10-21 that ADR 0013 was meant to maintain SoundCraft parity as far as practical and respect its decisions, and that decisions can now be made on transit's merits: SoundCraft gave transit a great starting point and got it moving.

Effect: the upstream-parity motivation is retired. Engine, device and crate decisions are made on transit's merits (for example a native WASAPI backend, ADR 0016). The unsafe discipline stays because it is good engineering: unsafe only in its own crate, quoted-unsafe refutation review, tests, safe fallbacks. Upstream PRs happen only when we choose to offer a fix, and stay minimal. SoundCraft credit and licenses stay.

## Amendment 2026-10-21 (second): soundcraft-audio-wasapi

Accepted with ADR 0016.

- The Windows-only crate `crates/audio-wasapi` (`soundcraft-audio-wasapi`) may contain `unsafe` for COM and WASAPI interface code, through the `windows` crate only.
- Every `unsafe` block carries a `// SAFETY:` comment quoting the COM or WASAPI rule it relies on, and the crate gets a quoted-unsafe refutation review before merge (ADR 0009).
- All COM objects live on one dedicated MTA thread per stream and never cross threads unmarshaled.
- Any failure falls back to the cpal backend.
- The crate compiles to empty on other platforms (wholly under `#![cfg(windows)]`).
- It is registered in xtask `LAYERS`, and the `unsafe` allowlist check lists exactly `crates/rt`, `crates/clap-host`, `crates/vst3-host` (SDK-interface code) and `crates/audio-wasapi`.
