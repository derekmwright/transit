# ADR 0018: A plug-in host thread owns hosted instances; the UI only sends requests

- **Status:** proposed 2026-10-23 (issue #36, known issue K10; implemented on `feat/plugin-host-thread`, awaiting review)
- **Date:** 2026-10-23
- **Reopen when:** a reference plug-in misbehaves because its main thread is not the process main thread on Windows; macOS needs the same guarantee (that needs editors out of process, or a run-loop design); or out-of-process hosting (G-13) is taken up, which replaces the stuck-thread handling below.

## Context

Hosted plug-ins (CLAP, VST3, AU) were created, restored, opened and destroyed on the UI thread. The Player already announces a new instance a frame before making it ("Loading…", `playback/src/player/handles.rs`), but the make itself (module entry, VST3 `createInstance` and `initialize`, CLAP `init` and `activate`, `setState`) is one SDK call that a large sampler can keep for seconds, and the UI is frozen meanwhile. Editor creation (`IPlugView::attached`, `clap_plugin_gui.create`), state reads for Save, and destruction of retired instances also ran there. A plug-in that never returns from `initialize` froze the app for good. The scan is already off-thread (`plugin_scan.rs`, child processes) and stays so.

What each SDK requires of the thread that hosts it:

- **VST3:** the edit controller, its views and the component's `initialize`, `setState`, `setActive` and `terminate` run on the host's "UI thread", which must be one thread per instance and must dispatch window messages for the views. Windows has no process main thread: any thread with a message loop qualifies. On macOS, views are `NSView`s and AppKit requires the process main thread.
- **CLAP:** `[main-thread]` calls go to the thread the host designates as main; `request_callback` must be answered there. GUI embedding on Windows needs that thread to pump messages; on macOS the Cocoa API requires the process main thread.
- **AU:** macOS only; the view and most property calls are main-thread.
- **Module entry and exit** (`InitDll`/`ExitDll`, `bundleEntry`/`bundleExit`) must pair on one thread; Qt-based modules tie their application object to it.

## Decision

1. **One plug-in host thread per process** (`transit-plugin-host`, layer 1 beside the format hosts) owns hosted instances for their whole life: creation, state restore, the editor (open, idle, close), state reads, `restartComponent`/`request_callback` servicing, and destruction. It is the instances' main thread in every SDK's sense; VST3 modules are entered and exited on it. It pumps the OS message queue (Windows: an OLE single-threaded apartment, `MsgWaitForMultipleObjectsEx`, `PeekMessage`/`DispatchMessage`; elsewhere a park with a timeout) between requests, and every 15 ms runs the formats' service calls and each editor's `idle`. The pump is a safe API in `transit-rt` (ADR 0013).
2. **The UI sends requests and polls results; it never waits for a plug-in for more than a frame.** A request is a job on the host thread's queue; the result comes back through a shared slot the UI polls once per frame. The slot shows "Loading…" from the frame the request is announced until the instance is bound. Editor open, close and parameter echo are fire-and-forget; the editor reports open, closed and its edits through shared state the UI reads. Parameter edits and automation keep the existing queues to the audio thread.
3. **Handoff to the audio thread is unchanged.** The host thread returns a finished, prepared `NewInstance`; the control (UI) thread compiles it into a `PreparedPlan` and publishes it through the existing lock-free queue (`playback/src/publication.rs`), adopted at a block boundary. The audio thread does not allocate, lock or wait for anything new: the instance is wrapped in a `Hosted` forwarder (one extra virtual call per method, no allocation; tested with the counting allocator).
4. **Destruction goes back to the host thread.** Dropping the `Hosted` wrapper (on the control thread, when a plan retires it) sends the inner instance to its host thread, which destroys it there. Editor and state handles live on the host thread; the UI holds proxies.
5. **Timeout and the stuck state.** Every request and tick is bounded by 10 s of running time; one window-message dispatch gets 120 s, because a plug-in's modal file dialog or message box runs inside it for as long as the user leaves it open. The session request that needs a new instance waits for it (the active plan keeps playing), so other edits in that request wait too, at most the timeout. When a call runs past its budget, the host thread is declared **wedged**:
   - The creation that ran past the timeout is stuck: the slot reads "Not responding", the status line says so, and the session compiles without that instance (empty, as for a plug-in that fails to load). If the call later returns, that late instance is destroyed on its own thread; re-inserting the plug-in retries.
   - A fresh host thread takes over new creations; creations that had not started move to it.
   - While the thread is wedged, the instances on it keep processing audio, but their editors and state reads answer "not responding" at once, and a save keeps their stored states. The status line says their live state is not being saved.
   - **When the call returns, the thread is responsive again** (review B1): its instances' state reads, editors and destruction work as before, and the next save carries their live states. It stays a non-current thread; new creations stay on the fresh one.
   - A thread that never returns is never joined; its instances are leaked rather than destroyed elsewhere.
6. **What still waits.** Reading plug-in states for Save, Save As, autosave and a device reopen waits for the host thread, bounded by the timeout per wedge (a thread wedged at that moment answers at once with no state, so that plug-in's stored state is kept; once it returns, saves read it again). Shutdown sends one request per live host thread and waits at most 5 s in total; a host thread busy in a plug-in does not hold the exit.
7. **Platforms.** Windows and Linux run the host thread. On Windows the editor's `HWND` is created and its messages are dispatched on the host thread (achieved, not a fallback). **macOS keeps today's behavior** (requests run inline on the UI thread): AppKit requires editors on the process main thread, and VST3 and CLAP require the editor and the instance on the same thread. On Windows the editor window is now created on the host thread with **no owner window**: a cross-thread owner would attach the two threads' input queues and let a hung plug-in hang the UI, so the editor gets its own taskbar button and no longer floats above the main window.
8. **Crashes are out of scope.** A plug-in that crashes (access violation) still takes the process down; containing that is out-of-process hosting (G-13, issue #9).

## Consequences

- Loading a heavy instrument, opening its editor, and removing it leave the UI responsive; a plug-in hung in `initialize` costs one thread, not the session.
- One host thread serializes plug-ins: a slow load delays other plug-ins' editor messages and requests for its duration (not the UI or audio).
- Code that creates instances itself (offline bounce, freeze, stems, tests) still runs on its own thread; it is not routed through the host thread (see the status update below).
- **Status update 2026-10-23 (issue #56).** The review's follow-ups are done, except macOS (section 7):
  - A save reads every plug-in state against one deadline (500 ms in all, `Handles::set_save_deadline`), not up to the timeout per plug-in; a thread inside one window message for longer than the request timeout answers at once. A plug-in that has not answered keeps its stored state and the status line names it ("State of X not refreshed: plug-in busy"). Section 6's wait is bounded by that deadline.
  - An instance the compile asks for that staging did not predict (a slot whose layout changed) no longer blocks the UI (`make_now` is gone). The compile submits it to the host thread and leaves the slot vacant, reading "Loading…"; when it is ready, the compiler is asked to make that vacant slot again (`PlanCompiler::retry_vacant`) and the same document is requested again, as a latency refresh is, so the next plan takes it. The active plan keeps playing and the slot is silent meanwhile. Where the host runs inline (macOS) it is made in the compile, as before.
  - The host thread drops its job queue before its message queue (the COM apartment) ends, so a destroy job sent at exit is leaked, never run after it. The stuck check names the busy section it found overdue, so a call that returns while it is being marked never leaves an idle thread wedged or starts a takeover thread for it.
  - VST3 modules are entered outside the loaded-module lock (K38, closed): a hung module entry holds back loads of its own binary only. Both registries build a type's description outside their lock.
  - A command that needs the parameter table of a type never made (`mix.insert_param`) reads it on the host thread and waits at most 500 ms; past that the command is refused as still loading and the read carries on, so a retry finds it. Adding a plug-in that needs its description (`mix.insert` or `mix.instrument` with parameter values, or a type the scan does not list) reads it on the host thread too, but waits up to the request timeout, as the command did when it instantiated the plug-in itself.
- **Offline renders stay where they are.** Bounce, stems and freeze build a `RangeRender` synchronously in the command (`engine/src/io/bounce.rs`, `io/paths.rs`, `cmd/freeze.rs`) on the calling thread, which then makes, processes and destroys every instance; no editor is opened for them, so the one-thread-per-instance rule holds. Routing their creation through the host thread with a blocking wait would not free the calling thread (it waits as long), and it would tie the render to the live host thread: a plug-in's modal dialog there (up to the 120 s dispatch budget) or a wedged thread would make the render give up on its instances and write audio without them, where today it renders correctly. Making it asynchronous needs a "preparing" phase, polled per frame, in the bounce, stems and freeze jobs; that is left for when one of those jobs needs it.
- New `unsafe` (the message pump) is in `transit-rt`, which needs the ADR 0009 quoted-unsafe review before merge.

## Alternatives rejected

- **Keep creation on the UI thread and only defer by a frame** (the current mitigation): the call still blocks for its full duration.
- **A host thread per format:** no format requires it, it doubles the threads that need a message loop, and the module-thread rule is easier with one.
- **Editors as children of an egui viewport on the UI thread:** keeps the UI thread exposed to every plug-in's view code and needs a cross-thread parent, which attaches input queues.
- **Out-of-process hosting:** the real fix for crashes, much larger; left to G-13.

## Related

[ADR 0007](0007-per-plugin-suspension-not-a-global-engine-switch.md) (per-plug-in threading and suspension), [ADR 0013](0013-unsafe-only-in-soundcraft-rt.md) (unsafe home), [ADR 0016](0016-native-wasapi-backend.md) (one owning thread per device, requests with timeouts), `docs/contributing/knowledge/audio-thread.md`, issues #36 and #9.
