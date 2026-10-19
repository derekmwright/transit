//! Test-only CLAP plugins for soundcraft-clap-host: a gain effect and a "DC synth".
//!
//! - `org.soundcraft.test.gain`: stereo in/out, param 7 "Gain" (0..2, default 1), param 3 "Mode"
//!   (stepped enum A/B/C), hidden param 9, latency 3 samples.
//! - `org.soundcraft.test.synth`: instrument, stereo out, CLAP note port; outputs a constant equal
//!   to the velocity of the held note (0 when none), switching at the event's sample offset. A
//!   MIDI CC 7 (volume) sets the constant to its value / 127.
//!
//! The gain plugin also implements `clap.state` (gain and mode as two little-endian f64s) and a
//! floating-only `clap.gui` without a real window: showing it "edits" the gain to 0.25 in the GUI,
//! reported as an output parameter event by the next `process`.
//!
//! - `org.soundcraft.test.requests` (only in the build with the `requests` feature, which lists
//!   nothing else): a stereo gain effect (params as the gain plugin, latency 5,
//!   no GUI) that sleeps (`CLAP_PROCESS_SLEEP`) after every block and exercises the host requests
//!   (audit F08, ADR 0012 G7). It logs every lifecycle call with a token of the calling thread.
//!   Commands arrive as a "state" to load, `CMD:<command>` (see `requests_command`); its saved
//!   "state" is that log as text, with the token of the thread saving it (`me=`), so a test can
//!   compare each call's thread with its own.
//! - `org.soundcraft.test.events` (only in the build with the `events` feature, which lists
//!   nothing else; F10): the synth with param 5 "Probe" (0..1) that writes every input event into
//!   its output instead of a level, one record of four samples each, in the order it reads them:
//!   `[code, time, a, b]` with code 1 note on, 2 note off (a = channel * 1000 + key, b = note id),
//!   3 pressure expression (a as for notes, b = value * 1e6), 4 parameter (a = id,
//!   b = value * 1e6), 5 MIDI (a = the three bytes, big-endian, b = 0). It keeps the sorted and
//!   in-block assertions. Integers below 2^24 are exact in `f32`.
#![allow(clippy::missing_safety_doc, non_upper_case_globals)]

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::state::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::stream::*;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{CStr, c_char, c_void};

struct Features<const N: usize>([*const c_char; N]);
unsafe impl<const N: usize> Sync for Features<N> {}

static GAIN_FEATURES: Features<3> = Features([c"audio-effect".as_ptr(), c"stereo".as_ptr(), std::ptr::null()]);
static SYNTH_FEATURES: Features<3> = Features([c"instrument".as_ptr(), c"synthesizer".as_ptr(), std::ptr::null()]);
static REQUESTS_FEATURES: Features<2> = Features([c"audio-effect".as_ptr(), std::ptr::null()]);

static EVENTS_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.soundcraft.test.events".as_ptr(),
    name: c"Fixture Events".as_ptr(),
    vendor: c"SoundCraft tests".as_ptr(),
    url: std::ptr::null(),
    manual_url: std::ptr::null(),
    support_url: std::ptr::null(),
    version: c"1.0.0".as_ptr(),
    description: std::ptr::null(),
    features: SYNTH_FEATURES.0.as_ptr(),
};

static GAIN_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.soundcraft.test.gain".as_ptr(),
    name: c"Fixture Gain".as_ptr(),
    vendor: c"SoundCraft tests".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1.0.0".as_ptr(),
    description: c"test gain".as_ptr(),
    features: GAIN_FEATURES.0.as_ptr(),
};

static SYNTH_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.soundcraft.test.synth".as_ptr(),
    name: c"Fixture Synth".as_ptr(),
    vendor: c"SoundCraft tests".as_ptr(),
    url: std::ptr::null(),
    manual_url: std::ptr::null(),
    support_url: std::ptr::null(),
    version: c"1.0.0".as_ptr(),
    description: std::ptr::null(),
    features: SYNTH_FEATURES.0.as_ptr(),
};

static REQUESTS_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.soundcraft.test.requests".as_ptr(),
    name: c"Fixture Requests".as_ptr(),
    vendor: c"SoundCraft tests".as_ptr(),
    url: std::ptr::null(),
    manual_url: std::ptr::null(),
    support_url: std::ptr::null(),
    version: c"1.0.0".as_ptr(),
    description: std::ptr::null(),
    features: REQUESTS_FEATURES.0.as_ptr(),
};

/// A small number per OS thread, unique in this process, for the requests plugin's log.
fn thread_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static TOKEN: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    TOKEN.with(|t| *t)
}

/// The host pointer, carried to the requests plugin's "IO" threads.
#[derive(Clone, Copy)]
struct HostPtr(*const clap_host);
unsafe impl Send for HostPtr {}

/// The plugin, for an "IO" thread's log entry.
#[derive(Clone, Copy)]
struct FxPtr(*const Fx);
unsafe impl Send for FxPtr {}

struct Fx {
    plugin: clap_plugin,
    host: *const clap_host,
    synth: bool,
    /// The requests plugin (see the crate docs).
    requests: bool,
    /// The events plugin (see the crate docs).
    events: bool,
    latency: u32,
    /// Latency to take at the next `activate` (requests plugin).
    next_latency: Option<u32>,
    /// Requests plugin: at this `process` call (1-based), ask for a restart to this latency.
    restart_at: Option<(u64, u32)>,
    /// `activate` fails (requests plugin, `CMD:restart-fail-now`).
    refuse_activate: bool,
    /// Every `process` calls `params.rescan(VALUES)` (requests plugin, `CMD:rescan-every-block`).
    rescan_every_block: bool,
    processes: u64,
    /// Requests plugin: (call, thread token) in order.
    log: std::sync::Mutex<Vec<(String, u64)>>,
    gain: f64,
    mode: f64,
    level: f64,
    active: bool,
    processing: bool,
    gui_created: bool,
    gui_visible: bool,
    gui_edit_pending: bool,
}

unsafe fn fx<'a>(p: *const clap_plugin) -> &'a mut Fx {
    unsafe { &mut *((*p).plugin_data as *mut Fx) }
}

fn note(f: &Fx, what: &str) {
    if f.requests {
        f.log.lock().unwrap().push((what.to_string(), thread_token()));
    }
}

/// The host's extension `id`, as `T` (null when it has none).
unsafe fn host_ext<T>(host: *const clap_host, id: &CStr) -> *const T {
    match unsafe { (*host).get_extension } {
        Some(ge) => (unsafe { ge(host, id.as_ptr()) }) as *const T,
        None => std::ptr::null(),
    }
}

/// Runs `f` with the host pointer on a new thread (an "IO" thread of the plugin) and waits for it.
fn on_io_thread(host: *const clap_host, f: impl FnOnce(HostPtr) + Send + 'static) {
    let h = HostPtr(host);
    std::thread::spawn(move || f(h)).join().unwrap();
}

/// `CMD:process-now`, `CMD:process-after:<ms>`, `CMD:flush-now`, `CMD:callback-now`,
/// `CMD:restart-now:<latency>`, `CMD:restart-at:<process call>:<latency>`,
/// `CMD:restart-fail-now` (asks for a restart whose `activate` then fails),
/// `CMD:rescan-every-block` (each `process` calls `params.rescan(VALUES)`),
/// `CMD:rescan-values:<gain>`, `CMD:rescan-all:<gain>`. The `-now` requests come from another thread (an "IO" thread),
/// joined before this returns; `process-after` leaves that thread running; `restart-at` asks from
/// the audio thread (inside `process`); `rescan-values` changes the gain "internally" and calls
/// `params.rescan` (`VALUES` or `ALL`) on this (main) thread, and then refuses the "state", so that the host learns of
/// the values only through the rescan. True when the command was taken as a state.
fn requests_command(f: &mut Fx, cmd: &str) -> bool {
    let parts: Vec<&str> = cmd.split(':').collect();
    let num = |i: usize| parts.get(i).and_then(|x| x.parse::<u64>().ok());
    match parts.first().copied() {
        Some("process-now") => on_io_thread(f.host, |h| {
            let host = unsafe { &*h.0 };
            unsafe { (host.request_process.unwrap())(h.0) };
        }),
        Some("process-after") => {
            // The test keeps the plugin alive until this thread has asked.
            let ms = num(1).unwrap_or(100);
            let h = HostPtr(f.host);
            let me = FxPtr(f as *const Fx);
            std::thread::spawn(move || {
                let (h, me) = (h, me);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                note(unsafe { &*me.0 }, "request_process");
                let host = unsafe { &*h.0 };
                unsafe { (host.request_process.unwrap())(h.0) };
            });
        }
        Some("flush-now") => on_io_thread(f.host, |h| {
            let params = unsafe { host_ext::<clap_host_params>(h.0, CLAP_EXT_PARAMS).as_ref() }.expect("host params");
            unsafe { (params.request_flush.unwrap())(h.0) };
        }),
        Some("callback-now") => on_io_thread(f.host, |h| {
            let host = unsafe { &*h.0 };
            unsafe { (host.request_callback.unwrap())(h.0) };
        }),
        Some("restart-now") => {
            f.next_latency = num(1).map(|l| l as u32);
            on_io_thread(f.host, |h| {
                let host = unsafe { &*h.0 };
                unsafe { (host.request_restart.unwrap())(h.0) };
            });
        }
        Some("restart-at") => f.restart_at = num(1).zip(num(2).map(|l| l as u32)),
        Some("restart-fail-now") => {
            f.refuse_activate = true;
            on_io_thread(f.host, |h| {
                let host = unsafe { &*h.0 };
                unsafe { (host.request_restart.unwrap())(h.0) };
            });
        }
        Some("rescan-every-block") => f.rescan_every_block = true,
        Some(which @ ("rescan-values" | "rescan-all")) => {
            f.gain = parts.get(1).and_then(|x| x.parse::<f64>().ok()).unwrap_or(1.0);
            let params = unsafe { host_ext::<clap_host_params>(f.host, CLAP_EXT_PARAMS).as_ref() }.expect("host params");
            let flags = if which == "rescan-all" { CLAP_PARAM_RESCAN_ALL } else { CLAP_PARAM_RESCAN_VALUES };
            unsafe { (params.rescan.unwrap())(f.host, flags) };
            note(f, &format!("cmd {cmd}"));
            return false;
        }
        _ => return false,
    }
    note(f, &format!("cmd {cmd}"));
    true
}

unsafe extern "C" fn init(p: *const clap_plugin) -> bool {
    let f = unsafe { fx(p) };
    // Exercise the host's extension lookup + log callback.
    let host = unsafe { &*f.host };
    if let Some(ge) = host.get_extension {
        let log = unsafe { ge(f.host, CLAP_EXT_LOG.as_ptr()) } as *const clap_host_log;
        if let Some(l) = unsafe { log.as_ref() }.and_then(|l| l.log) {
            unsafe { l(f.host, CLAP_LOG_INFO, c"fixture init".as_ptr()) };
        }
        let none = unsafe { ge(f.host, c"clap.does-not-exist".as_ptr()) };
        if !none.is_null() {
            return false;
        }
    }
    true
}

unsafe extern "C" fn destroy(p: *const clap_plugin) {
    assert!(!unsafe { fx(p) }.gui_created, "plugin destroyed with its GUI alive");
    let data = unsafe { (*p).plugin_data } as *mut Fx;
    drop(unsafe { Box::from_raw(data) });
}

unsafe extern "C" fn activate(p: *const clap_plugin, sr: f64, min: u32, max: u32) -> bool {
    let f = unsafe { fx(p) };
    if f.active || !(sr > 0.0) || min == 0 || max < min {
        return false;
    }
    if f.refuse_activate {
        note(f, "activate refused");
        return false;
    }
    f.active = true;
    note(f, "activate");
    if let Some(l) = f.next_latency.take() {
        // CLAP: the latency may change only while the plugin is (being) activated.
        f.latency = l;
        let lat = unsafe { host_ext::<clap_host_latency>(f.host, CLAP_EXT_LATENCY).as_ref() }.expect("host latency");
        unsafe { (lat.changed.unwrap())(f.host) };
        note(f, "latency.changed");
    }
    true
}

unsafe extern "C" fn deactivate(p: *const clap_plugin) {
    let f = unsafe { fx(p) };
    assert!(!f.processing, "deactivate while processing");
    f.active = false;
    note(f, "deactivate");
}

unsafe extern "C" fn start_processing(p: *const clap_plugin) -> bool {
    let f = unsafe { fx(p) };
    assert!(f.active, "start_processing while inactive");
    f.processing = true;
    note(f, "start_processing");
    true
}

unsafe extern "C" fn stop_processing(p: *const clap_plugin) {
    let f = unsafe { fx(p) };
    f.processing = false;
    note(f, "stop_processing");
}

unsafe extern "C" fn reset(p: *const clap_plugin) {
    let f = unsafe { fx(p) };
    f.level = 0.0;
    note(f, "reset");
}

fn apply(f: &mut Fx, h: &clap_event_header) {
    if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return;
    }
    match h.type_ {
        CLAP_EVENT_PARAM_VALUE => {
            let e = unsafe { &*(h as *const clap_event_header as *const clap_event_param_value) };
            match e.param_id {
                7 => f.gain = e.value,
                3 => f.mode = e.value,
                _ => {}
            }
        }
        CLAP_EVENT_NOTE_ON => {
            let e = unsafe { &*(h as *const clap_event_header as *const clap_event_note) };
            f.level = e.velocity;
        }
        CLAP_EVENT_NOTE_OFF => f.level = 0.0,
        CLAP_EVENT_MIDI => {
            let e = unsafe { &*(h as *const clap_event_header as *const clap_event_midi) };
            if e.data[0] & 0xF0 == 0xB0 && e.data[1] == 7 {
                f.level = f64::from(e.data[2]) / 127.0;
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn process(p: *const clap_plugin, pr: *const clap_process) -> clap_process_status {
    let f = unsafe { fx(p) };
    let pr = unsafe { &*pr };
    assert!(f.processing, "process while not processing");
    let n = pr.frames_count as usize;
    let ev = unsafe { &*pr.in_events };
    let count = unsafe { (ev.size.unwrap())(pr.in_events) };
    if pr.audio_outputs_count != 1 {
        return CLAP_PROCESS_ERROR;
    }
    let out: &clap_audio_buffer = unsafe { &*pr.audio_outputs };
    let input: Option<&clap_audio_buffer> = if f.synth {
        if pr.audio_inputs_count != 0 {
            return CLAP_PROCESS_ERROR;
        }
        None
    } else {
        if pr.audio_inputs_count != 1 {
            return CLAP_PROCESS_ERROR;
        }
        Some(unsafe { &*pr.audio_inputs })
    };
    if f.gui_edit_pending {
        f.gui_edit_pending = false;
        f.gain = 0.25;
        let e = clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: 7,
            cookie: std::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.25,
        };
        let out = unsafe { &*pr.out_events };
        assert!(unsafe { (out.try_push.unwrap())(pr.out_events, &e.header) });
        // Other event types are accepted and ignored by the host.
        let note = clap_event_note {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_note>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_END,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 0.0,
        };
        assert!(unsafe { (out.try_push.unwrap())(pr.out_events, &note.header) });
    }
    f.processes += 1;
    if f.rescan_every_block {
        // The host only records the request; it services it on its main thread.
        let params = unsafe { host_ext::<clap_host_params>(f.host, CLAP_EXT_PARAMS).as_ref() }.expect("host params");
        unsafe { (params.rescan.unwrap())(f.host, CLAP_PARAM_RESCAN_VALUES) };
    }
    note(f, "process");
    if let Some((at, latency)) = f.restart_at
        && at == f.processes
    {
        f.restart_at = None;
        f.next_latency = Some(latency);
        let host = unsafe { &*f.host };
        unsafe { (host.request_restart.unwrap())(f.host) };
        note(f, "request_restart");
    }
    if f.events {
        let mut rec: Vec<f32> = Vec::new();
        let mut last_time = 0u32;
        for k in 0..count {
            let h = unsafe { &*(ev.get.unwrap())(pr.in_events, k) };
            assert!(h.time >= last_time, "events not sorted");
            assert!((h.time as usize) < n, "event time outside the block");
            last_time = h.time;
            if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
                continue;
            }
            let (code, a, b) = match h.type_ {
                CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF => {
                    let e = unsafe { &*(h as *const clap_event_header as *const clap_event_note) };
                    (if h.type_ == CLAP_EVENT_NOTE_ON { 1.0 } else { 2.0 }, f32::from(e.channel) * 1000.0 + f32::from(e.key), e.note_id as f32)
                }
                CLAP_EVENT_NOTE_EXPRESSION => {
                    let e = unsafe { &*(h as *const clap_event_header as *const clap_event_note_expression) };
                    assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_PRESSURE);
                    (3.0, f32::from(e.channel) * 1000.0 + f32::from(e.key), (e.value * 1e6).round() as f32)
                }
                CLAP_EVENT_PARAM_VALUE => {
                    let e = unsafe { &*(h as *const clap_event_header as *const clap_event_param_value) };
                    (4.0, e.param_id as f32, (e.value * 1e6).round() as f32)
                }
                CLAP_EVENT_MIDI => {
                    let e = unsafe { &*(h as *const clap_event_header as *const clap_event_midi) };
                    (5.0, (u32::from(e.data[0]) << 16 | u32::from(e.data[1]) << 8 | u32::from(e.data[2])) as f32, 0.0)
                }
                _ => continue,
            };
            rec.extend([code, h.time as f32, a, b]);
        }
        for c in 0..out.channel_count as usize {
            let o = unsafe { std::slice::from_raw_parts_mut(*out.data32.add(c), n) };
            o.fill(0.0);
            for (d, r) in o.iter_mut().zip(&rec) {
                *d = *r;
            }
        }
        return CLAP_PROCESS_CONTINUE;
    }
    let mut next = 0u32;
    let mut last_time = 0u32;
    for i in 0..n {
        while next < count {
            let h = unsafe { &*(ev.get.unwrap())(pr.in_events, next) };
            assert!(h.time >= last_time, "events not sorted");
            assert!((h.time as usize) < n, "event time outside the block");
            if h.time as usize > i {
                break;
            }
            last_time = h.time;
            apply(f, h);
            next += 1;
        }
        for c in 0..out.channel_count as usize {
            let o = unsafe { *out.data32.add(c) };
            let v = match input {
                Some(inp) => {
                    let x = unsafe { *(*inp.data32.add(c.min(inp.channel_count as usize - 1))).add(i) };
                    x * f.gain as f32
                }
                None => f.level as f32,
            };
            unsafe { *o.add(i) = v };
        }
    }
    if f.requests { CLAP_PROCESS_SLEEP } else { CLAP_PROCESS_CONTINUE }
}

// ---- extensions ----

unsafe extern "C" fn params_count(p: *const clap_plugin) -> u32 {
    let f = unsafe { fx(p) };
    if f.events {
        1
    } else if f.synth {
        0
    } else {
        3
    }
}

fn write_name(dst: &mut [c_char], s: &CStr) {
    for (d, b) in dst.iter_mut().zip(s.to_bytes_with_nul()) {
        *d = *b as c_char;
    }
}

unsafe extern "C" fn params_get_info(p: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    let events = unsafe { fx(p) }.events;
    let info = unsafe { &mut *info };
    let (id, name, flags, min, max, def): (clap_id, &CStr, u32, f64, f64, f64) = match index {
        0 if events => (5, c"Probe", CLAP_PARAM_IS_AUTOMATABLE, 0.0, 1.0, 0.0),
        0 => (7, c"Gain", CLAP_PARAM_IS_AUTOMATABLE, 0.0, 2.0, 1.0),
        1 => (3, c"Mode", CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM, 0.0, 2.0, 0.0),
        2 => (9, c"Secret", CLAP_PARAM_IS_HIDDEN, 0.0, 1.0, 0.0),
        _ => return false,
    };
    info.id = id;
    info.flags = flags;
    info.cookie = std::ptr::null_mut();
    write_name(&mut info.name, name);
    write_name(&mut info.module, c"");
    info.min_value = min;
    info.max_value = max;
    info.default_value = def;
    true
}

unsafe extern "C" fn params_get_value(p: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
    let f = unsafe { fx(p) };
    let v = match id {
        7 => f.gain,
        3 => f.mode,
        9 | 5 => 0.0,
        _ => return false,
    };
    unsafe { *out = v };
    true
}

unsafe extern "C" fn params_value_to_text(_p: *const clap_plugin, id: clap_id, v: f64, buf: *mut c_char, cap: u32) -> bool {
    if id != 3 || cap < 2 {
        return false;
    }
    let label = [b'A', b'B', b'C'].get(v.round() as usize).copied().unwrap_or(b'?');
    unsafe {
        *buf = label as c_char;
        *buf.add(1) = 0;
    }
    true
}

unsafe extern "C" fn params_flush(p: *const clap_plugin, inp: *const clap_input_events, _out: *const clap_output_events) {
    let f = unsafe { fx(p) };
    let ev = unsafe { &*inp };
    let n = unsafe { (ev.size.unwrap())(inp) };
    for i in 0..n {
        apply(f, unsafe { &*(ev.get.unwrap())(inp, i) });
    }
    note(f, "flush");
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: None,
    flush: Some(params_flush),
};

unsafe extern "C" fn ports_count(p: *const clap_plugin, is_input: bool) -> u32 {
    if is_input && unsafe { fx(p) }.synth { 0 } else { 1 }
}

unsafe extern "C" fn ports_get(p: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_audio_port_info) -> bool {
    if index != 0 || (is_input && unsafe { fx(p) }.synth) {
        return false;
    }
    let info = unsafe { &mut *info };
    info.id = if is_input { 0 } else { 1 };
    write_name(&mut info.name, c"main");
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = clap_sys::id::CLAP_INVALID_ID;
    true
}

static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) };

unsafe extern "C" fn latency_get(p: *const clap_plugin) -> u32 {
    unsafe { fx(p) }.latency
}

static LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(latency_get) };

unsafe extern "C" fn notes_count(p: *const clap_plugin, is_input: bool) -> u32 {
    if is_input && unsafe { fx(p) }.synth { 1 } else { 0 }
}

unsafe extern "C" fn notes_get(_p: *const clap_plugin, index: u32, is_input: bool, info: *mut clap_note_port_info) -> bool {
    if index != 0 || !is_input {
        return false;
    }
    let info = unsafe { &mut *info };
    info.id = 0;
    info.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
    info.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    write_name(&mut info.name, c"notes");
    true
}

static NOTES: clap_plugin_note_ports = clap_plugin_note_ports { count: Some(notes_count), get: Some(notes_get) };

/// The requests plugin's "state": its log as text (see the crate docs).
fn requests_report(f: &Fx) -> String {
    let log = f.log.lock().unwrap();
    let entries: Vec<String> = log.iter().map(|(w, t)| format!("{w}@{t}")).collect();
    format!("gain={};latency={};processes={};me={};log={}", f.gain, f.latency, f.processes, thread_token(), entries.join(","))
}

unsafe extern "C" fn state_save(p: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let f = unsafe { fx(p) };
    if f.requests {
        let text = requests_report(f);
        let s = unsafe { &*stream };
        let n = unsafe { (s.write.unwrap())(stream, text.as_ptr().cast(), text.len() as u64) };
        return n == text.len() as i64;
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&f.gain.to_le_bytes());
    bytes[8..].copy_from_slice(&f.mode.to_le_bytes());
    let s = unsafe { &*stream };
    // Write in two pieces to exercise appending.
    let a = unsafe { (s.write.unwrap())(stream, bytes.as_ptr().cast(), 5) };
    let b = unsafe { (s.write.unwrap())(stream, bytes[5..].as_ptr().cast(), 11) };
    a == 5 && b == 11
}

unsafe extern "C" fn state_load(p: *const clap_plugin, stream: *const clap_istream) -> bool {
    let f = unsafe { fx(p) };
    let s = unsafe { &*stream };
    if f.requests {
        let mut buf = vec![0u8; 256];
        let n = unsafe { (s.read.unwrap())(stream, buf.as_mut_ptr().cast(), buf.len() as u64) };
        let text = String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned();
        return text.strip_prefix("CMD:").is_some_and(|c| requests_command(f, c));
    }
    let mut bytes = [0u8; 64];
    let mut got = 0usize;
    loop {
        let n = unsafe { (s.read.unwrap())(stream, bytes[got..].as_mut_ptr().cast(), (64 - got) as u64) };
        if n <= 0 {
            break;
        }
        got += n as usize;
        if got == 64 {
            break;
        }
    }
    // The gain plugin takes one command too: `CMD:rescan-values:<gain>`.
    if let Some(cmd) = std::str::from_utf8(&bytes[..got]).ok().and_then(|t| t.strip_prefix("CMD:"))
        && cmd.starts_with("rescan-")
    {
        return requests_command(f, &cmd.to_string());
    }
    if got != 16 {
        return false;
    }
    let gain = f64::from_le_bytes(bytes[..8].try_into().unwrap());
    let mode = f64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if !(0.0..=2.0).contains(&gain) || !(0.0..=2.0).contains(&mode) {
        return false;
    }
    f.gain = gain;
    f.mode = mode;
    true
}

static STATE: clap_plugin_state = clap_plugin_state { save: Some(state_save), load: Some(state_load) };

unsafe extern "C" fn gui_is_api_supported(_p: *const clap_plugin, api: *const c_char, floating: bool) -> bool {
    assert!(!api.is_null());
    floating
}

unsafe extern "C" fn gui_create(p: *const clap_plugin, _api: *const c_char, floating: bool) -> bool {
    let f = unsafe { fx(p) };
    assert!(floating && !f.gui_created, "create");
    f.gui_created = true;
    true
}

unsafe extern "C" fn gui_destroy(p: *const clap_plugin) {
    let f = unsafe { fx(p) };
    assert!(f.gui_created, "destroy without create");
    f.gui_created = false;
    f.gui_visible = false;
}

unsafe extern "C" fn gui_set_scale(_p: *const clap_plugin, scale: f64) -> bool {
    scale > 0.0
}

unsafe extern "C" fn gui_suggest_title(p: *const clap_plugin, title: *const c_char) {
    assert!(unsafe { fx(p) }.gui_created);
    assert!(!unsafe { CStr::from_ptr(title) }.to_bytes().is_empty());
}

unsafe extern "C" fn gui_show(p: *const clap_plugin) -> bool {
    let f = unsafe { fx(p) };
    assert!(f.gui_created, "show without create");
    f.gui_visible = true;
    f.gui_edit_pending = true;
    // Exercise the host GUI extension.
    let host = unsafe { &*f.host };
    let hg = unsafe { (host.get_extension.unwrap())(f.host, CLAP_EXT_GUI.as_ptr()) } as *const clap_host_gui;
    let hg = unsafe { hg.as_ref() }.expect("host gui extension");
    assert!(unsafe { (hg.request_resize.unwrap())(f.host, 300, 200) });
    true
}

unsafe extern "C" fn gui_hide(p: *const clap_plugin) -> bool {
    unsafe { fx(p) }.gui_visible = false;
    true
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_is_api_supported),
    get_preferred_api: None,
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: None,
    can_resize: None,
    get_resize_hints: None,
    adjust_size: None,
    set_size: None,
    set_parent: None,
    set_transient: None,
    suggest_title: Some(gui_suggest_title),
    show: Some(gui_show),
    hide: Some(gui_hide),
};

unsafe extern "C" fn get_extension(p: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    let synth = unsafe { fx(p) }.synth;
    if unsafe { fx(p) }.requests {
        return if id == CLAP_EXT_PARAMS {
            &PARAMS as *const _ as *const c_void
        } else if id == CLAP_EXT_AUDIO_PORTS {
            &PORTS as *const _ as *const c_void
        } else if id == CLAP_EXT_LATENCY {
            &LATENCY as *const _ as *const c_void
        } else if id == CLAP_EXT_STATE {
            &STATE as *const _ as *const c_void
        } else {
            std::ptr::null()
        };
    }
    if id == CLAP_EXT_PARAMS {
        &PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY && !synth {
        &LATENCY as *const _ as *const c_void
    } else if id == CLAP_EXT_NOTE_PORTS && synth {
        &NOTES as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE && !synth {
        &STATE as *const _ as *const c_void
    } else if id == CLAP_EXT_GUI && !synth {
        &GUI as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn on_main_thread(p: *const clap_plugin) {
    note(unsafe { fx(p) }, "on_main_thread");
}

// ---- factory + entry ----

/// The plug-ins this binary lists: the gain and the synth, or (feature `requests`) only the
/// requests plug-in, built as a binary of its own so that tests counting the default fixture's
/// plug-ins (here and in other crates) are unaffected.
#[cfg(not(any(feature = "requests", feature = "events")))]
static DESCS: [&clap_plugin_descriptor; 2] = [&GAIN_DESC, &SYNTH_DESC];
#[cfg(feature = "requests")]
static DESCS: [&clap_plugin_descriptor; 1] = [&REQUESTS_DESC];
#[cfg(all(feature = "events", not(feature = "requests")))]
static DESCS: [&clap_plugin_descriptor; 1] = [&EVENTS_DESC];

unsafe extern "C" fn factory_count(_f: *const clap_plugin_factory) -> u32 {
    DESCS.len() as u32
}

unsafe extern "C" fn factory_desc(_f: *const clap_plugin_factory, i: u32) -> *const clap_plugin_descriptor {
    DESCS.get(i as usize).map_or(std::ptr::null(), |d| *d as *const _)
}

unsafe extern "C" fn factory_create(_f: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    let id = unsafe { CStr::from_ptr(id) };
    let (desc, synth, requests, events) = if id == c"org.soundcraft.test.gain" {
        (&GAIN_DESC as *const _, false, false, false)
    } else if id == c"org.soundcraft.test.synth" {
        (&SYNTH_DESC as *const _, true, false, false)
    } else if id == c"org.soundcraft.test.requests" {
        (&REQUESTS_DESC as *const _, false, true, false)
    } else if id == c"org.soundcraft.test.events" {
        (&EVENTS_DESC as *const _, true, false, true)
    } else {
        return std::ptr::null();
    };
    let b = Box::new(Fx {
        plugin: clap_plugin {
            desc,
            plugin_data: std::ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        host,
        synth,
        requests,
        events,
        latency: if requests { 5 } else { 3 },
        next_latency: None,
        restart_at: None,
        refuse_activate: false,
        rescan_every_block: false,
        processes: 0,
        log: std::sync::Mutex::new(Vec::new()),
        gain: 1.0,
        mode: 0.0,
        level: 0.0,
        active: false,
        processing: false,
        gui_created: false,
        gui_visible: false,
        gui_edit_pending: false,
    });
    let raw = Box::into_raw(b);
    unsafe {
        (*raw).plugin.plugin_data = raw as *mut c_void;
        &(*raw).plugin
    }
}

static FACTORY: clap_plugin_factory =
    clap_plugin_factory { get_plugin_count: Some(factory_count), get_plugin_descriptor: Some(factory_desc), create_plugin: Some(factory_create) };

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}
unsafe extern "C" fn entry_deinit() {}
unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID { &FACTORY as *const _ as *const c_void } else { std::ptr::null() }
}

#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry =
    clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(entry_deinit), get_factory: Some(entry_get_factory) };
