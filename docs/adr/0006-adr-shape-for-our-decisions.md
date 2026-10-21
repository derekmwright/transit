# ADR 0006: Run our side by the stabilization playbook; propose its minimum footprint upstream, led by evidence

- **Status:** accepted (supersedes the narrower "record decisions as ADRs" version of this record, same day)
- **Date:** 2026-10-18

## Context
The project owner has run two large fully agent-coded repos (wadjet, glyphengine) and wrote the playbook that came
out of it: https://gist.github.com/derekmwright/0d5a880dddba6214883788eab7c2f0c9 (adapted from
`wadjet/docs/design/agentic-development-playbook.md`, 2026-08-29). Its governing rule: *every escaped
defect produces a gate that would have caught it, and the gate's class becomes part of the development
protocol.* Its appendix shows the class generalization paying off (one defect, then a type-by-consumer matrix,
then eleven more defects found) and, as honestly, where gates decayed.

SoundCraft runs on a single `AGENTS.md`, which carries rules (layers, never-crash) but not decisions,
contracts or escapes. In one week we hit six escapes there (recorded in the escape record, a defect log);
upstream has regression tests for three, gate classes for none, and the one thread-ownership rule that
mattered most lives in a code comment that admits it is violated. The project owner's conclusion: a single
`AGENTS.md` can only do so much and does not scale.

## Decision
1. **Our repo follows the playbook's minimum footprint**, using what already exists rather than new
   conventions: ADRs (decisions, with reopening conditions), an escape record (defect record in the
   six-item shape), integration rules (`transit` only merges, validated as a combined tree), and a baseline
   of what is true on the reference machine. Each fix we offer upstream states its evidence and, when it
   has no gate, says so.
2. **Each ADR here states what would reopen it** (added below for 0001–0005 as a one-liner each).
3. **Upstream proposal, when one of our fixes has merged:** not "please add ADRs" but the governing rule
   with this week's escape record as evidence, and the footprint list as the ask. Lead with E5 (a
   documented, unenforced thread rule that hung the app) because the gate class for it is cheap: a fake
   plugin recording the thread of every lifecycle call, asserting teardown never runs on the audio thread.
   Offer the escape record as a PR into `docs/`, so the maintainer gets evidence, not advice.

## Consequences
- Our fixes E2 and E3 are on record as gate-less. Closing that is work we owe before asking upstream to
  adopt the rule.
- Authorities for a DAW are weak compared with Wadjet's SQL oracles. The usable ones: Steinberg's VST3
  SDK validator and hosting rules (plug-in lifecycle), cpal's documented stream semantics (device I/O),
  and maintainer-approved acceptance scenarios for product behavior ("a stopped transport still passes
  live MIDI", with the three incumbents as the reference). None is written down upstream.
- Reopening conditions, retrofitted: 0001 reopens if the fork/UI decision goes to from-scratch; 0002
  reopens at the mid-November review or if upstream merges stop entirely; 0003 reopens if `transit`
  cannot be validated as a combined tree (E6); 0004 reopens if Limestudio90 stops contributing; 0005
  reopens if upstream grants us merge rights or if we commit to a separate UI.
