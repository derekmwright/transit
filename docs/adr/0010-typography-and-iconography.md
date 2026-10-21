# ADR 0010: Typography and iconography: Inter, JetBrains Mono, Lucide

- **Status:** accepted 2026-10-18
- **Date:** 2026-10-18
- **Implementation:** fonts landed at ad67518 (Inter 4.1 and JetBrains Mono 2.304 in `apps/transit/assets/fonts`). Icons and the final polish pass landed at `cba8e8b` (icons) and `d7b25ef` on `transit`, in `crates/ui-studio/assets/icons`. Attribution is in [`ATTRIBUTION.md`](../../ATTRIBUTION.md).
- **Reopen when:** egui gains OpenType shaping, so the frozen tabular feature is no longer needed; or the project owner rejects Inter after a 30-minute session on the spike build, in which case the fallback is IBM Plex Sans and Plex Mono; or an icon is needed that Lucide does not cover well enough to justify a second set.

## Context

The spike's first build used egui's default fonts. Ubuntu for proportional text and Hack for monospace made the app read as a developer tool, not a creative one. The project owner's direction was a little character but mainly easy to read, with a simplistic feel. The audience is creative producers, not engineers.

egui renders text through `ab_glyph` and does no OpenType shaping. That has a direct consequence: OpenType features such as tabular figures (`tnum`) and stylistic sets (`ss02`) cannot be switched on at runtime. A readout that changes width as digits change (a clock, a BPM, a dB value) jitters unless its digits are already tabular in the font file. The features therefore have to be frozen into the files themselves, at build time.

## Decision

1. **Inter 4.1 (SIL OFL 1.1) for all UI text.** Medium for labels, Regular for body text.
2. **Tabular figures and `ss02` are frozen into the Inter files** with `opentype-feature-freezer`, so digits are tabular by default. The frozen family is renamed **"Inter Tab"**. Inter's OFL declares no Reserved Font Name, so the rename is permitted; the modification is recorded in `ATTRIBUTION.md`.
3. **JetBrains Mono 2.304 (SIL OFL 1.1) for numeric readouts only:** the transport clock, bar:beat, tempo, sample rate and buffer size. Use it at 16 px and up. It is never used in dense lists, meters or ruler labels, which use frozen-tabular Inter.
4. **Two families total.** No third family without a new ADR.
5. **Icons: Lucide 1.50.0 (ISC).** The SVGs are used unmodified, rendered through egui_extras' `svg` feature with resvg 0.45.1, rasterized once per icon and per pixel size, and tinted by theme tokens. Eight of the icons are Feather-derived and fall under the MIT block in the same Lucide license file. The filled star is derived from the outline at load time.
6. **Letters stay where a word reads better than a glyph:** M, S, R and I on track headers, and Pre and Post on sends.
7. **One accent color.** Dark and light themes are both built from tokens. The light accent is `#1873b8`, which gives 3:1 on light panels and 4.5:1 for white text.
8. **Every asset is recorded in `ATTRIBUTION.md`.** `cargo xtask assets` enforces it: a font or icon with no attribution row fails the gate.

## Alternatives considered

- **IBM Plex Sans and Plex Mono.** More character, and the project owner looked at it. Inter was chosen first. Plex stays the fallback if Inter does not hold up in use.
- **Source Sans 3.** Rejected as less distinct than Inter at UI sizes, with no advantage that mattered to the project owner.
- **Roboto Mono.** Rejected for the numeric readouts: it is a generic monospace, and the transport is the one place the app should show character.
- **Martian Mono.** Rejected as too stylized for the register. The project owner wants easy reading first.
- **Keep the egui defaults.** Rejected: they are the cause of the developer-tool look.

## Consequences

- **Binary size.** Fonts add about 2.8 MB to `apps/transit` (+2.79 MB). Icons add about 1.4 MB to `crates/ui-studio` (+1.37 MB, mainly resvg, usvg, tiny-skia and their dependencies).
- **Tabular readouts do not jitter.** Numbers that change width in lists and meters stay put, because the tabular figures are in the file, not toggled at runtime.
- **The transport uses the mono family.** That is the one deliberate place where the app shows its character. Everything else stays in Inter.
- **Icons come only from Lucide at the pinned version.** Any new icon must come from Lucide 1.50.0 or get its own attribution row. An icon from another set needs its own ADR.
- **Font setup still lives in `apps/transit`.** It depends on `soundcraft-ui-egui` for `fonts::definitions()`. Moving font setup into `ui-studio` and dropping that dependency is open. Until then the leak stays on the list.
- **Open follow-ups from the polish review,** which this ADR does not change: dense header and strip offsets are not yet on the 8 px grid, and dark dim text and light mute and solo fills fall short of contrast targets.

## Related

- [ADR 0008](0008-fork-soundcraft-keep-the-engine-build-our-own-ux.md): the spike's UX target and the one-week exit criteria. Fonts and icons are the assets ADR 0008 said to replace.
- [`ATTRIBUTION.md`](../../ATTRIBUTION.md): the license and modification record for every font and icon.
