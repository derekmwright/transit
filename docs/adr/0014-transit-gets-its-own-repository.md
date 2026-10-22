# ADR 0014: transit gets its own repository; the fork stays for upstream pull requests

- **Status:** accepted 2026-10-19; move done 2026-10-21 (https://github.com/derekmwright/transit, `main` = transit, tag v0.1.0-alpha.1)
- **Date:** 2026-10-19
- **Reopen when:** SoundCraft upstream and transit converge again (shared engine, as [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md) allowed), or the project owner decides to contribute transit's UI upstream.

## Context

transit is built by forking SoundCraft (`storytold/soundcraft`) per [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md). The GitHub fork `derekmwright/soundcraft` holds the project owner's upstream pull requests to `storytold/soundcraft` (#752, #763, #765, #768, #769). The `transit` branch has never been pushed; that is a standing rule ([ADR 0005](0005-fork-under-derekmwright.md)).

The engine has diverged substantially: `PreparedPlan` render plans, a bounded publication queue, the runtime-owner decision (ADR 0012, G6), and a CPAL fork pin at `derekmwright/cpal`. Pulling upstream engine changes is becoming a port rather than a merge. The product name, binary, and UI are already "transit". Keeping the product inside storytold's fork network no longer matches what the code is.

The project owner decided this on 2026-10-19.

## Decision

Two repositories.

1. **`derekmwright/soundcraft`** stays a plain GitHub fork in storytold's network. It is used only for upstream pull requests.
2. **transit** gets its own standalone repository (working name `derekmwright/transit`). It is created fresh, not with GitHub's fork button, so it is not in storytold's fork network. The `transit` branch becomes its `main` branch with full history. It gets its own issues, releases, and CI.

### Trigger for the move

The first time the project owner wants someone else to run a transit build, or wants CI running in public. Until then nothing is pushed.

The trigger fired on 2026-10-21 when an outside tester wanted a build, and the project owner gave the go. The move needed that explicit go because it is a public, effectively one-way step.

### License

- Keep upstream's license files and copyright notices.
- Credit "based on SoundCraft by storytold" in the README and in About.
- transit stays dual-licensed **MIT OR Apache-2.0**, like upstream and the Rust ecosystem (decided 2026-10-19). Apache-2.0 gives an explicit patent grant, MIT is compatible with GPLv2, and users choose.

### Naming

Keep the internal `soundcraft-*` crate names for now. Renaming about 30 crates is churn and complicates offering fixes upstream. User-facing names are transit. Revisit crate names when transit stops offering engine fixes upstream.

### Upstream policy

Engine fixes may still be offered to `storytold/soundcraft` and to dependencies (for example CPAL). Each must be a minimal, idiomatic diff in the host project's style. Our heavy test machinery and abstractions stay in transit. The lesson is RustAudio/cpal#1406, which was closed as over-engineered. Each public post needs the project owner's approval.

## Consequences

- **What moves where.** The transit code, the `transit` branch history, product issues, releases, and CI move to the new repository. `derekmwright/soundcraft` keeps only the `fix/*` branches behind open upstream PRs.
- **No fork-network link.** The link between transit and storytold is intentionally not kept. Upstream changes arrive by explicit cherry-pick or port, as they already do in practice.
- **CPAL.** The CPAL fork stays at `derekmwright/cpal` as a thin patch fork (decision 2026-10-19).
- **Branch rule.** ADR 0003 (integration branch that only merges) applies to the new `main` unchanged. The "never pushed" rule ended only with the project owner's go.
- **CI.** The matrix from ADR 0013 becomes ours to maintain in the new repository.

## Alternatives considered

- **Stay one fork, push `transit` to it.** Keeps the fork-network link, but presents a diverged product as a SoundCraft fork and mixes our issues and releases with upstream pull-request branches. Rejected.
- **Rename the crates now.** Cleaner identity, but about 30 crates of churn and harder upstream offers. Deferred; partly superseded by the 2026-10-21 amendment below.
- **MIT-only for new code.** Allowed by upstream's dual license and simpler, but loses the explicit patent grant. Rejected by the project owner on 2026-10-19.

## Related

- [ADR 0003](0003-integration-branch-that-only-merges.md): the `transit` branch rule carries over.
- [ADR 0005](0005-fork-under-derekmwright.md): the fork and the "transit stays local" rule this ADR eventually ends.
- [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md): the decision to fork; its shared-core outcome is the reopen condition here.
- [ADR 0013](0013-unsafe-only-in-soundcraft-rt.md): the CI matrix that moves with the code.

## Trigger set (2026-10-19 14:55)

The project owner set the trigger as: convert to the transit repository once MIDI is sorted. Trigger: the KeyStep milestone is done (live MIDI from a hardware port reaches a hosted instrument through the default Player with F10 timestamps), and the open review queue for that milestone is merged. The move itself still waits for the project owner's explicit go when that point is reached.

## Amendment 2026-10-21: crate names

Decision: the crates transit created are renamed from `soundcraft-*` to `transit-*` now; the inherited engine crates keep `soundcraft-*` until one planned sweep. Reason: Soundcraft is a Harman mixer brand, and a trademark complaint against upstream would force a rename, so the crates we own carry our name today. Directory names (`crates/rt`, etc.) are unchanged.

Renamed today: `transit-rt`, `transit-midi-io`, `transit-audio-backend`, `transit-audio-wasapi`, `transit-ui-studio`. `audio-bench` and the `transit` app were already unprefixed.

Sweep checklist for the inherited crates, so the rest is one mechanical job later:

- Cargo package names and `[lib]` names; every `use` path and dependency entry.
- `xtask` tables (`LAYERS`, `EXEMPT`, `WASM`, the unsafe allowlist) and the prefix handling in `layers`.
- Workflow files under `.github/workflows`.
- `docs/adr` and `docs/contributing` mentions; `ATTRIBUTION.md`.
- `packaging/` ids `ai.storyteller.soundcraft.*`, desktop and metainfo file names, and the Flatpak manifests. Done: app id `dev.derekmwright.transit`, files `transit.*`.
- [x] Packaging builds the transit app (`-p transit`, binary `transit`, `transit.app`, volume "transit"); the inherited `apps/soundcraft` is not shipped.
- The `SOUNDCRAFT_CONFIG_DIR` and scan-cache compatibility shim: keep reading the old cache dir.
- The "Based on SoundCraft by storytold" credit stays (nominative use).
- The private notes.
