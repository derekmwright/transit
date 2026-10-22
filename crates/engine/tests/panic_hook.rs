//! The real installed hook is measured separately from Rust's allocating unwind runtime.
//! One test in its own binary owns the process-wide hook and logger.

use soundcraft_engine::{Engine, audio_thread_panics};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use transit_rt::testing::{AllocStats, CountingAlloc, measure};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

static PREVIOUS_CALLS: AtomicU64 = AtomicU64::new(0);
static LOG: TestLog = TestLog(Mutex::new(Vec::new()));

std::thread_local! {
    static HOOK_STATS: Cell<AllocStats> = const { Cell::new(AllocStats { allocs: 0, frees: 0, bytes: 0 }) };
}

struct TestLog(Mutex<Vec<String>>);

impl log::Log for TestLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if let Ok(mut records) = self.0.lock() {
            records.push(format!("{}: {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

#[test]
fn audio_panic_hook_allocates_nothing_and_non_audio_panics_still_log() {
    log::set_logger(&LOG).unwrap();
    log::set_max_level(log::LevelFilter::Error);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PREVIOUS_CALLS.fetch_add(1, Ordering::Relaxed);
        previous(info);
    }));
    let _engine = Engine::default();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Count only the production hook, including any previous hook it calls. Measuring
        // catch_unwind itself would also count the runtime's allocation of the panic payload.
        let ((), stats) = measure(|| hook(info));
        HOOK_STATS.set(stats);
    }));

    let before = audio_thread_panics();
    for (i, message) in ["callback panic", "render worker panic"].into_iter().enumerate() {
        let (caught, stats) = std::thread::spawn(move || {
            soundcraft_mix::mark_audio_thread();
            let caught = std::panic::catch_unwind(|| std::panic::panic_any(message));
            (caught, HOOK_STATS.get())
        })
        .join()
        .unwrap();

        assert_eq!(stats, AllocStats::default(), "the audio panic hook must allocate and free nothing");
        assert_eq!(audio_thread_panics(), before + i as u64 + 1);
        assert_eq!(PREVIOUS_CALLS.load(Ordering::Relaxed), 0, "the audio hook must not chain to stderr");
        let logged = !LOG.0.lock().unwrap().is_empty();
        assert!(!logged, "the audio hook must not call the logger");
        // The control thread can inspect and drop the retained payload without hook formatting.
        assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&message));
    }

    assert!(!soundcraft_mix::on_audio_thread());
    let count = audio_thread_panics();
    let caught = std::panic::catch_unwind(|| std::panic::panic_any("control panic"));
    assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&"control panic"));
    assert_eq!(audio_thread_panics(), count, "non-audio panics do not change the audio counter");
    assert_eq!(PREVIOUS_CALLS.load(Ordering::Relaxed), 1, "the control hook still chains");
    let records = LOG.0.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert!(records[0].starts_with("ERROR: panic:"));
    assert!(records[0].contains("control panic"));
    assert!(records[0].contains("panic_hook.rs"), "the panic location is still reported");
    assert!(HOOK_STATS.get().allocs > 0, "the control hook is a positive control for the allocator probe");
}
