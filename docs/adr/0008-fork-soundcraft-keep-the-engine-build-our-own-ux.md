# ADR 0008: Fork SoundCraft, keep and refine its engine, build our own Logic/Ableton-style UX

- **Status:** accepted 2026-10-18. Supersedes the undecided part of [ADR 0002](0002-evaluate-and-contribute-to-soundcraft-first.md); its mid-November review is resolved early.
- **Date:** 2026-10-18
- **Reopen when:** the one-week UI spike (below) shows the model layer fights the target UX. Two concrete signals:
  1. Insert slots are fixed at 10, or Pro Tools routing assumptions leak into the UI and cannot be removed without changing the model.
  2. An Ableton-style clip-launch view needs model changes larger than the spike itself.

  Either signal reopens this ADR toward from-scratch ([ADR 0001](0001-rust-egui-for-a-from-scratch-daw.md)'s plan).

## Context

Two days of working with SoundCraft have given us the evidence for this decision.

- **The engine is real.** It has VST3 and CLAP hosting, ASIO, recording, MIDI editing, and automation. Its model is UI-independent: the control protocol drives the same model without the UI.
- **We found ten escapes in two days.** E1 to E10 are in the escape record. All of them are in core paths. We found them by using the app for an hour each, not by reading code.
- **Upstream moves slowly.** One person merges. There were 3 merges in 4 days, with more than 200 open PRs. Audit finding #441 was closed as abandoned on 2026-10-18. Review comes from a bot and one contributor.
- **The UI is Pro Tools by charter.** The project keeps a parity catalog of menu items, and a test enforces it. The floor of that test may only rise.

The project owner's read of the target choice: Pro Tools was picked because a menu catalog can be measured by agents. Users were not asking for it. The growing part of the market that people enjoy using is Ableton, Logic, FL Studio, and Bitwig. The gate measured the wrong thing. That is why menu parity went up while the engine stayed shallow.

The project owner decided (2026-10-18) to take the engine and continue to refine it, while polishing the UX the way we want it. That lets work bootstrap without days spent rewriting a core engine that already carries major tweaks of ours.

**Audience.** The audience is creative and bedroom producers, on free and open-source software. The project owner reaffirmed this on 2026-10-18: Pro Tools was the wrong target; the project should aim at creatives, giving the masses free software to express themselves. Professionals will keep buying professional software and services, so breaking from them and targeting creative users is the way to go.

## Decision

1. The fork `derekmwright/soundcraft` becomes our product repo. The product name was to be chosen later; `transitDAW` was the working name (now **transit**).
2. Keep the engine layers: time, audio-io, dsp, model, mix, engine, playback, clap-host, vst3-host, and au-host. Keep refining them under the stabilization playbook.
3. Replace the UI crate with our own. Build it Logic-style first: arrangement, mixer, and piano roll. Track stacks and cycle region come early. Ableton ideas come later, and only where the model allows them.
4. Drop the Pro Tools parity catalog and its floor test from the fork. The authorities for product behavior become the project owner's acceptance scenarios, with the three incumbents (Logic, Ableton, FL Studio) as reference.
5. Upstream becomes a source of engine fixes to cherry-pick, and a place to offer ours. PRs #752, #763, and #765 stay open. We keep offering engine fixes and stop offering UI work.
### The spike (one week, bounded)

Build a new UI crate against the engine's command API. Everything is a command, and the control protocol is the proof that the model is UI-agnostic.

Deliverables:
- One arrangement window with tracks and regions.
- One mixer with inserts, sends, and routing.
- One piano roll.
- Transport.
- Logic ergonomics. No Pro Tools catalog.

Exit criteria:
- (a) It drives the engine without touching `soundcraft-model` beyond additive fields.
- (b) Jup-8000 V and Guitar Rig 7 work end to end, as they do today.
- (c) The project owner prefers it to the current UI for a 30-minute session.

Failing (a) reopens this ADR toward from-scratch.

## Toolkit (decided 2026-10-18)

The new UI stays on egui/eframe. After two days the project owner found the GUI engine itself responsive and
crisp; the objections are to fonts and graphic assets, which are ours to replace in the spike. Staying
keeps the engine's assumptions (plug-in editor hosting, the control protocol's screenshots and
clicks) working untouched. Reopen only if the spike hits an egui limit (e.g. custom text rendering or
GPU waveform drawing) that costs more than a toolkit change would.

## Design direction for the UX (2026-10-18)

Target audience: creative / bedroom producers (the project owner liked Cubase, uses Ableton because it is what the
digital-musician masses use, and found Cubasis on iPad a great value). Direction: **Cubase bones, Ableton
manners.** Cubase and Logic are arrangement DAWs (project window of tracks and regions, mixer with channel
strips, key editor, always-on engine), the same shape as SoundCraft's model, which is why criterion (a)
of the spike is plausible. Ableton's session view (clips, scenes, launching) is a model change and is
out of the spike. Ableton's UI-only habits are in: the browser as the way plug-ins and samples enter the
project, a device-chain view under the arrangement instead of insert slots A-J, everything audible with
the transport stopped (the engine does this now), Ableton keyboard muscle memory where it does not
conflict with Cubase/Logic conventions.

Scope yardstick: **Cubasis.** A Cubase-shaped DAW with a tenth of the features that still feels complete:
arrangement, mixer, key editor, browser, a few good built-in instruments and effects, nothing that needs a
manual. First milestone = a bedroom producer can do in the fork what they do in Cubasis.

## Engine strategy: shared core if upstream merges, fork if not (2026-10-18)

The project owner's framing: we take the creative / bedroom-producer users; SoundCraft keeps the Pro Tools-style
tracking, scoring and post users; ideally one engine under both. That is technically easy (the engine
is a set of workspace crates with a clean layer boundary) and fits the founder's suite-of-apps model,
but it needs a maintainer who merges, and the evidence so far (3 merges in 4 days, our tested fixes
unreviewed by a human, audit #441 abandoned) says no. So the fork is structured for either outcome:

- Keep the engine crates' names, paths and layer rules exactly as upstream has them.
- Keep our engine changes as small commits off `main` (as now) so they cherry-pick in both directions.
- Add our UI crate and app beside `ui-egui`/`apps/soundcraft` rather than replacing them in place, so
  upstream merges still compile.
- Cherry-pick engine commits from upstream `main` on a schedule; their velocity is useful in the engine.
- Decide shared-vs-fork in about a month, measured by whether #752, #763, #765 (and the two PRs filed
  2026-10-18) merge, not by a decision made now.
- The two-front-ends proposal to the founder is drafted only when the project owner asks; it costs one message and
  loses nothing if ignored.

## Consequences

- We own a 10-day-old, AI-written engine and its bugs. We no longer get them fixed for free.
- Merge cost against upstream drops to cherry-picks.
- The escape record and the gates continue as they are.
- Features are prioritised by what a creative producer needs to finish a track, not by the Pro Tools parity catalog, which is dropped.

## Alternatives considered

- **Keep contributing (ADR 0002 option 1).** Bounded by a merge queue that barely moves, and the UI never changes. Rejected.
- **From scratch (ADR 0001).** Months to reach plug-in hosting, ASIO, and recording. We would re-find E1 to E10 ourselves. The host crates we would borrow are the ones the fork keeps anyway. Rejected for now. It is the fallback if the spike fails.

2026-10-21: the shared-engine leaning is retired (see ADR 0013 amendment); transit's engine diverges on its own merits.
