# Escape record

An escape is a defect that passed the point where it should have been caught (a test, a review, a
build). Fixing the instance is not enough; the escape yields a gate.

## Rule

For every escape, record the six items below and land the gate with the fix. If no automated gate
is possible yet, say so, give the manual evidence, and name the candidate gate. Do not close an
escape with "fixed" alone. The practice comes from the stabilization playbook:
<https://gist.github.com/derekmwright/0d5a880dddba6214883788eab7c2f0c9>.

## Record format

One file per escape under `docs/escapes/` (create the folder with the first entry), named
`E<n>-<slug>.md`:

1. **Failure.** What happened and what the user saw.
2. **Mechanism.** The actual cause, with file paths.
3. **Why checks missed it.** Which test or review should have caught it, and why it did not.
4. **Regression test.** The smallest test; confirm it fails without the fix.
5. **Gate class.** The family of defect the new gate covers (for example "the child protocol is
   tested against every misbehaving child shape"), not just the one instance. One table-driven test
   beats a new test per instance.
6. **Protocol change, owner, blind spot.** What process changes, who owns the gate, what it still
   does not cover.

## Root cause

Never patch around a defect in our own code. Keep the repro, find the root cause, and record it.
A workaround may ship only with the root-cause issue listed in `../../known-issues/README.md`.
