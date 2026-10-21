# ADR 0005: Fork under derekmwright; offer fixes from the fork; keep `transit` local

- **Status:** accepted
- **Date:** 2026-10-18

## Context
We had a fix (a quit deadlock) that depends on no open PR and deserved a PR of its own.
Opening one needs a fork. Until then our fixes were offered as comments on other people's PRs.

## Decision
Fork storytold/soundcraft to github.com/derekmwright/soundcraft (remote `fork`). Branches offered
upstream are pushed there and PR'd against upstream `main`. `transit` stays local ([ADR 0003](0003-integration-branch-that-only-merges.md)).
Derek is credited as `@derekmwright` only, no `contributors/people.toml` entry.

## Consequences
- First PR: #752. Review so far by a bot (CodeRabbit), one valid finding, fixed.
- The local-only rule for `transit` ended when it moved to its own repository ([ADR 0014](0014-transit-gets-its-own-repository.md)).
- If upstream stays stalled, the fork is where a Logic/Ableton UI could start; nothing decided there yet.
