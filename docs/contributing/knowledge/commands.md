# Commands

Every user action is a command: `crates/engine/src/cmd/` (one file per area), registered with a
`cmd!` row: id, label, menu path, shortcut, params doc, enabled check, run function.

- **Execute:** `Engine::execute(id, params)` runs a command, catches an escaped panic, and pushes one
  undo snapshot (the previous `Arc<Session>`) if the document changed. One command is one undo step.
- **Merged gestures:** continuous gestures (a fader drag) use `execute_merged` so they form one step.
- **`noundo`:** commands that do not change the document (`cmd!(noundo ...)`, such as undo itself,
  or analysis) are registered with `noundo`. A document-changing command must not use it.
- **Refusals are results:** a command that cannot apply returns an error that reaches the status line
  and changes nothing.
- **Reachability:** a command is not done until the UI can reach it (menu, shortcut or control) and a
  test shows it; see `ui.md`.
- **Control channel:** the same commands are exposed to scripts and agents by `crates/automation`
  (`docs/control-protocol.md`, `docs/mcp.md`), so command params are a public contract.
- Menus come from `crates/engine/catalog/menus.txt`; a command lights up a menu item when the menu
  path and label match.

Tests: `crates/engine/tests/` (random edits, hostile params, route cycles).
