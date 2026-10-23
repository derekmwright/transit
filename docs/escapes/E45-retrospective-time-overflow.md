# E45: retrospective MIDI time overflow (#45)

1. **Failure.** An extreme retrospective note start produced an internal error after a
   contained command panic, instead of a parameter error. A new musical address exposed
   the same path already reachable with `start_ticks: 9223372036854775807`.
2. **Mechanism.** `retro_record` in `crates/engine/src/cmd/event_more.rs` evaluated
   `unwrap_or(first + 1)` eagerly even when the maximum note end existed. At `i64::MAX`
   this overflowed. Input bounds also allowed times beyond the session position limit.
   The fix checks each note's end and sample position before building the clip and uses
   a lazy, saturating fallback.
3. **Why checks missed it.** Existing retrospective-record tests used ordinary timing
   values. Generic hostile-parameter tests did not construct a nested captured note at
   the signed integer limit.
4. **Regression test.** `retrospective_record_rejects_extreme_addresses_without_an_internal_panic`
   in `crates/engine/tests/assistant_access.rs` tries the musical address
   `2401919801264265|3|127`, numeric `i64::MAX`, and a nonoverflowing end beyond the session
   limit. It requires a regular parameter error and an unchanged session. Run before the
   fix: failed with `attempt to add with overflow` and `EngineError::Internal`.
5. **Gate class.** Checked time arithmetic on nested command inputs, including both musical
   aliases and legacy numeric paths. Run with `cargo test --locked -p soundcraft-engine`.
6. **Protocol change, owner, blind spot.** Include nested note-time boundary inputs when
   reviewing MIDI command changes. The engine command area owns this gate. It covers
   retrospective capture admission, not every existing MIDI transform or hostile tempo map.
