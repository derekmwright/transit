//! Gate G7 (ADR 0012, audit F08), CLAP side: what a hosted plug-in asks of the host through
//! `clap_host` and `clap_host_params` is done, on the thread CLAP requires.
//!
//! The fixture's requests plug-in logs every lifecycle call with a token of the calling thread and
//! reports the log as its "state" (see `tests/fixture`). The thread that creates an instance is
//! its main thread (CLAP creates plug-ins on the main thread), so each test compares the tokens of
//! main-thread calls with the token of its own thread, read the same way (`me=`): a recorded
//! thread identity, not a thread name.

mod common;

use soundcraft_dsp::{Plugin, PluginStateReader, ProcessStatus};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REQUESTS: &str = "clap:org.soundcraft.test.requests";
const BLOCK: usize = 64;

/// The fixture's report: (`gain`, `latency`, `processes`, the reader's thread token, the log).
struct Report {
    gain: f64,
    latency: u32,
    processes: u64,
    me: u64,
    log: Vec<(String, u64)>,
}

impl Report {
    fn read(reader: &mut Box<dyn PluginStateReader>) -> Report {
        let text = String::from_utf8(reader.save_state().expect("the requests fixture reports its log")).unwrap();
        let field = |k: &str| text.split(';').find_map(|kv| kv.strip_prefix(k)).unwrap_or_default().to_string();
        let log = field("log=")
            .split(',')
            .filter(|e| !e.is_empty())
            .map(|e| {
                let (what, token) = e.rsplit_once('@').unwrap();
                (what.to_string(), token.parse().unwrap())
            })
            .collect();
        Report {
            gain: field("gain=").parse().unwrap(),
            latency: field("latency=").parse().unwrap(),
            processes: field("processes=").parse().unwrap(),
            me: field("me=").parse().unwrap(),
            log,
        }
    }

    /// The calls named `what`, with their thread tokens.
    fn calls(&self, what: &str) -> Vec<u64> {
        self.log.iter().filter(|(w, _)| w == what).map(|(_, t)| *t).collect()
    }

    /// Index of the last `what` in the log.
    fn last(&self, what: &str) -> Option<usize> {
        self.log.iter().rposition(|(w, _)| w == what)
    }

    fn names(&self) -> Vec<&str> {
        self.log.iter().map(|(w, _)| w.as_str()).collect()
    }

    /// The same report with only the log entries after the first `n`.
    fn since(mut self, n: usize) -> Report {
        self.log.drain(..n.min(self.log.len()));
        self
    }
}

fn silent() -> Vec<Vec<f32>> {
    vec![vec![0.0f32; BLOCK]; 2]
}

/// What the mixer does with a hosted effect whose input is silent (`soundcraft_mix::run_insert`):
/// it leaves the plug-in unprocessed while its status allows that. True when it processed.
fn mixer_block(p: &mut Box<dyn Plugin>) -> bool {
    let asleep = matches!(p.process_status(), ProcessStatus::Sleep);
    if asleep {
        return false;
    }
    p.set_input_silent(true);
    p.process(&mut silent(), BLOCK);
    true
}

/// A requests fixture created and prepared on this thread, its state reader, and how many log
/// entries the set-up wrote (all on this thread: creation and `prepare` are main-thread work).
fn new_requests() -> (Box<dyn Plugin>, Box<dyn PluginStateReader>, usize) {
    common::requests_fixture_dir();
    let mut p = soundcraft_clap_host::create(REQUESTS).expect("requests fixture");
    p.prepare(48_000.0, BLOCK, 2);
    assert!(p.editor().is_none(), "the requests fixture has no editor");
    let mut reader = p.state_reader().expect("the requests fixture reports through clap.state");
    let r = Report::read(&mut reader);
    assert_eq!(r.names(), ["activate", "deactivate", "activate"], "created prepared, then prepared again");
    assert!(r.log.iter().all(|(_, t)| *t == r.me), "set-up runs on this thread: {:?}", r.log);
    (p, reader, r.log.len())
}

#[test]
fn a_sleeping_plugin_that_requests_process_is_woken_with_no_audio_and_no_editor() {
    let (mut p, mut reader, _) = new_requests();
    assert!(mixer_block(&mut p), "the first block runs");
    assert_eq!(p.process_status(), ProcessStatus::Sleep);
    for _ in 0..10 {
        assert!(!mixer_block(&mut p), "silence keeps it asleep");
    }
    assert_eq!(Report::read(&mut reader).processes, 1);

    // CLAP `request_process` from the plug-in's own (IO) thread: "the host will then schedule a
    // call to process" even though the input is silent and no editor is open.
    assert!(p.load_state(b"CMD:process-now"));
    assert_eq!(p.process_status(), ProcessStatus::Continue, "the request wakes it");
    assert!(mixer_block(&mut p));
    assert_eq!(p.process_status(), ProcessStatus::Sleep, "one block, then it may sleep again");
    assert!(!mixer_block(&mut p));
    assert_eq!(Report::read(&mut reader).processes, 2);

    // `clap_host_params.request_flush` on a plug-in that is not processing: CLAP lets the host
    // answer with either `flush` or `process`; this host processes a block.
    assert!(p.load_state(b"CMD:flush-now"));
    assert_eq!(p.process_status(), ProcessStatus::Continue, "a flush request wakes it");
    assert!(mixer_block(&mut p));
    assert!(!mixer_block(&mut p));
    assert_eq!(Report::read(&mut reader).processes, 3);
}

#[test]
fn restart_requested_on_the_audio_thread_runs_on_main_keeps_state_and_reports_the_new_latency() {
    let (mut p, mut reader, base) = new_requests();
    assert_eq!(p.latency(), 5);
    // At its third block (on the audio thread) the plug-in asks for a restart, after which it
    // reports 64 samples of latency.
    assert!(p.load_state(b"CMD:restart-at:3:64"));
    assert!(p.set_param("7", 1.5));
    let latency = Arc::new(AtomicUsize::new(p.latency()));
    let stop = Arc::new(AtomicBool::new(false));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let (lat, halt, outs) = (Arc::clone(&latency), Arc::clone(&stop), Arc::clone(&outputs));
    let audio = std::thread::spawn(move || {
        let mut io = vec![vec![0.0f32; BLOCK]; 2];
        while !halt.load(Ordering::SeqCst) {
            for ch in io.iter_mut() {
                ch.fill(0.5);
            }
            // Audible input: a mixer processes it every block, whatever its status.
            p.set_input_silent(false);
            p.process(&mut io, BLOCK);
            outs.lock().unwrap().push(io[0][0]);
            lat.store(p.latency(), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
        }
        p
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while latency.load(Ordering::SeqCst) != 64 && Instant::now() < deadline {
        soundcraft_clap_host::service_requests();
        std::thread::sleep(Duration::from_millis(1));
    }
    // A few more blocks after the restart.
    let after = outputs.lock().unwrap().len() + 5;
    while outputs.lock().unwrap().len() < after && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::SeqCst);
    let p = audio.join().unwrap();
    assert_eq!(p.latency(), 64, "the latency read after the reactivation is reported (feeds delay compensation)");

    let r = Report::read(&mut reader).since(base);
    let main = r.me;
    let audio = r.calls("process")[0];
    assert_ne!(audio, main);
    assert_eq!(r.calls("request_restart"), vec![audio], "asked from the audio thread");
    // CLAP: activate/deactivate on the main thread, start/stop_processing on the audio thread.
    assert_eq!(r.calls("activate"), vec![main], "the restart reactivates on the main thread");
    assert_eq!(r.calls("deactivate"), vec![main], "the restart deactivates on the main thread");
    assert_eq!(r.calls("latency.changed"), vec![main], "during activate, where CLAP allows it");
    assert_eq!(r.calls("stop_processing"), vec![audio], "processing stops on the audio thread");
    assert!(r.calls("start_processing").iter().all(|&t| t == audio));
    assert_eq!(r.calls("start_processing").len(), 2, "processing restarts after the reactivation");
    // Order: request, stop, deactivate, activate, start, and no process call in between.
    let names = r.names();
    let at = |w: &str, from: usize| names.iter().skip(from).position(|n| *n == w).map(|i| i + from).unwrap();
    let req = at("request_restart", 0);
    let stop = at("stop_processing", req);
    let deact = at("deactivate", stop);
    let act = at("activate", deact);
    let start = at("start_processing", act);
    assert!(!names[stop..start].contains(&"process"), "no block while restarting: {names:?}");
    // State: the restart keeps the instance (and its gain); the host keeps its parameter values.
    assert_eq!(r.gain, 1.5);
    assert_eq!(r.latency, 64);
    assert_eq!(p.param("7"), Some(1.5));
    let outs = outputs.lock().unwrap();
    assert!((outs.last().unwrap() - 0.75).abs() < 1e-6, "gain kept across the restart: {outs:?}");
    assert!(r.last("process").unwrap() > start);
}

#[test]
fn restart_of_an_idle_plugin_is_serviced_on_main_only() {
    let (mut p, mut reader, base) = new_requests();
    // Asked from the plug-in's IO thread while it is not processing.
    assert!(p.load_state(b"CMD:restart-now:32"));
    let me = std::thread::current().id();
    std::thread::spawn(soundcraft_clap_host::service_requests).join().unwrap();
    assert_eq!(std::thread::current().id(), me);
    let r = Report::read(&mut reader).since(base);
    assert_eq!(r.calls("deactivate").len(), 0, "another thread does not service this instance's requests");
    assert_eq!(p.latency(), 5);

    soundcraft_clap_host::service_requests();
    let r = Report::read(&mut reader).since(base);
    assert_eq!(r.calls("deactivate"), vec![r.me]);
    assert_eq!(r.calls("activate"), vec![r.me]);
    assert_eq!(r.calls("latency.changed"), vec![r.me]);
    assert_eq!(p.latency(), 32);
    // Done: a second call does nothing more.
    soundcraft_clap_host::service_requests();
    assert_eq!(Report::read(&mut reader).since(base).calls("activate").len(), 1);
    // Processing starts on the audio thread as usual.
    p.set_input_silent(false);
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK);
    assert_eq!(Report::read(&mut reader).processes, 1);
}

#[test]
fn request_callback_of_a_plugin_without_editor_runs_on_main() {
    let (mut p, mut reader, _) = new_requests();
    assert!(p.load_state(b"CMD:callback-now"));
    std::thread::spawn(soundcraft_clap_host::service_requests).join().unwrap();
    assert!(Report::read(&mut reader).calls("on_main_thread").is_empty(), "not off the main thread");
    soundcraft_clap_host::service_requests();
    let r = Report::read(&mut reader);
    assert_eq!(r.calls("on_main_thread"), vec![r.me]);
    soundcraft_clap_host::service_requests();
    assert_eq!(Report::read(&mut reader).calls("on_main_thread").len(), 1, "once per request");
    drop(p);
}

/// Editor handles run on the process main thread on macOS (see `editor_main_thread.rs`).
#[cfg(not(target_os = "macos"))]
#[test]
fn a_values_rescan_is_read_on_main_and_reported_like_an_editor_edit() {
    common::fixture_dir();
    let mut p = soundcraft_clap_host::create("clap:org.soundcraft.test.gain").expect("gain fixture");
    p.prepare(48_000.0, BLOCK, 2);
    let mut ed = p.editor().expect("the gain fixture has an editor handle");
    assert!(ed.idle().is_empty());
    // The plug-in changes its gain itself and calls `params.rescan(CLAP_PARAM_RESCAN_VALUES)`; it
    // refuses the blob as a state, so the host learns the value only through the rescan.
    assert!(!p.load_state(b"CMD:rescan-values:1.25"));
    assert_eq!(p.param("7"), Some(1.0));
    let off = std::thread::spawn(soundcraft_clap_host::service_requests).join().unwrap();
    assert_eq!(off.rescans, 0, "not off the main thread");
    assert_eq!(ed.idle(), vec![("7".to_string(), 1.25)], "the re-read value that differs from the host's, reported for the session");
    assert!(ed.idle().is_empty(), "once");
    // The host now holds 1.25. The same value again is no news; a value back to what the host
    // set before is (review R7: the baseline is the host's view, not the last value read).
    assert!(!p.load_state(b"CMD:rescan-values:1.25"));
    assert!(ed.idle().is_empty(), "nothing differs from the host's view");
    assert!(p.set_param("7", 0.5));
    assert!(!p.load_state(b"CMD:rescan-values:1.25"));
    assert_eq!(ed.idle(), vec![("7".to_string(), 1.25)], "differs from the 0.5 the host set");
    // CLAP_PARAM_RESCAN_ALL re-reads the values too.
    assert!(!p.load_state(b"CMD:rescan-all:0.75"));
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.75)]);
}

/// The fixture's gain applied to a 0.5 input at 48 kHz: 2 ms fades are 96 samples.
const FADE: usize = 96;

/// Review R1: while the main thread restarts it, the slot outputs silence with a short fade out
/// from its last output and a fade in when it processes again, never the unprocessed input, and
/// keeps reporting its latency (delay compensation stays aligned).
#[test]
fn a_parked_block_is_silent_with_fades_and_keeps_the_latency() {
    let (mut p, _reader, _) = new_requests();
    assert!(p.set_param("7", 0.25));
    p.set_input_silent(false);
    let block = |p: &mut Box<dyn Plugin>| {
        let mut io = vec![vec![0.5f32; BLOCK]; 2];
        p.process(&mut io, BLOCK);
        io
    };
    let wet = block(&mut p)[0][BLOCK - 1];
    assert!((wet - 0.125).abs() < 1e-6);
    assert!(p.load_state(b"CMD:restart-now:32"));
    let s = soundcraft_clap_host::service_requests();
    assert_eq!(s.restarts_waiting, 1, "it is processing: the audio side parks it first");
    let parking = block(&mut p);
    let parked = block(&mut p);
    assert_eq!(p.latency(), 5, "the latency stays until the restart is done");
    let out: Vec<f32> = parking[0].iter().chain(&parked[0]).copied().collect();
    for (i, &x) in out.iter().enumerate() {
        let want = if i < FADE { wet * (1.0 - i as f32 / FADE as f32) } else { 0.0 };
        assert!((x - want).abs() < 1e-6, "sample {i}: {x}, want {want} (a fade out from {wet}, never the 0.5 input)");
    }
    assert_eq!(parking[0], parking[1], "every channel");
    assert_eq!(soundcraft_clap_host::service_requests().restarted, 1);
    assert_eq!(p.latency(), 32);
    let a = block(&mut p);
    let b = block(&mut p);
    let out: Vec<f32> = a[0].iter().chain(&b[0]).copied().collect();
    for (i, &x) in out.iter().enumerate() {
        let want = if i < FADE { wet * i as f32 / FADE as f32 } else { wet };
        assert!((x - want).abs() < 1e-6, "sample {i}: {x}, want {want} (a fade in)");
    }
}

/// Review R4: a restart parked while nothing calls `process` any more (device stopped, insert
/// bypassed) is completed by the main thread after a bounded wait. CLAP `thread-check.h`: "The
/// host may mark any OS thread, including the main-thread as the audio-thread", as long as "only
/// one OS thread is the audio-thread at a time in a plugin instance"; holding the instance makes
/// the main thread that one thread for `stop_processing`.
#[test]
fn a_parked_restart_without_callbacks_completes_on_main_after_a_deadline() {
    let (mut p, mut reader, base) = new_requests();
    p.set_input_silent(false);
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK); // in the processing state
    assert!(p.load_state(b"CMD:restart-now:32"));
    // The device stops: no more process calls.
    let first = soundcraft_clap_host::service_requests();
    assert_eq!((first.restarted, first.restarts_waiting), (0, 1));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut done = 0;
    while done == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        done = soundcraft_clap_host::service_requests().restarted;
    }
    assert_eq!(done, 1, "completed without an audio callback");
    assert_eq!(p.latency(), 32);
    let r = Report::read(&mut reader).since(base);
    let names = r.names();
    let stop = names.iter().position(|n| *n == "stop_processing").expect("stopped");
    assert_eq!(&names[stop..stop + 3], ["stop_processing", "deactivate", "activate"], "{names:?}");
    assert_eq!(r.log[stop].1, r.me, "the main thread stood in as the audio thread");
    // It processes again when callbacks resume.
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK);
    assert_eq!(Report::read(&mut reader).processes, 2);
}

/// Review R6: `reset` (stop, seek) always reaches the plug-in, on the audio side, before its next
/// block, however busy the main thread keeps the instance.
#[test]
fn reset_reaches_the_plugin_before_its_next_block() {
    let (mut p, mut reader, base) = new_requests();
    p.set_input_silent(false);
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK);
    p.reset();
    p.process(&mut vec![vec![0.5f32; BLOCK]; 2], BLOCK);
    let r = Report::read(&mut reader).since(base);
    let names = r.names();
    let reset = names.iter().position(|n| *n == "reset").expect("reset");
    assert_eq!(names[reset + 1], "process", "{names:?}");
    assert_eq!(r.log[reset].1, r.log[reset + 1].1, "on the thread that processes");
}
