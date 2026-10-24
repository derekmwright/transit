# E47: header state jobs raced the rendered document (#73)

1. **Failure.** Plug-in header state jobs (Save As, Copy, preset load, and restore after
   undo/redo; #68, shipped in build 10cfc19) started before the session's parameter values had
   reached the plug-in. A Save As right after a parameter change saved the old value, and an
   in-flight value could overwrite a just-loaded preset. The test
   `header_presets_save_load_undo_step_and_paste_through_the_plug_in_host` failed on CI Linux and
   macOS for three pushes (da073c6, 6597212, 698641d): deterministically with the inline host
   (macOS), about 1 in 7 on the host-thread path, and once on Windows under load.
2. **Mechanism.** Values reach the plug-in only when the audio side adopts and renders the next
   plan. The jobs read live state immediately. The test's `set_live` helper changed plug-in
   state behind the session's back, which also hid the product race.
3. **Why checks missed it.** The Windows gate and the WSL Linux gate passed (timing). The one
   Windows failure under load was dismissed as a flake and the chain proceeded, and nobody
   watched the CI badge.
4. **Regression tests.** `a_save_as_right_after_a_parameter_change_saves_the_changed_value`
   fails with the wait removed, on both host paths. The helpers now use real readiness
   (`plugin.state_values` plus a wait for the rendered document).
5. **Gate class.** Any UI job that reads live plug-in state after a document change must wait
   for `Player::plays(&session)` (the rendered-document confirmation), as `capture_states`
   already did. A test failure never passes a merge as a flake without a tracked issue and a
   green re-run on the failing platform. The CI conclusion is checked after every push.
6. **Protocol change, owner, blind spot.** The merge chain reports the CI run result, and the
   hourly watch checks CI first. Owner: the coordinator. Blind spots: preset load and restore
   after undo have no guard test yet (Save As only); continuous document edits (automation
   writes) can starve a job to the 10 s give-up; a bypassed insert's value is not applied, so a
   save there holds the plug-in's real value (#78 covers the undelivered-value case).
