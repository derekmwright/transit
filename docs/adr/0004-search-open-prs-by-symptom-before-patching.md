# ADR 0004: Search open PRs by symptom before patching

- **Status:** accepted
- **Date:** 2026-10-18

## Context
Two of our first three bugs (instrument silent while stopped; instrument silent until a preset change)
already had open PRs with the same root cause and tests (#419, #347). We wrote a patch for the second one
before finding #347 and threw it away. The PR author (Limestudio90) answers within hours; the maintainer
does not merge.

## Decision
Before writing a fix: `gh pr list --search "<symptom words>"` and `gh issue list --search` on
storytold/soundcraft. Prefer stacking the open PR into `transit` over our own patch; add only what it
lacks, as a small branch on top of it, offered as a comment on that PR.

## Consequences
- Our stack is increasingly "Limestudio90's PRs + small fixes on top", a fork in all but name.
- Credit flows to the original author; our contributions are measurements and the missing pieces, which
  is also the fastest way to be useful to this project.
