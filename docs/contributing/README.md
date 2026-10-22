# Contributing map

Short files, grouped by folder. Each folder has an index; a new topic gets a new file and an index
row, never a longer file.

| Folder | Holds |
|---|---|
| [`knowledge/`](knowledge/README.md) | How the engine and UI are built: layers, the audio-thread contract, clocks, the plan renderer, routing, MIDI, recording, session format, commands, UI, unsafe. |
| [`evidence/`](evidence/README.md) | What proves a change: the gate commands, refutation review, the escape record, how to write a test that is evidence, hardware evidence and CI. |
| [`../known-issues/`](../known-issues/README.md) | Defects we know about, each with a status and the gate or follow-up. |
| [`../adr/`](../adr/README.md) | Decisions and their reasons. Maintained by the project owner. |
| [`../principles.md`](../principles.md) | The engineering principles: what gets built first, and the three feature areas. |
| [`../building.md`](../building.md), [`../releasing.md`](../releasing.md) | Build, run, release. |
| [`../architecture.md`](../architecture.md) | One-page overview that points into `knowledge/`. |

## Read first (agents and people)

1. `AGENTS.md` at the repository root: the rules.
2. [`knowledge/README.md`](knowledge/README.md): always read `layering.md` and `audio-thread.md`;
   then the topic files for the crates you touch.
3. [`evidence/README.md`](evidence/README.md): read `gates.md` and `refutation-review.md` before
   opening a pull request.
4. [`../known-issues/README.md`](../known-issues/README.md): check that your finding is not already listed.
5. The ADR index, for any decision your change touches.

## Ground rules

- Cite file paths, not line numbers; paths age better.
- Point to code and ADRs instead of restating them.
- American English everywhere.
- Evidence beats assertion: a claim in a pull request names the test or measurement that backs it.
