//! Test-only VST3 plugin for soundcraft-vst3-host (two audio module classes):
//!
//! - "Fixture Gain" (`Fx`): stereo or mono in → out × gain, with a separate edit controller
//!   (connected through IConnectionPoint, synced through the component state). Params: 7 "Gain"
//!   (plain 0..2, linear, units dB), 3 "Mode" (3 steps: A/B/C), 12 "Freq" (20..20000 Hz, log),
//!   9 hidden, 11 bypass. Starts at gain 0.5 (its state) although the declared default is 1.0.
//!   Reports 3 samples of latency. State: the component stores the gain (one LE f64), the
//!   controller the Freq value (one LE f64). Its controller has an editor view (`IPlugView`,
//!   320×240, no real drawing) supporting NSView/HWND/X11.
//! - "Fixture Synth" (`Instrument|Synth`): a single-component instrument (the component is also
//!   its edit controller). No audio input, one stereo output, one event input; every output
//!   sample is the velocity of the held note (sample-accurate), 0 when none. Poly pressure, and
//!   MIDI CC 7 through `IMidiMapping` (hidden parameter 1), set it to their value too.
//!   For the host-request tests (audit F08, ADR 0012 G7) it takes commands as a component state
//!   to load: `LEARN:<cc>` maps that controller to the volume instead (a "MIDI learn") and calls
//!   `restartComponent(kMidiCCAssignmentChanged)`; `LATENCY:<n>` reports `n` samples after the
//!   next `setActive(true)` and calls `restartComponent(kLatencyChanged)`; `RESTART:<flags>`
//!   calls `restartComponent` with those flags; `REFUSE` makes `setActive(true)` fail from then
//!   on; `EVERY:<command>` runs the command at every `getState`; `ARM:<command>` runs the command
//!   at the next `getState` instead (a main-thread call the host makes while the processor runs elsewhere,
//!   like a learn in the plug-in's own window). Its component state is a report: the OS thread of
//!   the `getState` call (`me=`) and the threads that ran `getMidiControllerAssignment`,
//!   `setActive`, `process` and `getLatencySamples`.
//!
//! Gain's component talks to its controller the way iZotope's plug-ins do (and the SDK's
//! `ComponentBase::allocateMessage`): when connected it allocates an `IMessage` through the host
//! context's `IHostApplication::createInstance`, fills its attribute list, and `notify`s the
//! controller, which checks every value; it also allocates a bare `IAttributeList` there. A host
//! that cannot create them, or a controller that never got the message by the time the host sets
//! its component handler, is a host contract violation.
//!
//! Probe mode (`VST3_FIXTURE_PROBES` set; see "probe mode" below) checks VST3's edit-controller
//! threading rule and reports the Freq values Gain's processor receives.
//!
//! The module checks the host's module lifecycle the way real plugins depend on it (see "module
//! lifecycle" below) and aborts the process when it is broken.
//!
//! Test code: panics are fine here, the host must survive anything anyway.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComRef, ComWrapper, uid};

const GAIN_CID: TUID = uid(0x5C0A1D00, 0x11112222, 0x33334444, 0x55556666);
const GAIN_CTRL_CID: TUID = uid(0x5C0A1D01, 0x11112222, 0x33334444, 0x55556666);
const SYNTH_CID: TUID = uid(0x5C0A1D02, 0x11112222, 0x33334444, 0x55556666);
const EVENTS_CID: TUID = uid(0x5C0A1D03, 0x11112222, 0x33334444, 0x55556666);

fn copy_c8(src: &str, dst: &mut [c_char]) {
    for (d, s) in dst.iter_mut().zip(src.bytes().chain(std::iter::once(0))) {
        *d = s as c_char;
    }
}

fn copy_c16(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (d, s) in dst.iter_mut().zip(src.encode_utf16()) {
        *d = s;
        n += 1;
    }
    if let Some(z) = dst.get_mut(n) {
        *z = 0;
    }
}

fn audio_bus(bus: *mut BusInfo, dir: BusDirection, name: &str) {
    let b = unsafe { &mut *bus };
    b.mediaType = MediaTypes_::kAudio as MediaType;
    b.direction = dir;
    b.channelCount = 2;
    copy_c16(name, &mut b.name);
    b.busType = BusTypes_::kMain as BusType;
    b.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
}

unsafe fn channels<'a>(bus: &AudioBusBuffers, frames: usize) -> Vec<&'a mut [f32]> {
    let n = bus.numChannels.max(0) as usize;
    let table = unsafe { std::slice::from_raw_parts(bus.__field0.channelBuffers32, n) };
    table.iter().map(|&p| unsafe { std::slice::from_raw_parts_mut(p, frames) }).collect()
}

// ---- gain ----------------------------------------------------------------------------------

const HELLO_ID: &std::ffi::CStr = c"FixtureHello";
const HELLO_BLOB: [u8; 5] = [0, 1, 2, 0xfe, 0xff];

/// A host object of class (and interface) `I`, as `IHostApplication::createInstance` makes it.
fn host_create<I: vst3::Interface>(host: &ComRef<'_, IHostApplication>) -> Option<vst3::ComPtr<I>> {
    let mut iid = I::IID.map(|b| b as c_char);
    let mut obj: *mut c_void = std::ptr::null_mut();
    // SAFETY: the host is live, both IDs are readable TUIDs, and obj is a distinct writable output.
    let r = unsafe { host.createInstance(&mut iid, &mut iid, &mut obj) };
    if r != kResultOk && r != kResultTrue {
        return None;
    }
    // SAFETY: successful SDK creation returns the requested interface with one owned reference.
    unsafe { vst3::ComPtr::from_raw(obj.cast::<I>()) }
}

/// The component's greeting to its controller (see the module docs).
fn say_hello(host: &vst3::ComPtr<IHostApplication>, peer: &ComRef<'_, IConnectionPoint>) {
    let host = host.as_com_ref();
    let Some(list) = host_create::<IAttributeList>(&host) else { violation("host createInstance made no IAttributeList") };
    // SAFETY: list is owned here; IDs are terminated literals and n is a writable scalar.
    unsafe {
        assert_eq!(list.setInt(c"n".as_ptr(), -7), kResultOk);
        let mut n = 0i64;
        assert_eq!(list.getInt(c"n".as_ptr(), &mut n), kResultOk);
        assert_eq!(n, -7, "host attribute list int");
        assert_ne!(list.getInt(c"missing".as_ptr(), &mut n), kResultOk, "host attribute list: no such int");
    }
    let Some(msg) = host_create::<IMessage>(&host) else { violation("host createInstance made no IMessage for IConnectionPoint::notify") };
    // SAFETY: msg owns the borrowed attributes; all setter inputs remain live through their
    // copying calls. The live peer borrows msg only during synchronous notify (retention needs AddRef).
    unsafe {
        msg.setMessageID(HELLO_ID.as_ptr());
        let Some(attrs) = ComRef::from_raw(msg.getAttributes()) else { violation("host IMessage without attributes") };
        let name: Vec<TChar> = "Fixture".encode_utf16().chain(Some(0)).collect();
        assert_eq!(attrs.setInt(c"int".as_ptr(), 42), kResultOk);
        assert_eq!(attrs.setFloat(c"float".as_ptr(), 0.25), kResultOk);
        assert_eq!(attrs.setString(c"string".as_ptr(), name.as_ptr()), kResultOk);
        assert_eq!(attrs.setBinary(c"binary".as_ptr(), HELLO_BLOB.as_ptr().cast(), HELLO_BLOB.len() as u32), kResultOk);
        peer.notify(msg.as_ptr());
    }
}

/// Checks the component's greeting (see the module docs).
///
/// # Safety
/// message must be null or a live IMessage borrowed for this call, with no concurrent mutation.
unsafe fn hello_ok(message: *mut IMessage) -> bool {
    // SAFETY: notify forwards its live borrowed message; ComRef rejects null and does not release it.
    let Some(msg) = (unsafe { ComRef::from_raw(message) }) else { return false };
    // SAFETY: msg and its ID/attributes remain live and unmodified during notify. The output
    // buffers have the reported byte sizes; successful getBinary supplies size readable bytes.
    unsafe {
        let id = msg.getMessageID();
        if id.is_null() || std::ffi::CStr::from_ptr(id) != HELLO_ID {
            return false;
        }
        let Some(attrs) = ComRef::from_raw(msg.getAttributes()) else { return false };
        let (mut i, mut f) = (0i64, 0f64);
        let mut s = [0 as TChar; 4];
        let (mut data, mut size): (*const c_void, u32) = (std::ptr::null(), 0);
        attrs.getInt(c"int".as_ptr(), &mut i) == kResultOk
            && i == 42
            && attrs.getFloat(c"float".as_ptr(), &mut f) == kResultOk
            && f == 0.25
            // A 4-unit buffer truncates "Fixture" to "Fix" plus the terminator.
            && attrs.getString(c"string".as_ptr(), s.as_mut_ptr(), std::mem::size_of_val(&s) as u32) == kResultOk
            && s == [b'F' as TChar, b'i' as TChar, b'x' as TChar, 0]
            && attrs.getBinary(c"binary".as_ptr(), &mut data, &mut size) == kResultOk
            && size as usize == HELLO_BLOB.len()
            && std::slice::from_raw_parts(data.cast::<u8>(), size as usize) == HELLO_BLOB
            && attrs.getFloat(c"int".as_ptr(), &mut f) != kResultOk
    }
}

struct GainProcessor {
    /// Normalized gain (plain = 2 × normalized).
    gain: AtomicU64,
    arrangement: AtomicU64,
    /// The host context from `initialize` (alive while the component is initialized).
    host: AtomicPtr<FUnknown>,
    _live: Live,
}

impl Class for GainProcessor {
    type Interfaces = (IComponent, IAudioProcessor, IConnectionPoint);
}

impl IPluginBaseTrait for GainProcessor {
    unsafe fn initialize(&self, context: *mut FUnknown) -> tresult {
        assert!(!context.is_null(), "host context");
        self.host.store(context, Ordering::SeqCst);
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IConnectionPointTrait for GainProcessor {
    /// # Safety
    /// other must be null or a live connection point; this component must be initialized.
    unsafe fn connect(&self, other: *mut IConnectionPoint) -> tresult {
        // SAFETY: the SDK passes a live peer (or null, rejected); this call only borrows it.
        let Some(peer) = (unsafe { ComRef::from_raw(other) }) else { return kInvalidArgument };
        // SAFETY: initialize stored a host context that lives until terminate; cast owns an
        // additional reference, released after say_hello returns.
        let host = unsafe { ComRef::from_raw(self.host.load(Ordering::SeqCst)) }.and_then(|h| h.cast::<IHostApplication>());
        let Some(host) = host else { violation("the host context has no IHostApplication") };
        say_hello(&host, &peer);
        kResultOk
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }
    unsafe fn notify(&self, _message: *mut IMessage) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for GainProcessor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        unsafe { *class_id = GAIN_CTRL_CID };
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, _dir: BusDirection) -> i32 {
        if media == MediaTypes_::kAudio as MediaType { 1 } else { 0 }
    }
    unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
        if media != MediaTypes_::kAudio as MediaType || index != 0 {
            return kInvalidArgument;
        }
        audio_bus(bus, dir, "Main");
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, _i: i32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = [0u8; 8];
        let mut n = 0;
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) } != kResultOk || n != 8 {
            return kResultFalse;
        }
        let g = f64::from_le_bytes(bytes);
        if !(0.0..=1.0).contains(&g) {
            return kResultFalse;
        }
        self.gain.store(g.to_bits(), Ordering::Relaxed);
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = f64::from_bits(self.gain.load(Ordering::Relaxed)).to_le_bytes();
        let mut n = 0;
        unsafe { s.write(bytes.as_mut_ptr().cast(), 8, &mut n) }
    }
}

impl IAudioProcessorTrait for GainProcessor {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, n_in: i32, outputs: *mut SpeakerArrangement, n_out: i32) -> tresult {
        if n_in != 1 || n_out != 1 {
            return kResultFalse;
        }
        let (i, o) = unsafe { (*inputs, *outputs) };
        if i != o || (i != SpeakerArr::kStereo && i != SpeakerArr::kMono) {
            return kResultFalse;
        }
        self.arrangement.store(i, Ordering::Relaxed);
        kResultTrue
    }
    unsafe fn getBusArrangement(&self, _dir: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        unsafe { *arr = self.arrangement.load(Ordering::Relaxed) };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 as i32 { kResultOk } else { kNotImplemented }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        3
    }
    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let s = unsafe { &*setup };
        assert_eq!(s.symbolicSampleSize, SymbolicSampleSizes_::kSample32 as i32);
        assert!(s.maxSamplesPerBlock > 0 && s.sampleRate > 0.0);
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kNotImplemented
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let d = unsafe { &*data };
        assert!(!d.processContext.is_null());
        let probes = probes();
        if probes {
            note_process_thread();
        }
        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                let Some(q) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else { continue };
                let (mut off, mut v) = (0, 0.0);
                let n = unsafe { q.getPointCount() };
                let id = unsafe { q.getParameterId() };
                if n > 0 && unsafe { q.getPoint(n - 1, &mut off, &mut v) } == kResultOk {
                    if id == 7 {
                        self.gain.store(v.to_bits(), Ordering::Relaxed);
                    } else if id == 12 {
                        LAST_FREQ.store(v.to_bits(), Ordering::Relaxed);
                    } else if id == FINE {
                        LAST_FINE.store(v.to_bits(), Ordering::Relaxed);
                    }
                }
            }
        }
        if d.numInputs != 1 || d.numOutputs != 1 {
            return kResultOk;
        }
        let frames = d.numSamples.max(0) as usize;
        let g = 2.0 * f64::from_bits(self.gain.load(Ordering::Relaxed)) as f32;
        let (ins, outs) = unsafe { (channels(&*d.inputs, frames), channels(&*d.outputs, frames)) };
        for (c, (i, o)) in ins.iter().zip(outs).enumerate() {
            if probes && c == 1 {
                if many() > 0 {
                    // See `many`: controller calls so far, then the last Fine point.
                    o.fill(0.0);
                    if let [a, b, ..] = o {
                        *a = CTRL_CALLS.load(Ordering::Relaxed) as f32;
                        *b = f64::from_bits(LAST_FINE.load(Ordering::Relaxed)) as f32;
                    }
                } else {
                    o.fill(f64::from_bits(LAST_FREQ.load(Ordering::Relaxed)) as f32);
                }
                continue;
            }
            for (x, y) in i.iter().zip(o.iter_mut()) {
                *y = g * x;
            }
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

/// The host's component handler, carried to a plug-in thread (the test keeps the plug-in alive).
#[derive(Clone, Copy)]
struct HandlerPtr(*mut IComponentHandler);
unsafe impl Send for HandlerPtr {}

struct GainController {
    gain: AtomicU64,
    freq: AtomicU64,
    /// A gain (normalized, f64 bits) the controller returns to at every `getState` where its gain
    /// differs, as a preset in the plug-in's own window would, telling the host with
    /// `restartComponent(kParamValuesChanged)`; `u64::MAX`: none. Set by a controller state that
    /// carries a second f64 after the Freq value.
    preset: AtomicU64,
    /// The host's component handler for this controller.
    handler: AtomicPtr<IComponentHandler>,
    /// The component's greeting arrived (see the module docs).
    hello: AtomicBool,
    _live: Live,
}

struct View {
    attached: std::sync::atomic::AtomicBool,
    _live: Live,
}

impl Class for View {
    type Interfaces = (IPlugView,);
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, t: FIDString) -> tresult {
        let t = unsafe { std::ffi::CStr::from_ptr(t) }.to_bytes();
        if t == b"NSView" || t == b"HWND" || t == b"X11EmbedWindowID" { kResultTrue } else { kResultFalse }
    }
    unsafe fn attached(&self, parent: *mut c_void, _t: FIDString) -> tresult {
        if parent.is_null() {
            return kInvalidArgument;
        }
        self.attached.store(true, Ordering::Relaxed);
        kResultOk
    }
    unsafe fn removed(&self) -> tresult {
        assert!(self.attached.swap(false, Ordering::Relaxed), "removed without attached");
        kResultOk
    }
    unsafe fn onWheel(&self, _d: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _k: char16, _c: i16, _m: i16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _k: char16, _c: i16, _m: i16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        let r = unsafe { &mut *size };
        *r = ViewRect { left: 0, top: 0, right: 320, bottom: 240 };
        kResultOk
    }
    unsafe fn onSize(&self, _s: *mut ViewRect) -> tresult {
        kResultOk
    }
    unsafe fn onFocus(&self, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _f: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }
    unsafe fn checkSizeConstraint(&self, _r: *mut ViewRect) -> tresult {
        kResultOk
    }
}

impl Class for GainController {
    type Interfaces = (IEditController, IConnectionPoint);
}

impl IPluginBaseTrait for GainController {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IConnectionPointTrait for GainController {
    unsafe fn connect(&self, other: *mut IConnectionPoint) -> tresult {
        if other.is_null() { kInvalidArgument } else { kResultOk }
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }
    /// # Safety
    /// message must be null or a live IMessage borrowed for this call, with no concurrent mutation.
    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        // SAFETY: forwards the SDK notify borrow; hello_ok does not retain the message or its data.
        if !unsafe { hello_ok(message) } {
            violation("the component's IMessage reached the controller garbled");
        }
        self.hello.store(true, Ordering::SeqCst);
        kResultOk
    }
}

const GAIN_PARAMS: [(u32, &str, &str, i32, f64, i32); 5] = [
    (7, "Gain", "dB", 0, 0.5, ParameterInfo_::ParameterFlags_::kCanAutomate),
    (9, "Secret", "", 0, 0.0, ParameterInfo_::ParameterFlags_::kIsHidden),
    (3, "Mode", "", 2, 0.0, ParameterInfo_::ParameterFlags_::kIsList),
    (11, "Bypass", "", 1, 0.0, ParameterInfo_::ParameterFlags_::kIsBypass),
    (12, "Freq", "Hz", 0, 0.5, 0),
];

/// With `VST3_FIXTURE_PARAMS=N` (N > 0), Gain's controller also exposes N continuous parameters
/// (ids 1000.., plain `1 + 9 n²`) and "Fine" (id 2000, 300 steps, plain = step index), and its
/// processor reports the number of edit-controller calls made so far and the last Fine value it
/// received (normalized) in the first two samples of the second output channel. For the
/// large-plug-in tests of the host's lazy curve sampling.
fn many() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VST3_FIXTURE_PARAMS").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Parameter `index` of Gain's controller: (id, title, units, stepCount, default, flags).
fn gain_param(index: usize) -> Option<(u32, String, &'static str, i32, f64, i32)> {
    if let Some(&(id, title, units, steps, def, flags)) = GAIN_PARAMS.get(index) {
        return Some((id, title.to_string(), units, steps, def, flags));
    }
    let k = index - GAIN_PARAMS.len();
    let automate = ParameterInfo_::ParameterFlags_::kCanAutomate;
    match k.cmp(&many()) {
        std::cmp::Ordering::Less => Some((1000 + k as u32, format!("Extra {k}"), "", 0, 0.5, automate)),
        std::cmp::Ordering::Equal if many() > 0 => Some((FINE, "Fine".to_string(), "", FINE_STEPS, 0.5, automate)),
        _ => None,
    }
}

/// The 300-step parameter.
const FINE: u32 = 2000;
const FINE_STEPS: i32 = 300;
static FINE_VALUE: AtomicU64 = AtomicU64::new(0);
static LAST_FINE: AtomicU64 = AtomicU64::new(0);
/// Edit-controller calls made so far (any thread).
static CTRL_CALLS: AtomicU64 = AtomicU64::new(0);

/// Gain's normalized → plain conversion (Freq follows the current shape).
fn gain_to_plain(id: u32, n: f64) -> f64 {
    match id {
        7 => 2.0 * n,
        3 => (n * 2.0).round(),
        12 if FREQ_SQUARE.load(Ordering::SeqCst) => 20.0 + 19_980.0 * n * n,
        12 => 20.0 * 1000f64.powf(n),
        FINE => (n * f64::from(FINE_STEPS)).round(),
        1000..FINE => 1.0 + 9.0 * n * n,
        _ => n,
    }
}

/// Gain's plain → normalized conversion (Freq follows the current shape).
fn gain_to_normalized(id: u32, v: f64) -> f64 {
    match id {
        7 => v / 2.0,
        3 => v / 2.0,
        12 if FREQ_SQUARE.load(Ordering::SeqCst) => ((v - 20.0) / 19_980.0).max(0.0).sqrt(),
        12 => (v / 20.0).ln() / 1000f64.ln(),
        FINE => v / f64::from(FINE_STEPS),
        1000..FINE => ((v - 1.0) / 9.0).max(0.0).sqrt(),
        _ => v,
    }
}

impl IEditControllerTrait for GainController {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        ctrl_call("setComponentState");
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = [0u8; 8];
        let mut n = 0;
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) } == kResultOk && n == 8 {
            self.gain.store(f64::from_le_bytes(bytes).to_bits(), Ordering::Relaxed);
        }
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        ctrl_call("setState");
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = [0u8; 8];
        let mut n = 0;
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) } == kResultOk && n == 8 {
            self.freq.store(f64::from_le_bytes(bytes).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) } == kResultOk && n == 8 {
            self.preset.store(f64::from_le_bytes(bytes).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        // A third f64: after that many ms, a thread of the plug-in's own tells the host its values
        // changed (with nothing changed), outside any host call.
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 8, &mut n) } == kResultOk && n == 8 {
            let ms = f64::from_le_bytes(bytes).clamp(0.0, 10_000.0) as u64;
            let h = HandlerPtr(self.handler.load(Ordering::SeqCst));
            std::thread::spawn(move || {
                let h = h;
                std::thread::sleep(std::time::Duration::from_millis(ms));
                if let Some(h) = unsafe { ComRef::from_raw(h.0) } {
                    unsafe { h.restartComponent(RestartFlags_::kParamValuesChanged as i32) };
                }
            });
        }
        // Like many plug-ins, it tells the host its values changed while it loads a state.
        if let Some(h) = unsafe { ComRef::from_raw(self.handler.load(Ordering::SeqCst)) } {
            unsafe { h.restartComponent(RestartFlags_::kParamValuesChanged as i32) };
        }
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        ctrl_call("getState");
        let preset = self.preset.load(Ordering::Relaxed);
        if preset != u64::MAX && self.gain.swap(preset, Ordering::Relaxed) != preset {
            if let Some(h) = unsafe { ComRef::from_raw(self.handler.load(Ordering::SeqCst)) } {
                unsafe { h.restartComponent(RestartFlags_::kParamValuesChanged as i32) };
            }
        }
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = f64::from_bits(self.freq.load(Ordering::Relaxed)).to_le_bytes();
        let mut n = 0;
        unsafe { s.write(bytes.as_mut_ptr().cast(), 8, &mut n) }
    }
    unsafe fn getParameterCount(&self) -> i32 {
        ctrl_call("getParameterCount");
        (GAIN_PARAMS.len() + if many() > 0 { many() + 1 } else { 0 }) as i32
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        ctrl_call("getParameterInfo");
        let Some((id, title, units, steps, def, flags)) = gain_param(index as usize) else { return kInvalidArgument };
        let i = unsafe { &mut *info };
        i.id = id;
        copy_c16(&title, &mut i.title);
        copy_c16(&title, &mut i.shortTitle);
        copy_c16(units, &mut i.units);
        i.stepCount = steps;
        i.defaultNormalizedValue = def;
        i.unitId = 0;
        i.flags = flags;
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, id: u32, n: f64, string: *mut String128) -> tresult {
        ctrl_call("getParamStringByValue");
        let s = unsafe { &mut *string };
        let text = match id {
            3 => ["A", "B", "C"][((n * 2.0).round() as usize).min(2)].to_string(),
            _ => format!("{:.2}", unsafe { self.normalizedParamToPlain(id, n) }),
        };
        copy_c16(&text, s);
        kResultOk
    }
    unsafe fn getParamValueByString(&self, _id: u32, _s: *mut TChar, _v: *mut f64) -> tresult {
        ctrl_call("getParamValueByString");
        kNotImplemented
    }
    unsafe fn normalizedParamToPlain(&self, id: u32, n: f64) -> f64 {
        ctrl_call("normalizedParamToPlain");
        gain_to_plain(id, n)
    }
    unsafe fn plainParamToNormalized(&self, id: u32, v: f64) -> f64 {
        ctrl_call("plainParamToNormalized");
        gain_to_normalized(id, v)
    }
    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        ctrl_call("getParamNormalized");
        match id {
            7 => f64::from_bits(self.gain.load(Ordering::Relaxed)),
            12 => f64::from_bits(self.freq.load(Ordering::Relaxed)),
            FINE => f64::from_bits(FINE_VALUE.load(Ordering::Relaxed)),
            _ => 0.0,
        }
    }
    unsafe fn setParamNormalized(&self, id: u32, v: f64) -> tresult {
        ctrl_call("setParamNormalized");
        if probes() && id == 3 {
            reshape_freq(v >= 0.75);
        }
        if id == 7 {
            self.gain.store(v.to_bits(), Ordering::Relaxed);
        } else if id == 12 {
            self.freq.store(v.to_bits(), Ordering::Relaxed);
        } else if id == FINE {
            FINE_VALUE.store(v.to_bits(), Ordering::Relaxed);
        }
        kResultOk
    }
    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        ctrl_call("setComponentHandler");
        if !self.hello.load(Ordering::SeqCst) {
            violation("the controller got no IMessage from its connected component");
        }
        HANDLER.store(handler, Ordering::SeqCst);
        self.handler.store(handler, Ordering::SeqCst);
        // Exercise the host's handler: an edit from the "plugin GUI" must be accepted.
        if let Some(h) = unsafe { ComRef::from_raw(handler) } {
            assert_eq!(unsafe { h.beginEdit(7) }, kResultOk);
            assert_eq!(unsafe { h.performEdit(7, 0.9) }, kResultOk);
            assert_eq!(unsafe { h.endEdit(7) }, kResultOk);
        }
        kResultOk
    }
    unsafe fn createView(&self, name: FIDString) -> *mut IPlugView {
        ctrl_call("createView");
        if name.is_null() || unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes() != b"editor" {
            return std::ptr::null_mut();
        }
        match ComWrapper::new(View { attached: std::sync::atomic::AtomicBool::new(false), _live: Live::new(&OBJECTS) }).to_com_ptr::<IPlugView>() {
            Some(p) => p.into_raw(),
            None => std::ptr::null_mut(),
        }
    }
}

// ---- synth ---------------------------------------------------------------------------------

struct Synth {
    level: AtomicU32,
    /// The MIDI controller `IMidiMapping` assigns to the volume (7 until a learn).
    cc: AtomicI64,
    latency: AtomicU32,
    /// Latency to report after the next `setActive(true)` (-1: none).
    next_latency: AtomicI64,
    /// The host's component handler (alive as long as this controller).
    handler: AtomicPtr<IComponentHandler>,
    /// A command for the next `getState`.
    armed: std::sync::Mutex<Option<String>>,
    /// A command for every `getState`.
    every: std::sync::Mutex<Option<String>>,
    /// `setActive(true)` fails.
    refuse: AtomicBool,
    /// The events synth (see [`PROBE_PARAM`]).
    events: bool,
    /// (call, OS thread), each pair once.
    threads: std::sync::Mutex<Vec<(&'static str, usize)>>,
    _live: Live,
}

impl Synth {
    fn new() -> Synth {
        Synth {
            level: AtomicU32::new(0),
            cc: AtomicI64::new(7),
            latency: AtomicU32::new(0),
            next_latency: AtomicI64::new(-1),
            handler: AtomicPtr::new(std::ptr::null_mut()),
            armed: std::sync::Mutex::new(None),
            every: std::sync::Mutex::new(None),
            refuse: AtomicBool::new(false),
            events: false,
            threads: std::sync::Mutex::new(Vec::new()),
            _live: Live::new(&OBJECTS),
        }
    }

    fn note_thread(&self, call: &'static str) {
        let me = os_thread();
        let mut t = self.threads.lock().unwrap();
        if !t.contains(&(call, me)) {
            t.push((call, me));
        }
    }

    fn restart(&self, flags: i32) {
        if let Some(h) = unsafe { ComRef::from_raw(self.handler.load(Ordering::SeqCst)) } {
            unsafe { h.restartComponent(flags) };
        }
    }

    /// Runs a command (see the crate docs). False for an unknown one.
    fn command(&self, cmd: &str) -> bool {
        let (verb, arg) = cmd.split_once(':').unwrap_or((cmd, ""));
        match verb {
            "LEARN" => {
                self.cc.store(arg.parse().unwrap_or(7), Ordering::SeqCst);
                self.restart(RestartFlags_::kMidiCCAssignmentChanged as i32);
            }
            "LATENCY" => {
                self.next_latency.store(arg.parse().unwrap_or(0), Ordering::SeqCst);
                self.restart(RestartFlags_::kLatencyChanged as i32);
            }
            "RESTART" => self.restart(arg.parse().unwrap_or(0)),
            "ARM" => *self.armed.lock().unwrap() = Some(arg.to_string()),
            "EVERY" => *self.every.lock().unwrap() = Some(arg.to_string()),
            "REFUSE" => self.refuse.store(true, Ordering::SeqCst),
            _ => return false,
        }
        true
    }

    /// The component "state": the report (see the crate docs).
    fn report(&self) -> String {
        let threads = self.threads.lock().unwrap();
        let of = |call: &str| threads.iter().filter(|(c, _)| *c == call).map(|(_, t)| t.to_string()).collect::<Vec<_>>().join(",");
        format!(
            "me={};map={};active={};process={};get_latency={};latency={}",
            os_thread(),
            of("map"),
            of("active"),
            of("process"),
            of("latency"),
            self.latency.load(Ordering::SeqCst)
        )
    }
}

impl Class for Synth {
    type Interfaces = (IComponent, IAudioProcessor, IEditController, IMidiMapping);
}

/// The synth's hidden parameter that MIDI CC 7 drives.
const VOLUME_PARAM: ParamID = 1;

/// The events synth ("Fixture Events", F10) is the synth that exposes [`PROBE_PARAM`], maps CC 64 on channel `c`
/// to parameter `100 + c`, and writes what it receives into its output instead of a level: one
/// record of four samples per event and per parameter point, in the order it reads them —
/// `[1 on | 2 off | 3 poly pressure, offset, channel * 1000 + pitch, noteId]` and
/// `[4, offset, param id, value * 1e6]`. Integers below 2^24 are exact in `f32`.
/// The parameter the events mode exposes (normalized 0..1).
const PROBE_PARAM: ParamID = 5;

impl IMidiMappingTrait for Synth {
    unsafe fn getMidiControllerAssignment(&self, bus: i32, channel: i16, ctrl: CtrlNumber, id: *mut ParamID) -> tresult {
        self.note_thread("map");
        if bus == 0 && channel == 0 && i64::from(ctrl) == self.cc.load(Ordering::SeqCst) {
            unsafe { *id = VOLUME_PARAM };
            kResultOk
        } else if self.events && bus == 0 && ctrl == 64 && (0..16).contains(&channel) {
            unsafe { *id = 100 + channel as u32 };
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl IPluginBaseTrait for Synth {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for Synth {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        kNotImplemented
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, dir: BusDirection) -> i32 {
        let out = dir == BusDirections_::kOutput as BusDirection;
        if media == MediaTypes_::kAudio as MediaType && out {
            1
        } else if media == MediaTypes_::kEvent as MediaType && !out {
            1
        } else {
            0
        }
    }
    unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        if media == MediaTypes_::kAudio as MediaType && dir == BusDirections_::kOutput as BusDirection {
            audio_bus(bus, dir, "Out");
            return kResultOk;
        }
        if media == MediaTypes_::kEvent as MediaType && dir == BusDirections_::kInput as BusDirection {
            let b = unsafe { &mut *bus };
            b.mediaType = media;
            b.direction = dir;
            b.channelCount = 16;
            copy_c16("Notes", &mut b.name);
            b.busType = BusTypes_::kMain as BusType;
            b.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
            return kResultOk;
        }
        kInvalidArgument
    }
    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, _i: i32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, state: TBool) -> tresult {
        self.note_thread("active");
        if state != 0 && self.refuse.load(Ordering::SeqCst) {
            return kResultFalse;
        }
        if state != 0 {
            let next = self.next_latency.swap(-1, Ordering::SeqCst);
            if next >= 0 {
                self.latency.store(next as u32, Ordering::SeqCst);
            }
        }
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        // A command, or (anything else) an empty state.
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut bytes = [0u8; 64];
        let mut n = 0;
        if unsafe { s.read(bytes.as_mut_ptr().cast(), 64, &mut n) } == kResultOk
            && let Ok(text) = std::str::from_utf8(&bytes[..n.max(0) as usize])
        {
            self.command(text);
        }
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let armed = self.armed.lock().unwrap().take();
        if let Some(cmd) = armed {
            self.command(&cmd);
        }
        let every = self.every.lock().unwrap().clone();
        if let Some(cmd) = every {
            self.command(&cmd);
        }
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else { return kInvalidArgument };
        let mut text = self.report().into_bytes();
        let mut n = 0;
        unsafe { s.write(text.as_mut_ptr().cast(), text.len() as i32, &mut n) }
    }
}

impl IAudioProcessorTrait for Synth {
    unsafe fn setBusArrangements(&self, _i: *mut SpeakerArrangement, n_in: i32, outputs: *mut SpeakerArrangement, n_out: i32) -> tresult {
        if n_in == 0 && n_out == 1 && unsafe { *outputs } == SpeakerArr::kStereo { kResultTrue } else { kResultFalse }
    }
    unsafe fn getBusArrangement(&self, dir: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
        if dir != BusDirections_::kOutput as BusDirection || index != 0 {
            return kInvalidArgument;
        }
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size == SymbolicSampleSizes_::kSample32 as i32 { kResultOk } else { kNotImplemented }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        self.note_thread("latency");
        self.latency.load(Ordering::SeqCst)
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        self.note_thread("process");
        let d = unsafe { &*data };
        if d.numOutputs != 1 {
            return kResultOk;
        }
        let frames = d.numSamples.max(0) as usize;
        #[deny(clippy::undocumented_unsafe_blocks)]
        if self.events {
            // Allocation-free, so hosts can measure their own audio path around it.
            // SAFETY: IAudioProcessor::process supplies the one output bus checked above.
            let bus = unsafe { &*d.outputs };
            let n = bus.numChannels.max(0) as usize;
            // SAFETY: The SDK bus has n channel pointers; this fixture negotiates kSample32.
            let table = unsafe { std::slice::from_raw_parts(bus.__field0.channelBuffers32, n) };
            let Some(&first) = table.first() else { return kResultOk };
            // SAFETY: The SDK provides frames writable f32 samples on the first output channel.
            let out = unsafe { std::slice::from_raw_parts_mut(first, frames) };
            out.fill(0.0);
            let mut w = 0;
            let mut put = |r: [f32; 4]| {
                for x in r {
                    if let Some(o) = out.get_mut(w) {
                        *o = x;
                    }
                    w += 1;
                }
            };
            // SAFETY: inputEvents is null or a borrowed IEventList valid for this process call.
            if let Some(list) = unsafe { ComRef::from_raw(d.inputEvents) } {
                // SAFETY: list is the live SDK interface borrowed above.
                for i in 0..unsafe { list.getEventCount() } {
                    // SAFETY: SDK Event contains C scalars and a union, all valid as zero bits; getEvent fills its tagged payload.
                    let mut e: Event = unsafe { std::mem::zeroed() };
                    // SAFETY: i is within the SDK list and e is writable Event storage.
                    if unsafe { list.getEvent(i, &mut e) } != kResultOk {
                        continue;
                    }
                    let ty = e.r#type;
                    let (code, ch, pitch, id) = if ty == Event_::EventTypes_::kNoteOnEvent as u16 {
                        // SAFETY: The successful SDK event read has the kNoteOnEvent union tag.
                        let n = unsafe { e.__field0.noteOn };
                        (1.0, n.channel, n.pitch, n.noteId)
                    } else if ty == Event_::EventTypes_::kNoteOffEvent as u16 {
                        // SAFETY: The successful SDK event read has the kNoteOffEvent union tag.
                        let n = unsafe { e.__field0.noteOff };
                        (2.0, n.channel, n.pitch, n.noteId)
                    } else if ty == Event_::EventTypes_::kPolyPressureEvent as u16 {
                        // SAFETY: The successful SDK event read has the kPolyPressureEvent union tag.
                        let n = unsafe { e.__field0.polyPressure };
                        (3.0, n.channel, n.pitch, n.noteId)
                    } else {
                        continue;
                    };
                    put([code, e.sampleOffset as f32, f32::from(ch) * 1000.0 + f32::from(pitch), id as f32]);
                }
            }
            // SAFETY: inputParameterChanges is null or a borrowed SDK interface valid in process.
            if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
                // SAFETY: changes is the live IParameterChanges interface borrowed above.
                for i in 0..unsafe { changes.getParameterCount() } {
                    // SAFETY: i is in the SDK changes list; its queue remains borrowed for process.
                    let Some(q) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else { continue };
                    // SAFETY: q is the live IParamValueQueue returned by the SDK interface.
                    let pid = unsafe { q.getParameterId() };
                    // SAFETY: q remains valid for this process call.
                    for k in 0..unsafe { q.getPointCount() } {
                        let (mut at, mut v) = (0i32, 0f64);
                        // SAFETY: k is in the SDK queue; at and v are writable output values.
                        if unsafe { q.getPoint(k, &mut at, &mut v) } == kResultOk {
                            put([4.0, at as f32, pid as f32, (v * 1e6).round() as f32]);
                        }
                    }
                }
            }
            for &p in table.iter().skip(1) {
                // SAFETY: This SDK channel has frames writable samples distinct from the first channel (no input buses).
                let o = unsafe { std::slice::from_raw_parts_mut(p, frames) };
                // SAFETY: The first SDK output channel remains live and has frames initialized samples.
                o.copy_from_slice(unsafe { std::slice::from_raw_parts(first, frames) });
            }
            return kResultOk;
        }
        let mut events = Vec::new();
        if let Some(list) = unsafe { ComRef::from_raw(d.inputEvents) } {
            for i in 0..unsafe { list.getEventCount() } {
                let mut e: Event = unsafe { std::mem::zeroed() };
                if unsafe { list.getEvent(i, &mut e) } == kResultOk {
                    events.push(e);
                }
            }
        }
        let mut volume = None;
        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                let Some(q) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else { continue };
                let (mut at, mut v) = (0i32, 0f64);
                if unsafe { q.getParameterId() } == VOLUME_PARAM && unsafe { q.getPoint(0, &mut at, &mut v) } == kResultOk {
                    volume = Some((at.max(0) as usize, v as f32));
                }
            }
        }
        let outs = unsafe { channels(&*d.outputs, frames) };
        let mut level = f32::from_bits(self.level.load(Ordering::Relaxed));
        let mut out = vec![0.0f32; frames];
        for (i, o) in out.iter_mut().enumerate() {
            for e in events.iter().filter(|e| e.sampleOffset as usize == i) {
                if e.r#type == Event_::EventTypes_::kNoteOnEvent as u16 {
                    level = unsafe { e.__field0.noteOn.velocity };
                } else if e.r#type == Event_::EventTypes_::kNoteOffEvent as u16 {
                    level = 0.0;
                } else if e.r#type == Event_::EventTypes_::kPolyPressureEvent as u16 {
                    level = unsafe { e.__field0.polyPressure.pressure };
                }
            }
            if let Some((at, v)) = volume
                && at == i
            {
                level = v;
            }
            *o = level;
        }
        self.level.store(level.to_bits(), Ordering::Relaxed);
        for ch in outs {
            ch.copy_from_slice(&out);
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IEditControllerTrait for Synth {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> i32 {
        i32::from(self.events)
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        if !self.events || index != 0 {
            return kInvalidArgument;
        }
        let i = unsafe { &mut *info };
        i.id = PROBE_PARAM;
        copy_c16("Probe", &mut i.title);
        copy_c16("Probe", &mut i.shortTitle);
        copy_c16("", &mut i.units);
        i.stepCount = 0;
        i.defaultNormalizedValue = 0.0;
        i.unitId = 0;
        i.flags = ParameterInfo_::ParameterFlags_::kCanAutomate as i32;
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, _id: u32, _n: f64, _s: *mut String128) -> tresult {
        kInvalidArgument
    }
    unsafe fn getParamValueByString(&self, _id: u32, _s: *mut TChar, _v: *mut f64) -> tresult {
        kInvalidArgument
    }
    unsafe fn normalizedParamToPlain(&self, _id: u32, n: f64) -> f64 {
        n
    }
    unsafe fn plainParamToNormalized(&self, _id: u32, v: f64) -> f64 {
        v
    }
    unsafe fn getParamNormalized(&self, _id: u32) -> f64 {
        0.0
    }
    unsafe fn setParamNormalized(&self, _id: u32, _v: f64) -> tresult {
        kResultOk
    }
    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        self.handler.store(handler, Ordering::SeqCst);
        kResultOk
    }
    unsafe fn createView(&self, _name: FIDString) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}

// ---- factory -------------------------------------------------------------------------------

struct Factory {
    _live: Live,
}

impl Class for Factory {
    type Interfaces = (IPluginFactory2,);
}

const CLASSES: [(TUID, &str, &str, &str); 4] = [
    (GAIN_CID, "Audio Module Class", "Fixture Gain", "Fx"),
    (GAIN_CTRL_CID, "Component Controller Class", "Fixture Gain", ""),
    (SYNTH_CID, "Audio Module Class", "Fixture Synth", "Instrument|Synth"),
    (EVENTS_CID, "Audio Module Class", "Fixture Events", "Instrument|Synth"),
];

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let i = unsafe { &mut *info };
        copy_c8("SoundCraft tests", &mut i.vendor);
        copy_c8("https://example.invalid", &mut i.url);
        copy_c8("tests@example.invalid", &mut i.email);
        i.flags = PFactoryInfo_::FactoryFlags_::kUnicode as i32;
        kResultOk
    }
    unsafe fn countClasses(&self) -> i32 {
        CLASSES.len() as i32
    }
    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let Some(&(cid, cat, name, _)) = CLASSES.get(index as usize) else { return kInvalidArgument };
        let i = unsafe { &mut *info };
        i.cid = cid;
        i.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as i32;
        copy_c8(cat, &mut i.category);
        copy_c8(name, &mut i.name);
        kResultOk
    }
    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        set_up_late_state();
        let cid = unsafe { *(cid as *const TUID) };
        let unknown = if cid == GAIN_CID {
            ComWrapper::new(GainProcessor {
                gain: AtomicU64::new(0.25f64.to_bits()),
                arrangement: AtomicU64::new(SpeakerArr::kStereo),
                host: AtomicPtr::new(std::ptr::null_mut()),
                _live: Live::new(&OBJECTS),
            })
            .to_com_ptr::<FUnknown>()
        } else if cid == GAIN_CTRL_CID {
            ComWrapper::new(GainController {
                gain: AtomicU64::new(0.5f64.to_bits()),
                freq: AtomicU64::new(0.5f64.to_bits()),
                preset: AtomicU64::new(u64::MAX),
                handler: AtomicPtr::new(std::ptr::null_mut()),
                hello: AtomicBool::new(false),
                _live: Live::new(&OBJECTS),
            })
            .to_com_ptr::<FUnknown>()
        } else if cid == SYNTH_CID {
            ComWrapper::new(Synth::new()).to_com_ptr::<FUnknown>()
        } else if cid == EVENTS_CID {
            ComWrapper::new(Synth { events: true, ..Synth::new() }).to_com_ptr::<FUnknown>()
        } else {
            None
        };
        match unknown {
            Some(u) => {
                let p = u.as_ptr();
                unsafe { ((*(*p).vtbl).queryInterface)(p, iid as *const TUID, obj) }
            }
            None => kInvalidArgument,
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let Some(&(cid, cat, name, subs)) = CLASSES.get(index as usize) else { return kInvalidArgument };
        let i = unsafe { &mut *info };
        i.cid = cid;
        i.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as i32;
        copy_c8(cat, &mut i.category);
        copy_c8(name, &mut i.name);
        i.classFlags = 0;
        copy_c8(subs, &mut i.subCategories);
        copy_c8("", &mut i.vendor);
        copy_c8("1.2.3", &mut i.version);
        copy_c8("VST 3.8.0", &mut i.sdkVersion);
        kResultOk
    }
}

// ---- probe mode ----------------------------------------------------------------------------
//
// With `VST3_FIXTURE_PROBES` set: Gain's controller ends the process (status 3) when it is called
// on a thread that has run Gain's `process` (VST3 allows edit-controller calls on the UI thread
// only); Gain's processor writes the last Freq value it received (normalized) to its second output
// channel instead of the gained input; and setting Mode from the host reshapes Freq's curve to
// `20 + 19980 n²` at C (same endpoints; `20 · 1000ⁿ` at A and B) and tells the host with
// `restartComponent(kParamTitlesChanged)`, as a plug-in whose mode changes a range would.

/// 0: not read yet, 1: off, 2: on.
static PROBES: AtomicU32 = AtomicU32::new(0);
/// OS threads that have run Gain's `process` (0: free slot).
static PROCESS_THREADS: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
/// The last Freq point Gain's processor received (f64 bits).
static LAST_FREQ: AtomicU64 = AtomicU64::new(0);
/// Freq's curve shape: `20 · 1000ⁿ` (false) or `20 + 19980 n²` (true).
static FREQ_SQUARE: AtomicBool = AtomicBool::new(false);
/// The component handler the last Gain controller was given (the host's, alive with it).
static HANDLER: AtomicPtr<IComponentHandler> = AtomicPtr::new(std::ptr::null_mut());

fn probes() -> bool {
    match PROBES.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var_os("VST3_FIXTURE_PROBES").is_some();
            PROBES.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        n => n == 2,
    }
}

fn note_process_thread() {
    let me = os_thread();
    for slot in &PROCESS_THREADS {
        match slot.compare_exchange(0, me, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(t) if t == me => return,
            Err(_) => {}
        }
    }
}

fn ctrl_call(what: &str) {
    CTRL_CALLS.fetch_add(1, Ordering::Relaxed);
    if probes() {
        let me = os_thread();
        if PROCESS_THREADS.iter().any(|t| t.load(Ordering::SeqCst) == me) {
            violation(&format!("edit controller {what} called on the render thread"));
        }
    }
}

fn reshape_freq(square: bool) {
    if FREQ_SQUARE.swap(square, Ordering::SeqCst) == square {
        return;
    }
    if let Some(h) = unsafe { ComRef::from_raw(HANDLER.load(Ordering::SeqCst)) } {
        unsafe { h.restartComponent(RestartFlags_::kParamTitlesChanged as i32) };
    }
}

// ---- module lifecycle ----------------------------------------------------------------------
//
// The contract real plugins rely on: the host runs the module exit once the factory and every
// plugin object are released, on the thread that ran the module entry, and before the process
// tears down the module's statics. Breaking it ends the process with status 3, much as real
// plugins crash (HALion Sonic segfaults in its static destructors when the module exit never ran,
// and in its module exit when that runs from an exit handler after it was used; Massive X in its
// module exit when that runs on another thread than the entry).

/// Module entries minus module exits (they nest, as in the VST3 SDK).
static ENTERED: AtomicI64 = AtomicI64::new(0);
/// The OS thread that ran the outermost module entry.
static ENTRY_THREAD: AtomicUsize = AtomicUsize::new(0);
/// Factories handed out and not yet released.
static FACTORIES: AtomicI64 = AtomicI64::new(0);
/// Plugin objects alive: components, controllers, views.
static OBJECTS: AtomicI64 = AtomicI64::new(0);
static TEARDOWN_REGISTERED: AtomicBool = AtomicBool::new(false);
/// With `VST3_FIXTURE_LATE_STATE` set, the first plugin made sets up state that the process exit
/// tears down, like a static a real plugin initialises lazily; the module exit needs it.
static LATE_STATE_REGISTERED: AtomicBool = AtomicBool::new(false);
static LATE_STATE_GONE: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> c_int;
    fn _exit(status: c_int) -> !;
}

/// Counts itself in one of the counters above for as long as it exists.
struct Live(&'static AtomicI64);

impl Live {
    fn new(counter: &'static AtomicI64) -> Live {
        counter.fetch_add(1, Ordering::SeqCst);
        Live(counter)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The first violation any thread reported: later ones (a second thread, a drop-time call) must
/// not hide it.
static FIRST_VIOLATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Ends the process at once with status 3 (no crash report: one test expects this), naming the
/// first violation.
fn violation(what: &str) -> ! {
    let first = FIRST_VIOLATION.get_or_init(|| what.to_string());
    eprintln!("vst3-fixture: host contract violation: {first}");
    unsafe { _exit(3) }
}

/// The OS id of the calling thread (`std::thread::current` is gone in exit handlers).
fn os_thread() -> usize {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn pthread_self() -> usize;
        }
        unsafe { pthread_self() }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() as usize }
    }
}

fn module_entry() -> bool {
    if ENTERED.fetch_add(1, Ordering::SeqCst) == 0 {
        ENTRY_THREAD.store(os_thread(), Ordering::SeqCst);
    }
    if !TEARDOWN_REGISTERED.swap(true, Ordering::SeqCst) {
        unsafe { atexit(static_teardown) };
    }
    true
}

/// Called whenever the factory makes a plugin object.
fn set_up_late_state() {
    if std::env::var_os("VST3_FIXTURE_LATE_STATE").is_some() && !LATE_STATE_REGISTERED.swap(true, Ordering::SeqCst) {
        unsafe { atexit(late_state_teardown) };
    }
}

extern "C" fn late_state_teardown() {
    LATE_STATE_GONE.store(true, Ordering::SeqCst);
}

fn module_exit() -> bool {
    let left = ENTERED.fetch_sub(1, Ordering::SeqCst) - 1;
    if left < 0 {
        violation("module exit without a module entry");
    }
    let (factories, objects) = (FACTORIES.load(Ordering::SeqCst), OBJECTS.load(Ordering::SeqCst));
    if left == 0 && (factories != 0 || objects != 0) {
        violation(&format!("module exit with {factories} factories and {objects} plugin objects alive"));
    }
    if left == 0 && os_thread() != ENTRY_THREAD.load(Ordering::SeqCst) {
        violation("module exit on another thread than the module entry");
    }
    if LATE_STATE_GONE.load(Ordering::SeqCst) {
        violation("module exit after the process exit tore down state the module set up while in use");
    }
    true
}

/// Stands in for the C++ static destructors a real module registers while it loads.
extern "C" fn static_teardown() {
    if ENTERED.load(Ordering::SeqCst) > 0 && OBJECTS.load(Ordering::SeqCst) == 0 {
        violation("static teardown of a module that is not in use but was never exited");
    }
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    module_entry()
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    module_exit()
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn bundleEntry(bundle: *mut c_void) -> bool {
    !bundle.is_null() && module_entry()
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn bundleExit() -> bool {
    module_exit()
}

#[cfg(all(unix, not(target_os = "macos")))]
#[unsafe(no_mangle)]
extern "C" fn ModuleEntry(handle: *mut c_void) -> bool {
    !handle.is_null() && module_entry()
}

#[cfg(all(unix, not(target_os = "macos")))]
#[unsafe(no_mangle)]
extern "C" fn ModuleExit() -> bool {
    module_exit()
}

#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    match ComWrapper::new(Factory { _live: Live::new(&FACTORIES) }).to_com_ptr::<IPluginFactory>() {
        Some(p) => p.into_raw(),
        None => std::ptr::null_mut(),
    }
}
