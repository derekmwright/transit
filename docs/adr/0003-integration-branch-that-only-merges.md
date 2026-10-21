# ADR 0003: An integration branch that only merges; one fix per branch off its narrowest base

- **Status:** accepted
- **Date:** 2026-10-17

## Context
Upstream merges are slow (founder merges alone; 208 open PRs vs 193 merged on 2026-10-17). We need a
runnable build that carries several open upstream PRs plus our own fixes, while keeping each fix
offerable upstream on its own.

## Decision
In the SoundCraft fork: `main` tracks upstream; each upstream PR we want is pinned as `pr-NNN`; each fix of
ours is one branch off the narrowest base it needs (`main`, or the `pr-NNN` it depends on), one commit
where possible, message = cause + measurement. `transit` = `main` + those branches, rebuilt by merging
only; never commit to it directly. The release exe we run is always a `transit` build.

## Consequences
- A fix can be offered upstream as a comment (when it depends on an open PR) or a PR (when it is off
  `main`) without untangling anything.
- When upstream moves: fetch, merge `origin/main` into `transit`; rebase a `fix/*` only when offering it.
- `transit` was never pushed publicly while it lived in the fork: it was a merge of other people's open work and would have read as a release. [ADR 0014](0014-transit-gets-its-own-repository.md) later moved it to its own repository.
