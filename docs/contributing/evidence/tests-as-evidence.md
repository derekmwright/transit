# Tests as evidence

A test is evidence when it can fail for the reason you claim.

1. **Name the property**, not the function: `a_refused_edit_reaches_the_status_line_and_changes_nothing`,
   not `test_edit`.
2. **Assert the number.** "Zero allocations in 1000 blocks", "latency is 256 frames", "peak is
   0.5", not "does not crash" or "output is non-empty".
3. **Fail without the fix.** Revert the change, run the test, see it fail, restore. State that you
   did so in the pull request.
4. **Reach the code path.** A test that never executes the claimed path proves nothing. Confirm
   with a deliberate break.
5. **Table-driven for classes.** One row per cell of the defect class (track kind x armed x monitor)
   so a new case is a row.
6. **Hostile input.** Anything derived from files, MIDI or plug-ins gets a test with bad values; see
   `crates/engine/tests/hostile_*.rs`.
7. **Deterministic.** No sleeps that decide pass or fail on a loaded machine; use the fake device or
   a scripted clock. No third-party plug-ins; use the fixtures.
8. **Goldens are evidence only when reviewed.** Updating a file in `crates/mix/tests/golden` needs a
   stated reason in the pull request: what changed audibly and why the new result is correct.
9. **Audio thread:** measure allocations with `measure` (`crates/rt/src/testing.rs`) around the call
   and assert `is_zero()`.
