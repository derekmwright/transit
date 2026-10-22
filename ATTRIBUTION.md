# Attribution

Every asset bundled with or committed to SoundCraft is listed here with its author, source and
license. `cargo xtask assets` fails CI when a tracked asset file has no row.

**Rule:** no Avid, Pro Tools, Adobe or other proprietary icons, images, sounds or presets — ever.
Assets must be original, public domain (CC0) or permissively licensed. See `AGENTS.md`.

## Icons

All UI icons (tools, transport, track buttons) are drawn procedurally in
`crates/ui-egui/src/icons.rs` by the SoundCraft contributors (MIT OR Apache-2.0). No UI icon image
files are used by SoundCraft. The transit app's studio UI (`crates/ui-studio`) uses Lucide icons
(ISC), listed under Files below. The app icon (`assets/app-icon/`, the wordmark's first t, original artwork by
@derekmwright) is listed under Files below.

## Audio

The demo session's audio (drums, bass, pad) and its MIDI parts are synthesised in code in
`crates/engine/src/demo.rs` by the SoundCraft contributors (MIT OR Apache-2.0). No sample files
are bundled.

## Fonts

SoundCraft bundles no font files; the transit app bundles Inter and JetBrains Mono (rows under Files). SoundCraft uses the default fonts shipped inside the `egui` crate
(Ubuntu-Light: Ubuntu Font Licence 1.0; Hack: MIT; Noto Emoji: OFL-1.1; emoji-icon-font: OFL-1.1/MIT)
and, at runtime, the operating system's own UI fonts when available (never redistributed).

## Files

| Path | Author | Source | License |
|---|---|---|---|
| `assets/app-icon/transit.svg` | @derekmwright | original (the wordmark's first t on a rounded square; the app icon master) | MIT OR Apache-2.0 |
| `assets/app-icon/transit-1024.png` | @derekmwright | original, export of `transit.svg` | MIT OR Apache-2.0 |
| `assets/app-icon/transit-macos-512.png` | @derekmwright | original, derived from `transit-1024.png` (Apple-grid margin) | MIT OR Apache-2.0 |
| `assets/app-icon/transit.icns` | @derekmwright | original, derived from `transit-1024.png` | MIT OR Apache-2.0 |
| `assets/app-icon/transit.ico` | @derekmwright | original, export of `transit.svg` | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/` | @derekmwright | original, exports of `transit.svg` (16 to 512 px PNGs and the scalable SVG) | MIT OR Apache-2.0 |
| `assets/brand/transit-icon-dark.svg`, `assets/brand/transit-icon-dark-1024.png` | @derekmwright | original (pale t on charcoal; dark-surface icon variant) | MIT OR Apache-2.0 |
| `packaging/macos/dmg/` (all files) | @XusBadia | original (macOS DMG window background and layout, from the app icon; see `packaging/macos/dmg/README.md`) | MIT OR Apache-2.0 |
| `docs/images/transit-arrange.png` | @derekmwright | screenshot of transit (arrange) | MIT OR Apache-2.0 |
| `docs/images/transit-mixer.png` | @derekmwright | screenshot of transit (mixer) | MIT OR Apache-2.0 |
| `docs/images/transit-pianoroll.png` | @derekmwright | screenshot of transit (pianoroll) | MIT OR Apache-2.0 |
| `assets/brand/transit-wordmark-white.svg` | @derekmwright | transit wordmark | MIT OR Apache-2.0 |
| `assets/brand/transit-wordmark-graphite.svg` | @derekmwright | transit wordmark | MIT OR Apache-2.0 |
| `assets/brand/transit-wordmark-white-2172.png` | @derekmwright | transit wordmark | MIT OR Apache-2.0 |
| `assets/brand/transit-wordmark-graphite-2172.png` | @derekmwright | transit wordmark | MIT OR Apache-2.0 |
| `docs/images/transit-wordmark-dark.png` | @derekmwright | transit wordmark | MIT OR Apache-2.0 |
| `apps/transit/assets/fonts/inter/Inter-Regular.ttf` | The Inter Project Authors | Inter 4.1, https://github.com/rsms/inter | SIL OFL 1.1 (`apps/transit/assets/fonts/inter/OFL.txt`); modified: tabular figures (tnum) and ss02 frozen as defaults with opentype-feature-freezer; family renamed Inter Tab |
| `apps/transit/assets/fonts/inter/Inter-Medium.ttf` | The Inter Project Authors | Inter 4.1, https://github.com/rsms/inter | SIL OFL 1.1 (`apps/transit/assets/fonts/inter/OFL.txt`); modified: tabular figures (tnum) and ss02 frozen as defaults with opentype-feature-freezer; family renamed Inter Tab |
| `apps/transit/assets/fonts/inter/Inter-SemiBold.ttf` | The Inter Project Authors | Inter 4.1, https://github.com/rsms/inter | SIL OFL 1.1 (`apps/transit/assets/fonts/inter/OFL.txt`); modified: tabular figures (tnum) and ss02 frozen as defaults with opentype-feature-freezer; family renamed Inter Tab |
| `apps/transit/assets/fonts/inter/Inter-Bold.ttf` | The Inter Project Authors | Inter 4.1, https://github.com/rsms/inter | SIL OFL 1.1 (`apps/transit/assets/fonts/inter/OFL.txt`); modified: tabular figures (tnum) and ss02 frozen as defaults with opentype-feature-freezer; family renamed Inter Tab |
| `apps/transit/assets/fonts/inter/OFL.txt` | The Inter Project Authors | license text | SIL OFL 1.1 |
| `apps/transit/assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf` | The JetBrains Mono Project Authors | JetBrains Mono 2.304, https://github.com/JetBrains/JetBrainsMono | SIL OFL 1.1 (`apps/transit/assets/fonts/jetbrains-mono/OFL.txt`); unmodified |
| `apps/transit/assets/fonts/jetbrains-mono/JetBrainsMono-Medium.ttf` | The JetBrains Mono Project Authors | JetBrains Mono 2.304, https://github.com/JetBrains/JetBrainsMono | SIL OFL 1.1 (`apps/transit/assets/fonts/jetbrains-mono/OFL.txt`); unmodified |
| `apps/transit/assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf` | The JetBrains Mono Project Authors | JetBrains Mono 2.304, https://github.com/JetBrains/JetBrainsMono | SIL OFL 1.1 (`apps/transit/assets/fonts/jetbrains-mono/OFL.txt`); unmodified |
| `apps/transit/assets/fonts/jetbrains-mono/OFL.txt` | The JetBrains Mono Project Authors | license text | SIL OFL 1.1 |
| `crates/ui-studio/assets/icons/` (all `.svg` files) | Lucide Contributors | lucide-static 1.50.0, https://github.com/lucide-icons/lucide | ISC (`crates/ui-studio/assets/icons/LICENSE.txt`); unmodified (tinted and, for transport glyphs and the favorite star, filled at load time) |
| `crates/ui-studio/assets/icons/LICENSE.txt` | Lucide Contributors | license text | ISC |
