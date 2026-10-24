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
- **Transport wait:** Play draws muted and pulses, with the reason as its tooltip, while a Play request is pending or a plug-in is still loading (`transport_wait()` in `transport.rs`, derived from `player_requests.play` and `Player::any_loading`). Clicks while pending are ignored; Stop cancels.
- Known issue: `audio_devices_io.rs` reads Enter directly (`key_pressed`) in the rename field; it predates the shortcut gate's scan list and should move to a table row.
- **Loop handle** (`arrange.rs`, ADR 0019): drag the header's upper right. Modifiers: Shift no snap, Alt independent copies (Repeat), Ctrl/Cmd overwrite. Without Ctrl/Cmd a longer loop and its ghosts stop at the next clip; with it the clips under the new span are trimmed or removed (shaded while dragging; an edit-locked clip still stops it) in the same `clip.loop_to` undo step (`overwrite: true`). Shift is already no snap, so overwrite is on Ctrl/Cmd.
- Strings are American English.

## Tests

`crates/ui-studio/src/*_tests.rs` and `crates/ui-studio/tests/`. A UI feature needs a test through
the real handler, not only a unit test of a helper.
