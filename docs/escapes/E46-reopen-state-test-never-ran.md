# E46: the Windows reopen-state test passed without running (#36)

1. **Failure.** `audio_reopen_preserves_live_state_and_refuses_pending_document_without_teardown`
   (`crates/ui-studio/src/player_tests.rs`, Windows only) re-runs itself in a child process
   so the real CLAP fixture stays out of the other UI tests' plug-in catalog. The child was
   started with the filter `player_tests::reopen_state::…`, but the test lives at
   `host::player_tests::reopen_state::…`. With `--exact`, the child matched no test, ran zero
   tests, exited 0, and the parent passed. Live plug-in state across a device reopen was
   therefore never checked. The ADR 0018 work found it when the test kept passing after the
   change that should have broken it.
2. **Mechanism.** The parent asserted only the child's exit status. A test binary that runs zero
   tests exits 0, so a wrong or stale filter turns the test into a silent pass. The module had
   moved under `host` after the filter string was written.
3. **Why checks missed it.** The suite reported the parent as passed. Nothing counted the tests
   the child ran, and no review compared the filter with the test's module path.
4. **Regression tests.** The filter now names `host::player_tests::reopen_state::…`, and the
   parent asserts that the child's output contains `1 passed`. With the filter fixed, the test
   failed against the ADR 0018 branch, because opening an editor is now asynchronous. It now
   waits for the host thread and passes. The same assertion is in the new child-process test
   `crates/ui-studio/src/plugin_host_tests.rs`.
5. **Gate class.** A child-process test is not evidence unless the child ran its test. Every
   test that re-runs itself (or another test binary) in a child must assert that the child ran
   at least one test, for example `1 passed` in its output, and not only that it exited 0.
6. **Protocol change, owner, blind spot.** Reviews of a child-process test check the filter
   against the test's module path and look for the run-count assertion. Owner: whoever adds or
   moves such a test. Blind spot: the older child-process tests
   (`plugin_requests_tests.rs` in ui-studio, and the VST3 `module_exit*` harnesses, which run
   `main` directly rather than a filter) were not audited in this change.
