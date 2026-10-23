# transit: instructions for agents

transit is a desktop DAW (Rust, egui) with its own engine. A crash loses someone's recording, so
that outranks feature work.

Before changing code, read:

1. [`docs/contributing/agents.md`](docs/contributing/agents.md): the workflow and the rules agents must know.
2. [`CONTRIBUTING.md`](CONTRIBUTING.md): the gate commands.
3. [ADR 0009](docs/adr/0009-refutation-review-before-every-push.md): refutation review.
4. [ADR 0013](docs/adr/0013-unsafe-only-in-soundcraft-rt.md): where `unsafe` is allowed.

Then [`docs/contributing/README.md`](docs/contributing/README.md) for the knowledge and evidence
folders, and [`docs/known-issues/README.md`](docs/known-issues/README.md) so you do not rediscover
known defects.
