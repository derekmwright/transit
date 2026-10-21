# ADR 0009: A fresh-context refutation review runs before every push

- **Status:** accepted 2026-10-18.
- **Date:** 2026-10-18
- **Reopen when:** a month of data shows the step finds nothing that the gates and the upstream bot do not, or its cost exceeds the cost of a fix cycle.

## Context

The project owner asked why the CodeRabbit bot reviewing our PRs on upstream was catching so much that we were not, and whether we needed a reviewer. In one day it produced nine valid findings across three PRs (#752, #765, #768). Every one was semantic or on an edge path: fault paths, output-versus-input semantics, note dialects, event order at a shared offset, device stopped. None was a test or lint failure. Our pipeline was author plus gate: tests, clippy, and a negative check (the new test fails without the change).

The gates answer one question: does the fix do what its brief says? They cannot see what the brief did not say. The author works from the hypothesis that produced the change and tends to confirm it (playbook section 6). A passing gate on a wrong premise is still a wrong premise.

Trial run, the same evening, on two of our PRs:

- **#752 (ownership change, instances dropped on the UI thread).** The reviewer could not break the central claim. It did find a real regression: after a fault the command channel was never drained, so snapshots and never-running plug-in instances piled up and looked live to the UI. It also found a test gap (a drop while a block is in progress), a pre-existing hang class, stale comments, and a lead: the UI builds a new `Player` before dropping the old one, so two streams and two instance sets exist briefly.
- **#769 (ASIO: pause the output around input open).** The reviewer showed that the PR's rationale does not hold on its base, `main`. The lock cycle needs the input to come from the output's own `cpal::Device`, which only #424 does. On `main` the input path instead unloads the output's driver (use-after-free on resume), and closing the input kills the output. On top of #424 the pause narrows the race, but a callback preempted between its `playing` check and the lock can still meet `ASIOStop`. The two tests on the fix tested nothing about the deadlock. PR #769 was converted to a draft the same hour.

The second case matters most. Our own fix's rationale had gone unchallenged through a green gate and a manual check, and it was wrong on the base we would merge into.

## Decision

1. **Before every push to an upstream PR**, and before merging a behavior change into `transit`, a reviewer with no prior context on the change reviews it. Concurrency and FFI changes get the most careful review.
2. **Inputs to the reviewer:** the diff, the base it applies to, and the authority documents: the format headers (CLAP, VST3, AU), the cpal and asio-sys source at the version we build against, and the SDK docs. **Never** the author's conclusion, the PR rationale, or the hypotheses behind the change.
3. **Output asked for:** counterexamples. Each one gives the input or event sequence, the expected result and its authority, the observed code path (file and line), and a severity. The reviewer also lists what it probed and found sound, so a clean report shows its coverage.
4. **Handling findings.** Treat them as bot findings. A valid finding gets a fix and a test that fails without the fix. An invalid finding gets a one-line reason in the notes, so the next reviewer does not raise it again without knowing why it was dropped.
5. **Brief template.** The brief is written from this ADR. Its fixed parts are: the claim the change makes, in one sentence the reviewer must try to falsify; the base the change is meant for; the authority documents; and the output format in step 3. Its probe list, which every brief covers, is:
   - every path that drops an instance or a resource, and the thread each one runs on;
   - timing arguments (any "wait N buffers" or "after the callback" claim), with the timing the device actually gives;
   - poisoning: what happens to a lock or a channel after a panic on the far side;
   - lock order per backend, and whether any callback can take a lock the caller already holds;
   - reentrancy: a callback or a drop that calls back into the engine;
   - tests that can pass while the claim is false (the test does not reach the code path the claim is about).

## Consequences

- One more review step per push. The cost is paid before upstream, not in upstream rounds.
- Fewer bot round trips. The bot allows 10 reviews an hour, and pushes are batched; a fix that the bot would have found is found before upstream sees it.
- The author's own hypotheses get checked before upstream sees them.
- The step found a wrong PR of ours within an hour of its first use (#769).
- The reviewer can be wrong too. Its findings are checked the same way as the bot's: a fix and a failing-first test, or a written reason for rejection.

## Alternatives considered

- **Rely on the bot.** It runs only upstream, after the push, and is rate-limited to 10 reviews an hour. It found what it found after the fact and cost us round trips. Rejected as the only step; it stays on.
- **Have the author self-review.** It inherits the brief's blind spots: it reads the change through the same hypothesis that produced it. Rejected.
