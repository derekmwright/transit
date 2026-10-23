# E45: MIDI position format compatibility (#45)

1. **Failure.** Review of the musical aliases found that `midi.new_clip` and retrospective
   note starts rejected existing clock positions (`0:12`, `00:00:10:00`) and silently
   interpreted `1:2` as beat 2 instead of 62 seconds.
2. **Mechanism.** `is_musical` in `crates/engine/src/cmd/midi_time.rs` intercepted numeric
   colon strings before the existing position parser. Musical addresses now use pipes,
   `bar|beat[|tick]`; colon strings continue through the unchanged position parser.
3. **Why checks missed it.** The first acceptance tests covered the new musical aliases,
   but did not mix them with existing clock formats on the same command entry points.
4. **Regression tests.** `clip_position_formats_keep_clock_time_distinct_from_musical_time`
   and `retrospective_position_formats_keep_clock_time_distinct_from_musical_time` in
   `crates/engine/tests/assistant_access.rs` assert exact sample positions for `0:12`,
   `1:30`, `00:00:10:00`, `1:2`, `1|2`, `5|1|000`, and `bars_beats:5|1|000`.
   Both tests failed on `0:12` before the fix and passed afterward. The clip test also
   covers `end` and `length` routing. At 48 kHz, `1:2` is 2,976,000 samples, while `1|2`
   at 120 BPM in 4/4 is 24,000 samples.
5. **Gate class.** Backward-compatible dispatch of musical aliases and existing position
   formats. Run with `cargo test --locked -p soundcraft-engine`.
6. **Protocol change, owner, blind spot.** New position syntaxes must be tested alongside
   existing unprefixed and prefixed formats through each affected command. The engine
   command area owns this gate. These rows use the default 48 kHz, 120 BPM, 4/4 session;
   separate musical-address tests exercise tempo changes and meter changes.
