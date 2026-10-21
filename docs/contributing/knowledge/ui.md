# UI

`crates/ui-studio` is transit's UI. `crates/ui-egui` is the legacy upstream UI; do not extend it.
UI code follows the same no-panic rule (`#![deny(clippy::unwrap_used, ...)]` in `lib.rs`).

## Conventions

- **Shortcuts:** one table, `TABLE` in `crates/ui-studio/src/shortcuts.rs`. Handlers take their keys
  from the table, never from raw key reads. Tests enforce it:
  `every_handler_takes_its_keys_from_the_table` and
  `every_table_row_is_reachable_and_no_looser_row_shadows_it` in
  `crates/ui-studio/src/slice5_tests.rs`. Menus and Help > Keyboard Shortcuts render from the table.
- **Icons:** Lucide icons through `crates/ui-studio/src/icons.rs`, colored by theme token. The glyph
  guard `symbols_are_icons_not_text_glyphs` fails the build on a symbol typed as text.
- **Theme tokens:** every color comes from `TOKENS` in `crates/ui-studio/src/theme.rs` (dark and
  light sets, flat style). No literal colors in widgets.
- **Commands:** the UI calls `engine.execute` (`commands.md`); it does not edit the session
  directly.
- **Device lifetime:** `crates/ui-studio/src/audio_connection.rs` is the boundary; tests use
  `FakeConnection` to script devices without hardware (`audio_devices_tests.rs`).
- **Long work** (plug-in scan, import) runs off the UI thread. Known gap: a heavy plug-in create can
  still block it (`../../known-issues/README.md`).
- Strings are American English.

## Tests

`crates/ui-studio/src/*_tests.rs` and `crates/ui-studio/tests/`. A UI feature needs a test through
the real handler, not only a unit test of a helper.
