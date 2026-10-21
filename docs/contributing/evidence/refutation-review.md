# Refutation review

Decision: [ADR 0009](../../adr/0009-refutation-review-before-every-push.md).

Gates answer "does the change do what its brief says?" They cannot see what the brief did not say.
A second reviewer, with fresh context, tries to break the change before it merges.

## What the reviewer gets

- The diff and the base it applies to.
- The authority documents: plug-in format headers (CLAP, VST3, AU), the cpal source at the pinned
  version, OS API docs.
- Not the author's conclusion or hypotheses. Those bias the review.

## Attack list

The brief states the claim to falsify in one sentence, then covers at least:

- Every path that drops an instance or resource, and the thread it runs on.
- Timing claims ("after N buffers", "after the callback") against the timing the device gives.
- Poisoning: a lock or channel after a panic on the far side.
- Lock order, and callbacks that take a lock the caller holds.
- Reentrancy: a callback or drop that calls back into the engine.
- Tests that can pass while the claim is false (they never reach the path in question).
- Format compatibility: old sessions load; new files load on the previous build.

## Verdict shape

Per finding: severity, `file:line`, the input or event sequence (a repro), the expected result and
its authority, the observed result. Also list what was probed and found sound, so a clean report
shows its coverage. Valid findings get a fix and a test that fails without it; rejected findings
get a one-line reason so nobody raises them again unknowingly.

## Merge bar

Only critical findings block a merge: stuck notes or sound, lost takes or data, timing errors,
audio-thread allocation or locks, crashes in normal use. Everything else becomes a known issue
(`../../known-issues/README.md`) with a follow-up. Exotic edge cases are known issues, not blockers.

Changes to `crates/rt` always get a review that quotes each `unsafe` block in full.
