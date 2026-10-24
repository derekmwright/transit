# Hosted plug-in windows and presets

Issue #68. Read with [ADR 0018](../../adr/0018-plugin-host-thread.md) (the plug-in host thread).

## The editor window

- On Windows both format hosts open editors in `transit_rt::editor_window`: a top-level window
  on the plug-in host thread with a header strip (GDI, no egui: that thread has no egui context)
  above a content container the plug-in parents its view in. CLAP editors open embedded there when
  the plug-in offers it, so the strip shows; floating CLAP editors and macOS windows have none.
- The client area is the plug-in's reported size plus the strip, on open and on every resize
  request (`resize_content`). `EditorWindow::sync`, run from the editor's `idle`, follows the
  plug-in's child window when it settles at another size (`Follow`).
- The strip never takes focus; activating the window gives focus back to the plug-in.

## Presets

- A user preset is the plug-in's full state, the same state a session stores. Files:
  `.vstpreset` (Steinberg's container, `soundcraft_vst3_host::preset`) and `.clappreset`
  (transit's documented container, `soundcraft_clap_host::preset`).
- Folder: `<settings>/presets/<plug-in id made file-safe>/` (`soundcraft_engine::presets`); file
  access goes through `soundcraft_engine::io::presets` (checked roots).
- State reads and live loads are `StateJob`s on the host thread, polled once per frame
  (`PluginStateReader::begin_save`/`begin_load`); the UI side is `ui-studio/src/plugin_header.rs`.
- Each job (and each restore after an undo or redo) starts only once the audio side plays the
  current document (`Player::plays`: its plan confirmed and rendered), so the plug-in has every
  parameter value the session stores. Earlier, a read missed a value still on its way and a value
  that arrived after a load overwrote the loaded state (issue #73). Tests that change a live state
  directly also store the values the plug-in reports (`plugin.state_values`), or the session sends
  its older values over it.
- Commands: `plugin.preset_save` (not undoable), `plugin.preset_load` and `plugin.state_load` (one
  undo step each, recorded after the plug-in took the state), `plugin.preset_list`,
  `plugin.preset_delete`. Undoing or redoing a load asks the UI to load the stored state live
  (`Engine::take_state_restores`).
- Factory presets (VST3 program lists, CLAP preset discovery) are not listed yet.
