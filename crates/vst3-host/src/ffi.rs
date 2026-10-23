//! The FFI boundary: the only module of this crate allowed to use `unsafe`.
//!
//! It loads VST3 binaries (`libloading`), runs the platform module entry, calls the COM
//! interfaces of the factory, component, audio processor and edit controller (`vst3` raw
//! bindings), and implements the host-side COM objects (host context, parameter changes, event
//! lists, a memory stream). Everything it exposes to the rest of the crate is a safe API.
//!
//! Soundness rests on the VST3 contract (the plugin honours the COM vtable signatures and the
//! documented pointer lifetimes). A plugin that violates it can still crash the process: hosting
//! in-process can't prevent that. What this module guarantees is that *SoundCraft* never hands
//! the plugin dangling or mis-sized pointers and never reads plugin strings beyond their bounds.

use crate::{Vst3Error, scan};
use soundcraft_dsp::Transport;
use std::ffi::{c_char, c_int, c_void};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, TryLockError, mpsc};
use std::time::Duration;
use vst3::Steinberg::Vst::{self as sv};
use vst3::Steinberg::Vst::{
    IAttributeListTrait, IAudioProcessorTrait, IComponentHandlerTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait, IEventListTrait,
    IHostApplicationTrait, IMessageTrait, IMidiMappingTrait, IParamValueQueueTrait, IParameterChangesTrait,
};
use vst3::Steinberg::{self as sb};
use vst3::Steinberg::{FUnknown, IBStreamTrait, IPlugFrameTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory2Trait, IPluginFactoryTrait};
use vst3::com_scrape_types::{Construct, Header, MakeHeader, Wrapper};
use vst3::{Class, ComPtr, ComWrapper, Interface};

/// Most classes one factory may report (hostile-input cap).
const MAX_CLASSES: i32 = 1024;
/// Most parameters read from one controller.
pub(crate) const MAX_PARAMS: i32 = 4096;
/// Most audio buses per direction we provide buffers for.
const MAX_BUSES: i32 = 16;
/// Most channels per audio bus.
const MAX_BUS_CHANNELS: u32 = 64;
/// Capacity of the per-block event lists (preallocated: `process` never allocates).
pub(crate) const EVENT_CAP: usize = 1024;
/// Most parameter queues per block (preallocated).
pub(crate) const PARAM_QUEUE_CAP: usize = 1024;
/// Parameter points one block carries across all its queues (a preallocated slab).
pub(crate) const PARAM_POINTS: usize = 4096;
/// Event entries kept for releases (F10): one per note and pedal the mix holds open (224 + 32),
/// plus one reset. Other events are refused once only these remain.
pub(crate) const RELEASE_RESERVE: usize = 257;
/// MIDI channels the controller mapping covers.
pub(crate) const MIDI_CHANNELS: usize = 16;
// Largest plugin state we buffer (component or controller state).
use crate::preset::MAX_STATE_BYTES;
/// Most editor parameter edits buffered between two UI frames.
const EDIT_CAP: usize = 256;
// Magic of SoundCraft's state container: SCV3 + u32 LE component length + component state +
// u32 LE controller length + controller state (crate::preset::join_state).
#[cfg(test)]
use crate::preset::STATE_MAGIC;

// ---- strings -------------------------------------------------------------------------------

/// Reads a fixed-size `char8[N]` buffer, stopping at the first NUL or the end of the array.
fn read_c8(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Reads a fixed-size UTF-16 `String128`, stopping at the first NUL or the end of the array.
fn read_c16(buf: &[u16]) -> String {
    let units: Vec<u16> = buf.iter().take_while(|c| **c != 0).copied().collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// Writes `s` into a UTF-16 buffer, truncated and always NUL-terminated.
fn write_c16(s: &str, dst: &mut [u16]) {
    let Some(room) = dst.len().checked_sub(1) else { return };
    let mut n = 0;
    for (d, u) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = u;
        n += 1;
    }
    if let Some(z) = dst.get_mut(n) {
        *z = 0;
    }
}

fn ok(r: sb::tresult) -> bool {
    r == sb::kResultOk
}

/// VST3's numbers for the MIDI controllers that aren't CCs.
pub(crate) const AFTERTOUCH: usize = sv::ControllerNumbers_::kAfterTouch as usize;
pub(crate) const PITCH_BEND: usize = sv::ControllerNumbers_::kPitchBend as usize;

// ---- loaded binaries -----------------------------------------------------------------------

/// One class as reported by the factory.
#[derive(Debug, Clone)]
pub(crate) struct RawClass {
    pub cid: [u8; 16],
    pub category: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub sdk_version: String,
    pub sub_categories: String,
}

/// A loaded VST3 binary and its plugin factory. Loaded binaries are cached for the life of the
/// process (see [`Bundle::load`]): plugin code must never be unmapped while an instance may run,
/// and VST3 modules are not required to support being re-entered after their exit function, which
/// therefore runs once, at process exit (see [`exit_modules`]).
pub(crate) struct Bundle {
    factory: ComPtr<sb::IPluginFactory>,
    /// The module exit matching the entry that ran at load (`bundleExit`, `ExitDll`, `ModuleExit`).
    exit: Option<ModuleExit>,
    /// The thread that ran the module entry, and so must run the exit.
    entered_on: EntryThread,
    /// Set when plugin objects were leaked on purpose (see [`EditorLink::kill`]): the module exit
    /// must not run while any of its objects are alive.
    leaked: AtomicBool,
    // Never closed: code stays mapped for the process lifetime, even after the module exit.
    _lib: ManuallyDrop<libloading::Library>,
}

/// Loaded binaries, oldest first (module exits run newest first, the reverse of loading).
fn loaded() -> &'static Mutex<Vec<(PathBuf, Arc<Bundle>)>> {
    static L: OnceLock<Mutex<Vec<(PathBuf, Arc<Bundle>)>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Vec::new()))
}

/// Binaries whose module entry runs now, outside the [`loaded`] lock (K38): a module entry that
/// hangs holds back only later loads of the same binary, not every VST3 load.
fn opening() -> &'static (Mutex<Vec<PathBuf>>, Condvar) {
    static O: OnceLock<(Mutex<Vec<PathBuf>>, Condvar)> = OnceLock::new();
    O.get_or_init(|| (Mutex::new(Vec::new()), Condvar::new()))
}

/// This thread's claim on opening one binary ([`opening`]); dropping it lets other loaders of
/// that binary find it in the cache (or open it themselves if this load failed).
struct Opening<'a>(&'a Path);

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        let (list, done) = opening();
        list.lock().unwrap_or_else(PoisonError::into_inner).retain(|p| p != self.0);
        done.notify_all();
    }
}

/// Set once the module exits have run: loading another module after that is refused.
static MODULES_EXITED: AtomicBool = AtomicBool::new(false);

/// Where a module's entry ran. Its exit must run on the same thread: Qt-based modules tie their
/// application object to it (Native Instruments' Massive X crashes in `bundleExit` on another
/// thread). Modules wanted on a plug-in main thread (the thread named `main`, or one that called
/// [`adopt_main_thread`], the host thread of ADR 0018) are entered there; any other thread's
/// requests go to the module thread, because the asking thread may be gone by the time the
/// process exits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryThread {
    /// A plug-in main thread, by its OS id ([`os_thread`]).
    Here(usize),
    Modules,
}

thread_local! {
    /// This thread called [`adopt_main_thread`]. `const`-initialized and without a destructor.
    static ADOPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the calling thread a plug-in main thread for module entry and exit (see [`EntryThread`]).
pub(crate) fn adopt_main_thread() {
    ADOPTED.with(|a| a.set(true));
}

/// Whether this is a plug-in main thread. Not for exit handlers, where `std::thread::current`
/// panics (its thread-local data is gone by then): compare [`os_thread`] with a bundle's
/// [`EntryThread::Here`] there.
fn on_main_thread() -> bool {
    ADOPTED.with(std::cell::Cell::get) || std::thread::current().name() == Some("main")
}

/// The OS id of the calling thread, which (unlike `std::thread::current`) exit handlers can use.
fn os_thread() -> usize {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn pthread_self() -> usize;
        }
        // SAFETY: `pthread_self` takes no arguments and cannot fail; `pthread_t` is an integer or
        // a pointer of pointer size on the Unix systems SoundCraft builds for.
        unsafe { pthread_self() }
    }
    #[cfg(windows)]
    {
        // SAFETY: `GetCurrentThreadId` takes no arguments and cannot fail.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() as usize }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// The module thread (started on first use): it runs jobs until the process exits.
fn module_thread() -> Option<&'static mpsc::Sender<Job>> {
    static TX: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("vst3-modules".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
        });
        spawned.ok().map(|_| tx)
    })
    .as_ref()
}

/// Runs `f` on the module thread and waits for its result (at most `timeout`, when given).
fn on_module_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static, timeout: Option<Duration>) -> Option<R> {
    let (rtx, rrx) = mpsc::sync_channel(1);
    module_thread()?
        .send(Box::new(move || {
            let _ = rtx.send(f());
        }))
        .ok()?;
    match timeout {
        Some(t) => rrx.recv_timeout(t).ok(),
        None => rrx.recv().ok(),
    }
}

type GetFactory = unsafe extern "system" fn() -> *mut sb::IPluginFactory;
/// `bundleExit` (macOS), `ExitDll` (Windows) or `ModuleExit` (Linux/BSD). `extern "system"` is the
/// C ABI everywhere except 32-bit Windows, where it is the `PLUGIN_API` (stdcall) `ExitDll` uses.
type ModuleExit = unsafe extern "system" fn() -> bool;

#[cfg(target_os = "macos")]
mod cf {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, is_dir: u8) -> *const c_void;
        pub fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *mut c_void;
        pub fn CFRelease(cf: *const c_void);
    }
}

/// Opens the shared library and runs the platform module entry (`bundleEntry` with a
/// `CFBundleRef` on macOS, `InitDll` on Windows, `ModuleEntry` with the `dlopen` handle on Linux).
/// Also returns the matching module exit, when the entry ran and the module exports one.
fn open_library(bundle: &Path, exe: &Path) -> Result<(libloading::Library, Option<ModuleExit>), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use libloading::os::unix::{Library as UnixLib, RTLD_LOCAL, RTLD_NOW};
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in VST3 plugin folders or named explicitly.
        let lib = unsafe { UnixLib::open(Some(exe), RTLD_NOW | RTLD_LOCAL) }.map_err(|e| e.to_string())?;
        type ModuleEntry = unsafe extern "C" fn(*mut c_void) -> bool;
        // SAFETY: `ModuleEntry` is the VST3-mandated Linux entry with this signature.
        let entry = unsafe { lib.get::<ModuleEntry>(b"ModuleEntry\0") }.ok().map(|s| *s);
        let raw = lib.into_raw();
        // SAFETY: `raw` is the handle we just took out of `lib`; it is rewrapped right away.
        let lib = unsafe { UnixLib::from_raw(raw) };
        let mut exit = None;
        if let Some(f) = entry {
            // SAFETY: called once, with the module's own `dlopen` handle, as the spec requires.
            if !unsafe { f(raw) } {
                return Err("ModuleEntry failed".into());
            }
            // SAFETY: `ModuleExit` is the VST3-mandated Linux exit with this signature; the
            // pointer stays valid because loaded modules are never unmapped.
            exit = unsafe { lib.get::<ModuleExit>(b"ModuleExit\0") }.ok().map(|s| *s);
        }
        let _ = bundle;
        Ok((lib.into(), exit))
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in VST3 plugin folders or named explicitly.
        let lib = unsafe { libloading::Library::new(exe) }.map_err(|e| e.to_string())?;
        let mut exit: Option<ModuleExit> = None;
        #[cfg(windows)]
        {
            let _ = bundle;
            type InitDll = unsafe extern "system" fn() -> bool;
            // SAFETY: `InitDll` is the VST3-mandated Windows entry with this signature.
            if let Ok(f) = unsafe { lib.get::<InitDll>(b"InitDll\0") } {
                // SAFETY: called once after loading, as the spec requires.
                if !unsafe { f() } {
                    return Err("InitDll failed".into());
                }
                // SAFETY: `ExitDll` is the VST3-mandated Windows exit with this signature; the
                // pointer stays valid because loaded modules are never unmapped.
                exit = unsafe { lib.get::<ModuleExit>(b"ExitDll\0") }.ok().map(|s| *s);
            }
        }
        #[cfg(target_os = "macos")]
        {
            type BundleEntry = unsafe extern "C" fn(*mut c_void) -> bool;
            // SAFETY: `bundleEntry`/`BundleEntry` is the VST3-mandated macOS entry with this signature.
            let entry = unsafe { lib.get::<BundleEntry>(b"bundleEntry\0") }
                .or_else(|_| {
                    // SAFETY: as above (older SDKs export the capitalised name).
                    unsafe { lib.get::<BundleEntry>(b"BundleEntry\0") }
                })
                .ok()
                .map(|s| *s);
            if let Some(f) = entry
                && bundle.is_dir()
            {
                use std::os::unix::ffi::OsStrExt;
                let bytes = bundle.as_os_str().as_bytes();
                let len = isize::try_from(bytes.len()).map_err(|_| "path too long".to_string())?;
                // SAFETY: `bytes` is a valid buffer of `len` bytes for the duration of the call;
                // a null allocator means the default one.
                let url = unsafe { cf::CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), len, 1) };
                if url.is_null() {
                    return Err("cannot make a CFURL for the bundle".into());
                }
                // SAFETY: `url` is a valid CFURL created above.
                let cfbundle = unsafe { cf::CFBundleCreate(std::ptr::null(), url) };
                // SAFETY: we own `url` (Create rule) and no longer need it.
                unsafe { cf::CFRelease(url) };
                if cfbundle.is_null() {
                    return Err("not a loadable bundle".into());
                }
                // SAFETY: called once with a valid CFBundleRef. The bundle ref is intentionally
                // kept (never released): the module may hold on to it for its whole life, and
                // loaded modules are never unloaded.
                if !unsafe { f(cfbundle) } {
                    return Err("bundleEntry failed".into());
                }
                // SAFETY: `bundleExit`/`BundleExit` is the VST3-mandated macOS exit with this
                // signature; the pointer stays valid because loaded modules are never unmapped.
                exit = unsafe { lib.get::<ModuleExit>(b"bundleExit\0") }
                    .or_else(|_| {
                        // SAFETY: as above (older SDKs export the capitalised name).
                        unsafe { lib.get::<ModuleExit>(b"BundleExit\0") }
                    })
                    .ok()
                    .map(|s| *s);
            }
        }
        Ok((lib, exit))
    }
}

/// Runs the module exits at process exit, newest module first, each on the thread that ran its
/// entry. VST3 modules expect their exit before their static destructors run, and some crash in
/// those destructors without it (seen with Steinberg's HALion Sonic, after a plain scan). A module
/// with a live instance or leaked objects keeps running and is not exited, and so is one entered
/// on another plug-in main thread than the calling one (that thread's own call exits it). The
/// first call refuses later loads.
pub(crate) fn exit_modules() {
    // Later loads are refused. A later call (another main thread, or the exit handler) still
    // exits what is left that it may: each bundle is exited at most once, it leaves the cache.
    MODULES_EXITED.store(true, Ordering::Release);
    let mut cache = match loaded().try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        // Another thread is loading a module right now: leave every module as it is.
        Err(TryLockError::WouldBlock) => return,
    };
    let me = EntryThread::Here(os_thread());
    let (mut kept, mut here, mut there) = (Vec::new(), Vec::new(), Vec::new());
    while let Some((path, b)) = cache.pop() {
        if b.leaked.load(Ordering::Acquire) || (b.entered_on != EntryThread::Modules && b.entered_on != me) {
            kept.push((path, b));
            continue;
        }
        match Arc::try_unwrap(b) {
            Ok(bundle) if bundle.entered_on == me => here.push(bundle),
            Ok(bundle) => there.push(bundle),
            Err(b) => kept.push((path, b)),
        }
    }
    kept.reverse();
    *cache = kept;
    drop(cache);
    here.into_iter().for_each(Bundle::exit);
    if !there.is_empty() {
        // Bounded: a module thread stuck inside a plugin must not hang the process exit.
        let _ = on_module_thread(move || there.into_iter().for_each(Bundle::exit), Some(Duration::from_secs(5)));
    }
}

/// Registers [`exit_modules`] to run at process exit. Exit handlers run in reverse order of
/// registration, C++ static destructors included, so registering again after each module load
/// puts the exits ahead of the destructors of every module loaded so far.
fn register_exit_hook() {
    unsafe extern "C" {
        fn atexit(f: extern "C" fn()) -> c_int;
    }
    extern "C" fn hook() {
        // Nothing may unwind out of an exit handler.
        let _ = std::panic::catch_unwind(exit_modules);
    }
    // SAFETY: `atexit` is the C library function with this signature; `hook` is a plain function
    // of this binary, so it lives as long as the process.
    if unsafe { atexit(hook) } != 0 {
        log::warn!("vst3: cannot register the module exit handler");
    }
}

impl Bundle {
    /// Loads (once per path, then cached for the life of the process) a `.vst3`.
    ///
    /// The module entry and `GetPluginFactory` run without the cache lock (K38): a module entry
    /// that never returns blocks later loads of the same binary only, which wait for it; other
    /// binaries load meanwhile. The bundle is cached under the lock once it is open.
    pub fn load(path: &Path) -> Result<Arc<Bundle>, Vst3Error> {
        let err = |m: &str| Vst3Error::Load(path.display().to_string(), m.to_string());
        let cached = || loaded().lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(p, _)| p == path).map(|(_, b)| b.clone());
        // Claim the binary, or wait for the thread that is opening it and take its result.
        let claim = {
            let (list, done) = opening();
            let mut list = list.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(b) = cached() {
                    return Ok(b);
                }
                if MODULES_EXITED.load(Ordering::Acquire) {
                    return Err(err("plugin hosting has shut down"));
                }
                if !list.iter().any(|p| p == path) {
                    list.push(path.to_path_buf());
                    break Opening(path);
                }
                list = done.wait(list).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let b = if on_main_thread() {
            Bundle::open(path, EntryThread::Here(os_thread()))?
        } else {
            let p = path.to_path_buf();
            on_module_thread(move || Bundle::open(&p, EntryThread::Modules), None).ok_or_else(|| err("the module thread is not running"))??
        };
        let b = Arc::new(b);
        loaded().lock().unwrap_or_else(PoisonError::into_inner).push((path.to_path_buf(), b.clone()));
        // After every new module, so that the exits run before its static destructors.
        register_exit_hook();
        drop(claim);
        // Entered while the module exits ran: it stays cached, so a later exit call on its entry
        // thread (the host thread's shutdown job, or the exit handler for a module-thread entry)
        // still balances its entry; on a host thread that is wedged at shutdown it is leaked,
        // never exited on another thread. Hosting is over either way.
        if MODULES_EXITED.load(Ordering::Acquire) {
            return Err(err("plugin hosting has shut down"));
        }
        Ok(b)
    }

    fn open(path: &Path, entered_on: EntryThread) -> Result<Bundle, Vst3Error> {
        let err = |m: String| Vst3Error::Load(path.display().to_string(), m);
        let exe = scan::bundle_binary(path).ok_or_else(|| err("no loadable binary".into()))?;
        let (lib, exit) = open_library(path, &exe).map_err(err)?;
        // The module entry ran: balance it before giving up on the module.
        let fail = |m: String| {
            if let Some(f) = exit {
                // SAFETY: balanced with the entry that ran in `open_library`; no plugin object exists.
                unsafe { f() };
            }
            err(m)
        };
        // SAFETY: `GetPluginFactory` is the VST3-mandated export with this signature.
        let get = match unsafe { lib.get::<GetFactory>(b"GetPluginFactory\0") } {
            Ok(sym) => *sym,
            Err(e) => return Err(fail(format!("not a VST3 plugin: {e}"))),
        };
        // SAFETY: the module is loaded and its entry function (if any) has run.
        let raw = unsafe { get() };
        // SAFETY: a non-null result is a factory pointer with one reference owned by the caller
        // (VST3 contract), which the `ComPtr` takes over.
        let factory = unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| fail("GetPluginFactory returned null".into()))?;
        Ok(Bundle { factory, exit, entered_on, leaked: AtomicBool::new(false), _lib: ManuallyDrop::new(lib) })
    }

    /// Releases the factory, then runs the module exit, as the VST3 SDK's own host does.
    fn exit(self) {
        let Bundle { factory, exit, .. } = self;
        drop(factory);
        if let Some(f) = exit {
            // SAFETY: balanced with the entry that ran when the module was loaded; called once
            // (`self` is consumed), after every instance is gone (each holds an `Arc` to the
            // bundle) and the factory released. The code stays mapped (`_lib` is never closed).
            unsafe { f() };
        }
    }

    /// Plugin objects of this module were leaked on purpose: never run its module exit.
    pub fn mark_leaked(&self) {
        self.leaked.store(true, Ordering::Release);
    }

    /// The vendor from the factory info (fallback for classes without one).
    fn factory_vendor(&self) -> String {
        // SAFETY: `PFactoryInfo` is plain data (char arrays and an int); all-zero is valid.
        let mut info: sb::PFactoryInfo = unsafe { std::mem::zeroed() };
        // SAFETY: valid factory; `info` is a writable struct of the right type.
        if ok(unsafe { self.factory.getFactoryInfo(&mut info) }) { read_c8(&info.vendor) } else { String::new() }
    }

    /// Every class this binary exports.
    pub fn classes(&self) -> Vec<RawClass> {
        // SAFETY: valid factory.
        let n = unsafe { self.factory.countClasses() }.clamp(0, MAX_CLASSES);
        let f2 = self.factory.cast::<sb::IPluginFactory2>();
        let vendor = self.factory_vendor();
        let mut out = Vec::new();
        for i in 0..n {
            if let Some(f2) = &f2 {
                // SAFETY: plain data; all-zero is valid.
                let mut c: sb::PClassInfo2 = unsafe { std::mem::zeroed() };
                // SAFETY: index below the count; `c` is writable.
                if ok(unsafe { f2.getClassInfo2(i, &mut c) }) {
                    let v = read_c8(&c.vendor);
                    out.push(RawClass {
                        cid: tuid_bytes(&c.cid),
                        category: read_c8(&c.category),
                        name: read_c8(&c.name),
                        vendor: if v.is_empty() { vendor.clone() } else { v },
                        version: read_c8(&c.version),
                        sdk_version: read_c8(&c.sdkVersion),
                        sub_categories: read_c8(&c.subCategories),
                    });
                    continue;
                }
            }
            // SAFETY: plain data; all-zero is valid.
            let mut c: sb::PClassInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `c` is writable.
            if ok(unsafe { self.factory.getClassInfo(i, &mut c) }) {
                out.push(RawClass {
                    cid: tuid_bytes(&c.cid),
                    category: read_c8(&c.category),
                    name: read_c8(&c.name),
                    vendor: vendor.clone(),
                    version: String::new(),
                    sdk_version: String::new(),
                    sub_categories: String::new(),
                });
            }
        }
        out
    }

    /// Creates an object of class `cid` and queries interface `I` on it.
    fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = std::ptr::null_mut();
        let iid = I::IID;
        // SAFETY: valid factory; `cid` and `iid` point to 16 readable bytes each (VST3 passes TUIDs
        // as `FIDString`); `obj` is writable.
        let r = unsafe { self.factory.createInstance(cid.as_ptr().cast::<c_char>(), iid.as_ptr().cast::<c_char>(), &mut obj) };
        if !ok(r) {
            return None;
        }
        // SAFETY: on success `obj` holds an `I` pointer with one reference owned by us.
        unsafe { ComPtr::from_raw(obj.cast::<I>()) }
    }

    /// Creates, initialises and wires up one plugin instance.
    pub fn instantiate(self: &Arc<Self>, cid: &[u8; 16]) -> Result<Instance, Vst3Error> {
        let name = hex(cid);
        let err = |m: &str| Vst3Error::Instantiate(name.clone(), m.to_string());
        let host = ComWrapper::new(HostContext::default());
        let host_unknown = host.as_com_ref::<sv::IHostApplication>().ok_or_else(|| err("host context"))?.upcast::<FUnknown>().as_ptr();
        let component = self.create::<sv::IComponent>(cid).ok_or_else(|| err("factory cannot create the component"))?;
        // SAFETY: fresh component; the host context outlives it (owned by the returned Instance).
        if !ok(unsafe { component.initialize(host_unknown) }) {
            return Err(err("component initialize failed"));
        }
        let mut inst = Instance {
            processor: None,
            controller: None,
            single: false,
            connections: None,
            component_initialized: true,
            controller_initialized: false,
            active: false,
            processing: false,
            max_frames: 0,
            in_params: ComWrapper::new(ParamChanges::new(PARAM_QUEUE_CAP, PARAM_POINTS)),
            out_params: ComWrapper::new(ParamChanges::new(PARAM_QUEUE_CAP, 0)),
            in_events: ComWrapper::new(EventList::new(EVENT_CAP)),
            out_events: ComWrapper::new(EventList::new(EVENT_CAP)),
            // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
            context: unsafe { std::mem::zeroed() },
            event_in: false,
            editor: Arc::new(EditorLink::default()),
            component,
            host,
            bundle: self.clone(),
        };
        let processor = inst.component.cast::<sv::IAudioProcessor>().ok_or_else(|| err("no IAudioProcessor"))?;
        inst.processor = Some(processor);
        // Edit controller: the component itself (single-component plugin) or a separate class.
        if let Some(c) = inst.component.cast::<sv::IEditController>() {
            inst.controller = Some(c);
            inst.single = true;
        } else {
            let mut ccid: sb::TUID = [0; 16];
            // SAFETY: valid component; `ccid` is a writable 16-byte TUID.
            if ok(unsafe { inst.component.getControllerClassId(&mut ccid) })
                && let Some(c) = self.create::<sv::IEditController>(&tuid_bytes(&ccid))
            {
                // SAFETY: fresh controller; the host context outlives it.
                if ok(unsafe { c.initialize(host_unknown) }) {
                    inst.controller = Some(c);
                    inst.controller_initialized = true;
                } else {
                    log::warn!("vst3 {name}: controller initialize failed; parameters unavailable");
                }
            }
        }
        if let Some(c) = &inst.controller {
            if !inst.single
                && let (Some(a), Some(b)) = (inst.component.cast::<sv::IConnectionPoint>(), c.cast::<sv::IConnectionPoint>())
            {
                // SAFETY: both connection points are valid for as long as `inst` lives; they are
                // disconnected in `Drop` before either side terminates.
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                inst.connections = Some((a, b));
            }
            inst.sync_controller_state();
            if let Some(h) = inst.host.as_com_ref::<sv::IComponentHandler>() {
                // SAFETY: valid controller; the handler (host context) outlives it.
                unsafe { c.setComponentHandler(h.as_ptr()) };
            }
        }
        inst.editor = Arc::new(EditorLink {
            main: std::thread::current().id(),
            inner: Mutex::new(EditorInner { component: Some(inst.component.clone()), controller: inst.controller.clone(), ..EditorInner::default() }),
            header: transit_rt::editor_window::HeaderCell::new(),
        });
        Ok(inst)
    }
}

/// A `TUID` (`[i8; 16]`) as raw bytes.
fn tuid_bytes(t: &sb::TUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (o, b) in out.iter_mut().zip(t.iter()) {
        *o = *b as u8;
    }
    out
}

/// Upper-case hex of a class id (the form used in `vst3:<hex>` ids).
pub(crate) fn hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

// ---- host-side COM objects -----------------------------------------------------------------

/// The host context handed to `initialize` (`IHostApplication`) and to the controller
/// (`IComponentHandler`: parameter edits made in the plugin's editor are queued for the UI, which
/// writes them into the session, from where they reach the processor).
///
/// `restartComponent` (a UI-thread call) only records its flags here; the main-thread service
/// (`plugin::Shared::service`) acts on them (audit F08, ADR 0012 G7).
pub(crate) struct HostContext {
    /// `kLatencyChanged`: deactivate, reactivate, then read the latency (main thread).
    pub latency_changed: AtomicBool,
    pub restart_requested: AtomicBool,
    /// `restartComponent` carried `kParamTitlesChanged` (curve shapes can change): the service
    /// invalidates the conversion curves, which the editor handle re-samples on the UI thread.
    pub titles_changed: AtomicBool,
    /// `restartComponent` carried `kParamValuesChanged` (values changed, curves did not; many
    /// plug-ins send it on every preset load): the host re-reads the values (audit F08), and the
    /// curves stay.
    pub values_changed: AtomicBool,
    /// `kMidiCCAssignmentChanged`: re-read the `IMidiMapping` table (main thread).
    pub midi_cc_changed: AtomicBool,
    /// `kIoChanged`: deactivate, renegotiate the buses, reactivate (main thread).
    pub io_changed: AtomicBool,
    /// `kReloadComponent`: not supported (the component would have to be created again).
    pub reload_requested: AtomicBool,
    /// Any other flags, or-ed (nothing this host needs to do; logged).
    pub other_flags: AtomicU32,
    /// `performEdit` values (normalized), bounded by `EDIT_CAP`.
    pub edits: Mutex<Vec<(u32, f64)>>,
}

impl Default for HostContext {
    fn default() -> Self {
        let edits = Mutex::new(Vec::with_capacity(EDIT_CAP));
        // Plug-ins are known to call `performEdit` from the audio thread despite the spec, and on
        // macOS the first lock boxes the pthread mutex (an allocation). Take it once here.
        drop(edits.lock());
        HostContext {
            latency_changed: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            titles_changed: AtomicBool::new(false),
            values_changed: AtomicBool::new(false),
            midi_cc_changed: AtomicBool::new(false),
            io_changed: AtomicBool::new(false),
            reload_requested: AtomicBool::new(false),
            other_flags: AtomicU32::new(0),
            edits,
        }
    }
}

impl HostContext {
    /// Takes the queued editor edits.
    pub fn take_edits(&self) -> Vec<(u32, f64)> {
        let mut e = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let out = e.clone();
        e.clear();
        out
    }
}

impl Class for HostContext {
    type Interfaces = (sv::IHostApplication, sv::IComponentHandler);
}

impl IHostApplicationTrait for HostContext {
    unsafe fn getName(&self, name: *mut sv::String128) -> sb::tresult {
        // SAFETY: the plugin passes a pointer to a writable String128 (or null, handled).
        match unsafe { name.as_mut() } {
            Some(n) => {
                write_c16("SoundCraft", n);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }

    /// Host-side `IMessage` and `IAttributeList` objects, which plug-ins whose component and
    /// controller talk through `IConnectionPoint` allocate here (the SDK's `allocateMessage`).
    /// A host must provide them; some plug-ins (iZotope Ozone 11) use the result unchecked.
    ///
    /// # Safety
    /// Non-null IDs must point to readable TUIDs; `obj` must be null or a distinct writable pointer.
    unsafe fn createInstance(&self, cid: *mut sb::TUID, iid: *mut sb::TUID, obj: *mut *mut c_void) -> sb::tresult {
        // SAFETY: `obj` is the plugin's out-pointer (or null, handled).
        let Some(out) = (unsafe { obj.as_mut() }) else { return sb::kInvalidArgument };
        *out = std::ptr::null_mut();
        // SAFETY: the plugin passes two readable 16-byte TUIDs (or null, handled).
        let (Some(cid), Some(iid)) = (unsafe { (cid.as_ref(), iid.as_ref()) }) else { return sb::kInvalidArgument };
        let (cid, iid) = (tuid_bytes(cid), tuid_bytes(iid));
        // Refuse unsupported pairs before allocating, including under memory pressure.
        let raw = if cid == sv::IMessage::IID && (iid == cid || iid == FUnknown::IID) {
            HostMessage::try_new().map(|p| p.into_raw().cast())
        } else if cid == sv::IAttributeList::IID && (iid == cid || iid == FUnknown::IID) {
            HostAttributeList::try_new().map(|p| p.into_raw().cast())
        } else {
            return sb::kResultFalse;
        };
        match raw {
            Ok(p) => {
                *out = p;
                sb::kResultOk
            }
            Err(e) => e,
        }
    }
}

impl IComponentHandlerTrait for HostContext {
    unsafe fn beginEdit(&self, _id: sv::ParamID) -> sb::tresult {
        sb::kResultOk
    }
    unsafe fn performEdit(&self, id: sv::ParamID, value: sv::ParamValue) -> sb::tresult {
        if value.is_finite() {
            let mut e = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
            let v = value.clamp(0.0, 1.0);
            if let Some(x) = e.iter_mut().find(|(i, _)| *i == id) {
                x.1 = v;
            } else if e.len() < EDIT_CAP {
                e.push((id, v));
            }
        }
        sb::kResultOk
    }
    unsafe fn endEdit(&self, _id: sv::ParamID) -> sb::tresult {
        sb::kResultOk
    }
    unsafe fn restartComponent(&self, flags: sb::int32) -> sb::tresult {
        if flags as u32 & sv::RestartFlags_::kLatencyChanged as u32 != 0 {
            self.latency_changed.store(true, Ordering::Relaxed);
        }
        if flags as u32 & sv::RestartFlags_::kParamTitlesChanged as u32 != 0 {
            self.titles_changed.store(true, Ordering::Relaxed);
        }
        if flags as u32 & sv::RestartFlags_::kParamValuesChanged as u32 != 0 {
            self.values_changed.store(true, Ordering::Relaxed);
        }
        if flags as u32 & sv::RestartFlags_::kMidiCCAssignmentChanged as u32 != 0 {
            self.midi_cc_changed.store(true, Ordering::Relaxed);
        }
        if flags as u32 & sv::RestartFlags_::kIoChanged as u32 != 0 {
            self.io_changed.store(true, Ordering::Relaxed);
        }
        if flags as u32 & sv::RestartFlags_::kReloadComponent as u32 != 0 {
            self.reload_requested.store(true, Ordering::Relaxed);
        }
        let handled = sv::RestartFlags_::kLatencyChanged as u32
            | sv::RestartFlags_::kParamTitlesChanged as u32
            | sv::RestartFlags_::kParamValuesChanged as u32
            | sv::RestartFlags_::kMidiCCAssignmentChanged as u32
            | sv::RestartFlags_::kIoChanged as u32
            | sv::RestartFlags_::kReloadComponent as u32;
        self.other_flags.fetch_or(flags as u32 & !handled, Ordering::Relaxed);
        self.restart_requested.store(true, Ordering::Release);
        sb::kResultOk
    }
}

// ComWrapper uses infallible Arc allocation. These two SDK objects instead allocate one
// header/data block fallibly, retaining the dependency's generated ABI thunks.
#[repr(C)]
struct HostObject<C: Class> {
    header: Header<C>,
    refs: AtomicUsize,
    data: C,
}

// fetch_update keeps this wrapper compatible with the crate's Rust 1.90 MSRV.
#[allow(deprecated)]
impl<C: Class> Wrapper<C> for HostObject<C> {
    /// # Safety
    /// `ptr` must be the header of a live allocation made by `try_host_object`.
    unsafe fn data_from_header(ptr: *mut Header<C>) -> *mut C {
        // SAFETY: repr(C) puts the header first; the allocation contains an initialized C.
        unsafe { std::ptr::addr_of_mut!((*ptr.cast::<Self>()).data) }
    }

    /// # Safety
    /// `ptr` must be the data member of a live allocation made by `try_host_object`.
    unsafe fn header_from_data(ptr: *mut C) -> *mut Header<C> {
        // SAFETY: subtracting the repr(C) member offset recovers the original allocation/header.
        unsafe { ptr.cast::<u8>().sub(std::mem::offset_of!(Self, data)).cast() }
    }

    /// # Safety
    /// `ptr` must name live data and the caller must hold a reference throughout the call.
    unsafe fn add_ref(ptr: *mut C) -> usize {
        // SAFETY: the caller's live reference keeps this header and atomic counter alive.
        let object = unsafe { &*Self::header_from_data(ptr).cast::<Self>() };
        // Saturate instead of wrapping. A saturated object is intentionally never freed.
        object.refs.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_add(1))).unwrap_or(usize::MAX).saturating_add(1)
    }

    /// # Safety
    /// `ptr` must name live data and the caller must surrender exactly one owned reference.
    unsafe fn release(ptr: *mut C) -> usize {
        // SAFETY: the surrendered reference keeps the allocation alive through the decrement.
        let allocation = unsafe { Self::header_from_data(ptr).cast::<Self>() };
        // SAFETY: allocation is live; only the atomic counter is accessed before the last release.
        let previous = unsafe { &(*allocation).refs }
            .fetch_update(Ordering::Release, Ordering::Relaxed, |n| if n == usize::MAX { None } else { n.checked_sub(1) });
        match previous {
            Ok(1) => {
                std::sync::atomic::fence(Ordering::Acquire);
                // SAFETY: this was the last reference. Drop once, then free with the allocation's
                // original layout; no references to the header or data are used afterward.
                unsafe {
                    std::ptr::drop_in_place(allocation);
                    std::alloc::dealloc(allocation.cast(), std::alloc::Layout::new::<Self>());
                }
                0
            }
            Ok(n) => n.saturating_sub(1),
            Err(n) => n,
        }
    }
}

// Test-only, thread-local failure countdown: no global allocator changes or real OOM.
#[cfg(test)]
thread_local! {
    static HOST_ALLOC_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn host_allocation() -> Result<(), sb::tresult> {
    #[cfg(test)]
    HOST_ALLOC_FAIL_AFTER.with(|remaining| match remaining.get() {
        Some(0) => Err(sb::kOutOfMemory),
        Some(n) => {
            remaining.set(Some(n - 1));
            Ok(())
        }
        None => Ok(()),
    })?;
    Ok(())
}

fn try_host_copy<T: Copy>(source: &[T]) -> Result<Vec<T>, sb::tresult> {
    let mut copy = Vec::new();
    if !source.is_empty() {
        host_allocation()?;
        copy.try_reserve_exact(source.len()).map_err(|_| sb::kOutOfMemory)?;
        copy.extend_from_slice(source);
    }
    Ok(copy)
}

/// Allocate a single-interface SDK object with one reference. Only the two matching static
/// vtables below are supplied here; each uses HostObject<C> and offset zero.
fn try_host_object<C, I>(data: C, vtbl: &'static I::Vtbl) -> Result<ComPtr<I>, sb::tresult>
where
    C: Class<Interfaces = (I,)> + 'static,
    I: Construct<C, HostObject<C>, 0>,
{
    host_allocation()?;
    let layout = std::alloc::Layout::new::<HostObject<C>>();
    // SAFETY: the header/counter make layout nonzero; null is handled before any write.
    let allocation = unsafe { std::alloc::alloc(layout) }.cast::<HostObject<C>>();
    if allocation.is_null() {
        return Err(sb::kOutOfMemory);
    }
    // SAFETY: allocation has the exact size/alignment of HostObject<C>. Its single-interface
    // header starts at offset zero, with a vtable pointer as required by Interface/MakeHeader.
    // The supplied static vtable retains the generated thunks for C and this wrapper, replacing
    // only QI. ComPtr takes the sole initial reference and release frees this exact allocation.
    unsafe {
        allocation.write(HostObject { header: <(I,) as MakeHeader<C, HostObject<C>>>::HEADER, refs: AtomicUsize::new(1), data });
        allocation.cast::<*const I::Vtbl>().write(vtbl);
        Ok(ComPtr::from_raw_unchecked(allocation.cast::<I>()))
    }
}

/// vst3 0.3.0's FUnknown QI neither clears failed outputs nor checks null arguments. Wrap that
/// third-party helper for the new objects; successful QI retains its ABI, identity and AddRef.
///
/// # Safety
/// `this` must be the live single-interface HostObject<C> header. Non-null `iid` must be
/// readable, and non-null `obj` must be a distinct writable pointer.
unsafe extern "system" fn host_query_interface<C: Class>(this: *mut FUnknown, iid: *const sb::TUID, obj: *mut *mut c_void) -> sb::tresult {
    if obj.is_null() {
        return sb::kInvalidArgument;
    }
    // SAFETY: the caller supplies a writable pointer, and null was rejected.
    unsafe { obj.write(std::ptr::null_mut()) };
    if iid.is_null() {
        return sb::kInvalidArgument;
    }
    // SAFETY: inputs are validated; the generated helper knows C's header and reference counter.
    unsafe { (FUnknown::make_vtbl::<C, HostObject<C>, 0>().queryInterface)(this, iid, obj) }
}

const ATTRIBUTE_VTBL: sv::IAttributeListVtbl = {
    // SAFETY: Construct supplies a static vtable with the SDK ABI for this C/wrapper/offset.
    let mut vtbl = unsafe { *<sv::IAttributeList as Construct<HostAttributeList, HostObject<HostAttributeList>, 0>>::OBJ.vtbl };
    vtbl.base.queryInterface = host_query_interface::<HostAttributeList>;
    vtbl
};
const MESSAGE_VTBL: sv::IMessageVtbl = {
    // SAFETY: Construct supplies a static vtable with the SDK ABI for this C/wrapper/offset.
    let mut vtbl = unsafe { *<sv::IMessage as Construct<HostMessage, HostObject<HostMessage>, 0>>::OBJ.vtbl };
    vtbl.base.queryInterface = host_query_interface::<HostMessage>;
    vtbl
};

/// One `IAttributeList` value.
enum Attr {
    Int(i64),
    Float(f64),
    /// UTF-16 without the terminator.
    String(Vec<u16>),
    Binary(Vec<u8>),
}

/// A host `IAttributeList` (created through `IHostApplication::createInstance`, or owned by a
/// [`HostMessage`]). Binary values handed out by `getBinary` stay valid until that attribute is
/// set again or the list is released, as with the SDK's own host classes.
#[derive(Default)]
pub(crate) struct HostAttributeList {
    attrs: Mutex<std::collections::HashMap<Vec<u8>, Attr>>,
}

impl Class for HostAttributeList {
    type Interfaces = (sv::IAttributeList,);
}

/// Borrow a terminated SDK string without allocation, rejecting oversized input.
///
/// # Safety
/// Non-null `ptr` must be readable through its first NUL (including that terminator) and
/// remain valid for the returned borrow. `max` must be less than isize::MAX / size_of::<T>().
unsafe fn host_string<'a, T: Copy + Default + PartialEq>(ptr: *const T, max: usize) -> Result<&'a [T], sb::tresult> {
    if ptr.is_null() {
        return Err(sb::kInvalidArgument);
    }
    for len in 0..=max {
        // SAFETY: the SDK supplies terminated input; we stop at its first terminator or cap.
        if unsafe { *ptr.add(len) } == T::default() {
            // SAFETY: these len elements precede the terminator and fit within isize::MAX.
            return Ok(unsafe { std::slice::from_raw_parts(ptr, len) });
        }
    }
    Err(sb::kResultFalse)
}

impl HostAttributeList {
    fn try_new() -> Result<ComPtr<sv::IAttributeList>, sb::tresult> {
        try_host_object(Self::default(), &ATTRIBUTE_VTBL)
    }

    /// # Safety
    /// `id` must be null or a readable NUL-terminated SDK attribute ID for this call.
    unsafe fn set(&self, id: sv::IAttributeList_::AttrID, v: Attr) -> sb::tresult {
        // SAFETY: forwarded SDK string contract; keys are only borrowed during this call.
        let key = match unsafe { host_string(id.cast::<u8>(), ATTR_STRING_MAX) } {
            Ok(k) => k,
            Err(e) => return e,
        };
        let mut attrs = self.attrs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = attrs.get_mut(key) {
            *old = v;
            return sb::kResultOk;
        }
        if attrs.len() >= ATTR_COUNT_MAX {
            return sb::kOutOfMemory;
        }
        let key = match try_host_copy(key) {
            Ok(k) => k,
            Err(e) => return e,
        };
        if attrs.len() == attrs.capacity() && (host_allocation().is_err() || attrs.try_reserve(1).is_err()) {
            return sb::kOutOfMemory;
        }
        attrs.insert(key, v);
        sb::kResultOk
    }

    /// # Safety
    /// `id` must be null or a readable NUL-terminated SDK attribute ID for this call.
    unsafe fn get<T>(&self, id: sv::IAttributeList_::AttrID, f: impl FnOnce(&Attr) -> Option<T>) -> Result<T, sb::tresult> {
        // SAFETY: forwarded SDK string contract; the closure cannot retain the key or value borrow.
        let key = unsafe { host_string(id.cast::<u8>(), ATTR_STRING_MAX) }?;
        self.attrs.lock().unwrap_or_else(PoisonError::into_inner).get(key).and_then(f).ok_or(sb::kResultFalse)
    }
}

/// Maximum accepted text length (excluding NUL), binary size, and distinct attribute count.
const ATTR_STRING_MAX: usize = 1 << 20;
const ATTR_BINARY_MAX: usize = 64 << 20;
const ATTR_COUNT_MAX: usize = 1 << 16;

impl IAttributeListTrait for HostAttributeList {
    /// # Safety
    /// `id` must be null or a readable NUL-terminated SDK attribute ID.
    unsafe fn setInt(&self, id: sv::IAttributeList_::AttrID, value: i64) -> sb::tresult {
        // SAFETY: forwards the SDK attribute ID contract.
        unsafe { self.set(id, Attr::Int(value)) }
    }
    /// # Safety
    /// `id` must be null or terminated; `value` must be null or a distinct writable i64.
    unsafe fn getInt(&self, id: sv::IAttributeList_::AttrID, value: *mut i64) -> sb::tresult {
        // SAFETY: the plugin's out-pointer (or null, handled).
        let Some(out) = (unsafe { value.as_mut() }) else { return sb::kInvalidArgument };
        // SAFETY: forwards the SDK attribute ID contract; only the scalar is copied.
        match unsafe { self.get(id, |a| if let Attr::Int(v) = a { Some(*v) } else { None }) } {
            Ok(v) => {
                *out = v;
                sb::kResultOk
            }
            Err(e) => e,
        }
    }
    /// # Safety
    /// `id` must be null or a readable NUL-terminated SDK attribute ID.
    unsafe fn setFloat(&self, id: sv::IAttributeList_::AttrID, value: f64) -> sb::tresult {
        // SAFETY: forwards the SDK attribute ID contract.
        unsafe { self.set(id, Attr::Float(value)) }
    }
    /// # Safety
    /// `id` must be null or terminated; `value` must be null or a distinct writable f64.
    unsafe fn getFloat(&self, id: sv::IAttributeList_::AttrID, value: *mut f64) -> sb::tresult {
        // SAFETY: the plugin's out-pointer (or null, handled).
        let Some(out) = (unsafe { value.as_mut() }) else { return sb::kInvalidArgument };
        // SAFETY: forwards the SDK attribute ID contract; only the scalar is copied.
        match unsafe { self.get(id, |a| if let Attr::Float(v) = a { Some(*v) } else { None }) } {
            Ok(v) => {
                *out = v;
                sb::kResultOk
            }
            Err(e) => e,
        }
    }
    /// # Safety
    /// `id` and `string` must be null or readable NUL-terminated SDK strings.
    unsafe fn setString(&self, id: sv::IAttributeList_::AttrID, string: *const sv::TChar) -> sb::tresult {
        if id.is_null() {
            return sb::kInvalidArgument;
        }
        // SAFETY: the SDK supplies a terminated UTF-16 string; the helper rejects null/oversize.
        let source = match unsafe { host_string(string, ATTR_STRING_MAX) } {
            Ok(s) => s,
            Err(e) => return e,
        };
        let copy = match try_host_copy(source) {
            Ok(s) => s,
            Err(e) => return e,
        };
        // SAFETY: forwards the SDK attribute ID contract after successfully copying the value.
        unsafe { self.set(id, Attr::String(copy)) }
    }
    /// # Safety
    /// `id` must be null or terminated; `string` must be null or an aligned, distinct writable
    /// buffer of size_in_bytes bytes. It must not alias this list's stored values.
    unsafe fn getString(&self, id: sv::IAttributeList_::AttrID, string: *mut sv::TChar, size_in_bytes: u32) -> sb::tresult {
        let units = size_in_bytes as usize / 2;
        if string.is_null() || units == 0 || units > isize::MAX as usize / 2 {
            return sb::kInvalidArgument;
        }
        // SAFETY: the plugin supplies the writable UTF-16 buffer; its length fits isize::MAX.
        let dst = unsafe { std::slice::from_raw_parts_mut(string, units) };
        // SAFETY: forwards the SDK ID contract; copies while the lock keeps storage stable.
        unsafe {
            self.get(id, |a| {
                if let Attr::String(s) = a {
                    write_c16_units(s, dst);
                    Some(())
                } else {
                    None
                }
            })
        }
        .map_or_else(|e| e, |()| sb::kResultOk)
    }
    /// # Safety
    /// `id` must be null or terminated. Nonempty `data` must have size_in_bytes readable bytes;
    /// rejected oversized buffers are never read.
    unsafe fn setBinary(&self, id: sv::IAttributeList_::AttrID, data: *const c_void, size_in_bytes: u32) -> sb::tresult {
        if id.is_null() {
            return sb::kInvalidArgument;
        }
        let len = size_in_bytes as usize;
        if len > ATTR_BINARY_MAX {
            return sb::kOutOfMemory;
        }
        let source = if len == 0 {
            &[]
        } else if data.is_null() {
            return sb::kInvalidArgument;
        } else {
            // SAFETY: the SDK supplies len readable bytes; the cap also fits isize::MAX on 32-bit.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }
        };
        let bytes = match try_host_copy(source) {
            Ok(b) => b,
            Err(e) => return e,
        };
        // SAFETY: forwards the SDK attribute ID contract after successfully copying the bytes.
        unsafe { self.set(id, Attr::Binary(bytes)) }
    }
    /// # Safety
    /// `id` must be null or terminated; non-null output pointers must be aligned, writable and
    /// distinct. Borrowed data must not be used concurrently with replacement/final release.
    unsafe fn getBinary(&self, id: sv::IAttributeList_::AttrID, data: *mut *const c_void, size_in_bytes: *mut u32) -> sb::tresult {
        // SAFETY: the plugin's out-pointers (or null, handled).
        let (Some(d), Some(n)) = (unsafe { (data.as_mut(), size_in_bytes.as_mut()) }) else { return sb::kInvalidArgument };
        // The pointer stays valid after the lock is released: the vector is untouched until the
        // attribute is replaced or the list is released (the SDK's contract).
        // SAFETY: forwards the SDK ID contract; the caller synchronizes borrowed pointer use.
        match unsafe { self.get(id, |a| if let Attr::Binary(b) = a { Some((b.as_ptr().cast::<c_void>(), b.len())) } else { None }) } {
            Ok((p, len)) => {
                *d = p;
                *n = u32::try_from(len).unwrap_or(u32::MAX);
                sb::kResultOk
            }
            Err(e) => {
                *d = std::ptr::null();
                *n = 0;
                e
            }
        }
    }
}

/// Copies UTF-16 `src` into `dst` with a terminator, truncating to fit.
fn write_c16_units(src: &[u16], dst: &mut [u16]) {
    let Some(room) = dst.len().checked_sub(1) else { return };
    let n = src.len().min(room);
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}

/// A host `IMessage`: an id plus its own attribute list.
pub(crate) struct HostMessage {
    /// NUL-terminated bytes, valid until the next successful setMessageID or final release.
    id: Mutex<Option<Vec<u8>>>,
    attrs: ComPtr<sv::IAttributeList>,
}

impl HostMessage {
    fn try_new() -> Result<ComPtr<sv::IMessage>, sb::tresult> {
        let attrs = HostAttributeList::try_new()?;
        try_host_object(HostMessage { id: Mutex::new(None), attrs }, &MESSAGE_VTBL)
    }
}

impl Class for HostMessage {
    type Interfaces = (sv::IMessage,);
}

impl IMessageTrait for HostMessage {
    /// # Safety
    /// The returned borrowed ID must not be used concurrently with replacement/final release.
    unsafe fn getMessageID(&self) -> sb::FIDString {
        self.id.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or(std::ptr::null(), |s| s.as_ptr().cast())
    }
    /// # Safety
    /// `id` must be null or a readable NUL-terminated SDK string for this call.
    unsafe fn setMessageID(&self, id: sb::FIDString) {
        let new = if id.is_null() {
            None
        } else {
            // SAFETY: the SDK supplies a terminated ID; the helper checks null and the length cap.
            let Ok(source) = (unsafe { host_string(id.cast::<u8>(), ATTR_STRING_MAX) }) else { return };
            // SAFETY: source excludes the terminator, which the helper read; include it in the copy.
            let source = unsafe { std::slice::from_raw_parts(id.cast::<u8>(), source.len() + 1) };
            // The SDK method returns void: preserve the prior ID on failure. Copy first so
            // assigning our own borrowed ID is also valid.
            let Ok(copy) = try_host_copy(source) else { return };
            Some(copy)
        };
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = new;
    }
    /// # Safety
    /// The returned pointer is borrowed for the message lifetime; callers must AddRef to retain it.
    unsafe fn getAttributes(&self) -> *mut sv::IAttributeList {
        self.attrs.as_ptr()
    }
}

/// One parameter point the host sends: its time in the block, the parameter, its class
/// (0: automation and host values, 1: a mapped MIDI controller, which follows automation at the
/// same offset), the order it was set in, and the normalized value. `release`: a controller
/// from a release-class message ([`Message::survives_reset`](soundcraft_dsp::event::Message)),
/// which the host keeps through a plug-in reset.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct ParamPoint {
    pub time: u32,
    pub id: u32,
    pub class: u8,
    pub seq: u32,
    pub value: f64,
    pub release: bool,
}

/// A queue's points live inline (one point a plug-in wrote), not in the slab.
const INLINE: u32 = u32::MAX;

/// The points one block carries: (offset, value bits).
type Slab = Arc<[(AtomicI32, AtomicU64)]>;

/// One parameter's points for one block: a contiguous range of its [`ParamChanges`]'s slab,
/// ascending by offset (host → plug-in), or one inline point (a plug-in's output changes; only the
/// last one it adds is kept).
pub(crate) struct ParamQueue {
    id: AtomicU32,
    /// First slab point, or [`INLINE`].
    start: AtomicU32,
    points: AtomicI32,
    offset: AtomicI32,
    value: AtomicU64,
    slab: Slab,
}

impl ParamQueue {
    fn new(slab: Slab) -> ParamQueue {
        ParamQueue {
            id: AtomicU32::new(0),
            start: AtomicU32::new(INLINE),
            points: AtomicI32::new(0),
            offset: AtomicI32::new(0),
            value: AtomicU64::new(0),
            slab,
        }
    }

    /// Point `index`: (offset, value).
    fn point(&self, index: i32) -> Option<(i32, f64)> {
        if index < 0 || index >= self.points.load(Ordering::Relaxed) {
            return None;
        }
        let start = self.start.load(Ordering::Relaxed);
        if start == INLINE {
            return (index == 0).then(|| (self.offset.load(Ordering::Relaxed), f64::from_bits(self.value.load(Ordering::Relaxed))));
        }
        let (o, v) = self.slab.get((start as usize).checked_add(index as usize)?)?;
        Some((o.load(Ordering::Relaxed), f64::from_bits(v.load(Ordering::Relaxed))))
    }
}

impl Class for ParamQueue {
    type Interfaces = (sv::IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> sv::ParamID {
        self.id.load(Ordering::Relaxed)
    }
    unsafe fn getPointCount(&self) -> sb::int32 {
        self.points.load(Ordering::Relaxed)
    }
    unsafe fn getPoint(&self, index: sb::int32, offset: *mut sb::int32, value: *mut sv::ParamValue) -> sb::tresult {
        let Some((at, v)) = self.point(index) else { return sb::kInvalidArgument };
        // SAFETY: out-pointers from the plugin (or null, handled).
        if let (Some(o), Some(out)) = (unsafe { offset.as_mut() }, unsafe { value.as_mut() }) {
            *o = at;
            *out = v;
            sb::kResultOk
        } else {
            sb::kInvalidArgument
        }
    }
    unsafe fn addPoint(&self, offset: sb::int32, value: sv::ParamValue, index: *mut sb::int32) -> sb::tresult {
        self.offset.store(offset, Ordering::Relaxed);
        self.value.store(value.to_bits(), Ordering::Relaxed);
        self.start.store(INLINE, Ordering::Relaxed);
        self.points.store(1, Ordering::Relaxed);
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(i) = unsafe { index.as_mut() } {
            *i = 0;
        }
        sb::kResultOk
    }
}

/// A fixed pool of parameter queues (`IParameterChanges`) and the slab of their points, reused
/// every block. Nothing here allocates after `new`.
pub(crate) struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    count: AtomicI32,
    slab: Slab,
    /// Slab points handed out this block.
    used: AtomicUsize,
}

impl Class for ParamChanges {
    type Interfaces = (sv::IParameterChanges,);
}

impl ParamChanges {
    /// `cap` queues sharing a slab of `points` points (0 for output changes, which keep one
    /// inline point per queue).
    fn new(cap: usize, points: usize) -> ParamChanges {
        let slab: Slab = (0..points).map(|_| (AtomicI32::new(0), AtomicU64::new(0))).collect::<Vec<_>>().into();
        ParamChanges {
            queues: (0..cap).map(|_| ComWrapper::new(ParamQueue::new(Arc::clone(&slab)))).collect(),
            count: AtomicI32::new(0),
            slab,
            used: AtomicUsize::new(0),
        }
    }

    fn clear(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.used.store(0, Ordering::Relaxed);
    }

    /// Finds or adds the queue for `id`; `None` when the pool is full.
    fn queue_for(&self, id: u32) -> Option<(i32, &ParamQueue)> {
        let n = self.count.load(Ordering::Relaxed).max(0);
        for (i, q) in self.queues.iter().enumerate().take(n as usize) {
            if q.id.load(Ordering::Relaxed) == id {
                return Some((i32::try_from(i).ok()?, q));
            }
        }
        let q = self.queues.get(n as usize)?;
        q.id.store(id, Ordering::Relaxed);
        q.start.store(INLINE, Ordering::Relaxed);
        q.points.store(0, Ordering::Relaxed);
        self.count.store(n + 1, Ordering::Relaxed);
        Some((n, q))
    }

    /// Host side: this block's points (times relative to `offset`, all inside the block) become
    /// one queue per parameter with its points ascending by offset. At one offset the last point
    /// wins (a controller after automation). When the slab runs out, a parameter keeps only its
    /// last point. Sorts `points` in place (`sort_unstable_by_key`: no allocation). Returns the
    /// points dropped (slab or queue pool exhausted).
    pub(crate) fn fill(&self, points: &mut [ParamPoint], offset: u32) -> u32 {
        self.clear();
        points.sort_unstable_by_key(|p| (p.id, p.time, p.class, p.seq));
        let mut groups = points.chunk_by(|a, b| a.id == b.id).count();
        let mut dropped = 0u32;
        for group in points.chunk_by(|a, b| a.id == b.id) {
            groups -= 1;
            let Some(first) = group.first() else { continue };
            // The last point at each offset.
            let last_at = |k: usize| group.get(k + 1).is_none_or(|n| n.time != group.get(k).map_or(0, |p| p.time));
            let distinct = (0..group.len()).filter(|&k| last_at(k)).count();
            let Some((_, q)) = self.queue_for(first.id) else {
                dropped = dropped.saturating_add(distinct as u32);
                continue;
            };
            let start = self.used.load(Ordering::Relaxed);
            // Keep one slab point for each parameter still to come.
            let room = self.slab.len().saturating_sub(start).saturating_sub(groups);
            let all = distinct <= room;
            let mut w = 0usize;
            for (k, p) in group.iter().enumerate() {
                if !last_at(k) || (!all && k + 1 != group.len()) {
                    continue;
                }
                let Some((o, v)) = self.slab.get(start + w) else { break };
                o.store(i32::try_from(p.time.saturating_sub(offset)).unwrap_or(i32::MAX), Ordering::Relaxed);
                v.store(p.value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
                w += 1;
            }
            dropped = dropped.saturating_add((distinct - w) as u32);
            q.start.store(u32::try_from(start).unwrap_or(INLINE), Ordering::Relaxed);
            q.points.store(i32::try_from(w).unwrap_or(0), Ordering::Relaxed);
            self.used.store(start + w, Ordering::Relaxed);
        }
        dropped
    }

    fn queue_ptr(&self, index: usize) -> *mut sv::IParamValueQueue {
        self.queues.get(index).and_then(|q| q.as_com_ref::<sv::IParamValueQueue>()).map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> sb::int32 {
        self.count.load(Ordering::Relaxed)
    }
    unsafe fn getParameterData(&self, index: sb::int32) -> *mut sv::IParamValueQueue {
        if index < 0 || index >= self.count.load(Ordering::Relaxed) {
            return std::ptr::null_mut();
        }
        self.queue_ptr(index as usize)
    }
    unsafe fn addParameterData(&self, id: *const sv::ParamID, index: *mut sb::int32) -> *mut sv::IParamValueQueue {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&id) = (unsafe { id.as_ref() }) else { return std::ptr::null_mut() };
        let Some((i, _)) = self.queue_for(id) else { return std::ptr::null_mut() };
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(out) = unsafe { index.as_mut() } {
            *out = i;
        }
        self.queue_ptr(i as usize)
    }
}

/// A bounded event list (`IEventList`). Only ever touched by one thread at a time (the host
/// fills it, then the plugin reads it inside `process`), so the lock is never contended; the
/// plugin side uses `try_lock` and never blocks.
pub(crate) struct EventList {
    events: Mutex<Vec<sv::Event>>,
    cap: usize,
}

impl Class for EventList {
    type Interfaces = (sv::IEventList,);
}

impl EventList {
    fn new(cap: usize) -> EventList {
        let list = EventList { events: Mutex::new(Vec::with_capacity(cap)), cap };
        // On macOS the first lock boxes the mutex's pthread_mutex_t (64 bytes). Take it once here,
        // on the control thread, so `process` never allocates (see the audio-thread knowledge note).
        drop(list.events.lock());
        list
    }
    fn with<R>(&self, f: impl FnOnce(&mut Vec<sv::Event>) -> R) -> Option<R> {
        match self.events.try_lock() {
            Ok(mut g) => Some(f(&mut g)),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(f(&mut p.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> sb::int32 {
        self.with(|v| i32::try_from(v.len()).unwrap_or(i32::MAX)).unwrap_or(0)
    }
    unsafe fn getEvent(&self, index: sb::int32, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: out-pointer from the plugin (or null, handled).
        let Some(out) = (unsafe { e.as_mut() }) else { return sb::kInvalidArgument };
        let Ok(i) = usize::try_from(index) else { return sb::kInvalidArgument };
        match self.with(|v| v.get(i).copied()).flatten() {
            Some(ev) => {
                *out = ev;
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&ev) = (unsafe { e.as_ref() }) else { return sb::kInvalidArgument };
        let cap = self.cap;
        // Output events (e.g. MIDI out) are accepted while there is room; SoundCraft ignores them.
        match self.with(|v| {
            if v.len() < cap {
                v.push(ev);
                true
            } else {
                false
            }
        }) {
            Some(true) => sb::kResultOk,
            _ => sb::kResultFalse,
        }
    }
}

/// An in-memory `IBStream`, used to hand the component's state to its controller.
pub(crate) struct MemoryStream {
    inner: Mutex<(Vec<u8>, usize)>,
}

impl Class for MemoryStream {
    type Interfaces = (sb::IBStream,);
}

impl MemoryStream {
    fn new(data: Vec<u8>) -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream { inner: Mutex::new((data, 0)) })
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.lock().0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Vec<u8>, usize)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: sb::int32, num_read: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let want = usize::try_from(num_bytes).unwrap_or(0);
        let avail = data.len().saturating_sub(*pos);
        let n = want.min(avail);
        if n > 0 && !buffer.is_null() {
            let Some(src) = data.get(*pos..*pos + n) else { return sb::kInternalError };
            // SAFETY: the plugin provides `buffer` with room for `num_bytes` ≥ `n` bytes; `src`
            // is `n` valid bytes; the regions cannot overlap (ours is a private Vec).
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buffer.cast::<u8>(), n) };
            *pos += n;
        }
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { num_read.as_mut() } {
            *r = i32::try_from(n).unwrap_or(0);
        }
        sb::kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: sb::int32, num_written: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let n = usize::try_from(num_bytes).unwrap_or(0);
        let end = pos.saturating_add(n);
        if buffer.is_null() || end > MAX_STATE_BYTES {
            return sb::kResultFalse;
        }
        if data.len() < end {
            data.resize(end, 0);
        }
        // SAFETY: the plugin provides `n` readable bytes at `buffer`; the destination range was
        // just made valid.
        let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        if let Some(dst) = data.get_mut(*pos..end) {
            dst.copy_from_slice(src);
        }
        *pos = end;
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(w) = unsafe { num_written.as_mut() } {
            *w = num_bytes;
        }
        sb::kResultOk
    }
    unsafe fn seek(&self, pos: sb::int64, mode: sb::int32, result: *mut sb::int64) -> sb::tresult {
        let mut g = self.lock();
        let len = i64::try_from(g.0.len()).unwrap_or(i64::MAX);
        let cur = i64::try_from(g.1).unwrap_or(0);
        #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
        let base = match mode as u32 {
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekSet as u32 => 0,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekCur as u32 => cur,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekEnd as u32 => len,
            _ => return sb::kInvalidArgument,
        };
        let new = base.saturating_add(pos).clamp(0, len);
        g.1 = usize::try_from(new).unwrap_or(0);
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { result.as_mut() } {
            *r = new;
        }
        sb::kResultOk
    }
    unsafe fn tell(&self, pos: *mut sb::int64) -> sb::tresult {
        let g = self.lock();
        // SAFETY: out-pointer from the plugin (or null, handled).
        match unsafe { pos.as_mut() } {
            Some(p) => {
                *p = i64::try_from(g.1).unwrap_or(0);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
}

// ---- events and buffers --------------------------------------------------------------------

/// A host → plugin note event, safe to build anywhere; converted inside `process`. `channel` is
/// the MIDI channel (0..=15) and `id` the host's voice id (`-1`: none), as VST3's `noteId`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NoteEvent {
    On { time: u32, key: u8, velocity: f32, channel: u8, id: i32 },
    Off { time: u32, key: u8, velocity: f32, channel: u8, id: i32 },
    Pressure { time: u32, key: u8, pressure: f32, channel: u8, id: i32 },
}

impl NoteEvent {
    pub fn time(&self) -> u32 {
        match *self {
            NoteEvent::On { time, .. } | NoteEvent::Off { time, .. } | NoteEvent::Pressure { time, .. } => time,
        }
    }
    pub fn set_time(&mut self, t: u32) {
        match self {
            NoteEvent::On { time, .. } | NoteEvent::Off { time, .. } | NoteEvent::Pressure { time, .. } => *time = t,
        }
    }

    fn to_raw(self, offset: u32) -> sv::Event {
        // SAFETY: `Event` is plain data (numbers and a union of plain structs); all-zero is valid.
        let mut e: sv::Event = unsafe { std::mem::zeroed() };
        e.busIndex = 0;
        e.sampleOffset = i32::try_from(self.time().saturating_sub(offset)).unwrap_or(0);
        e.flags = sv::Event_::EventFlags_::kIsLive as u16;
        match self {
            NoteEvent::On { key, velocity, channel, id, .. } => {
                e.r#type = sv::Event_::EventTypes_::kNoteOnEvent as u16;
                e.__field0.noteOn =
                    sv::NoteOnEvent { channel: i16::from(channel & 0x0F), pitch: i16::from(key), tuning: 0.0, velocity, length: 0, noteId: id };
            }
            NoteEvent::Off { key, velocity, channel, id, .. } => {
                e.r#type = sv::Event_::EventTypes_::kNoteOffEvent as u16;
                e.__field0.noteOff =
                    sv::NoteOffEvent { channel: i16::from(channel & 0x0F), pitch: i16::from(key), velocity, noteId: id, tuning: 0.0 };
            }
            NoteEvent::Pressure { key, pressure, channel, id, .. } => {
                e.r#type = sv::Event_::EventTypes_::kPolyPressureEvent as u16;
                e.__field0.polyPressure = sv::PolyPressureEvent { channel: i16::from(channel & 0x0F), pitch: i16::from(key), pressure, noteId: id };
            }
        }
        e
    }
}

/// Planar f32 buffers for every audio bus in one direction, with the pointer tables VST3 needs.
/// Allocated in `new`; `process` only rewrites pointers in place (no allocation).
pub(crate) struct BusBuffers {
    data: Vec<Vec<Vec<f32>>>,
    ptrs: Vec<Vec<*mut f32>>,
    bufs: Vec<sv::AudioBusBuffers>,
    frames: usize,
    /// Every channel of the next block is silent: `refresh` sets each bus's `silenceFlags` to all ones.
    silent: bool,
    /// Per-bus silence set by [`BusBuffers::set_bus_silent`]; wins over `silent` for that bus.
    bus_silent: Vec<Option<bool>>,
}

// SAFETY: the raw pointers only ever point into `data`, which this struct owns; moving the struct
// to another thread moves that ownership with it.
unsafe impl Send for BusBuffers {}

impl BusBuffers {
    /// One buffer set per bus, `channels[b]` channels each (capped), `frames` long.
    pub fn new(channels: &[u32], frames: usize) -> BusBuffers {
        let data: Vec<Vec<Vec<f32>>> =
            channels.iter().take(MAX_BUSES as usize).map(|&c| (0..c.min(MAX_BUS_CHANNELS)).map(|_| vec![0.0; frames]).collect()).collect();
        let ptrs = data.iter().map(|p| vec![std::ptr::null_mut(); p.len()]).collect();
        let bufs = data
            .iter()
            .map(|_| sv::AudioBusBuffers {
                numChannels: 0,
                silenceFlags: 0,
                __field0: sv::AudioBusBuffers__type0 { channelBuffers32: std::ptr::null_mut() },
            })
            .collect();
        let bus_silent = vec![None; data.len()];
        BusBuffers { data, ptrs, bufs, frames, silent: false, bus_silent }
    }

    pub fn channels(&self, bus: usize) -> usize {
        self.data.get(bus).map_or(0, Vec::len)
    }

    pub fn channel(&self, bus: usize, ch: usize) -> Option<&[f32]> {
        self.data.get(bus).and_then(|p| p.get(ch)).map(Vec::as_slice)
    }

    pub fn channel_mut(&mut self, bus: usize, ch: usize) -> Option<&mut [f32]> {
        self.data.get_mut(bus).and_then(|p| p.get_mut(ch)).map(Vec::as_mut_slice)
    }

    /// Flags every channel of the next block silent, or clears the flags (every bus, including
    /// any set by [`BusBuffers::set_bus_silent`]).
    pub fn set_silent(&mut self, silent: bool) {
        self.silent = silent;
        self.bus_silent.iter_mut().for_each(|b| *b = None);
    }

    /// Flags one bus silent, or clears its flags, whatever [`BusBuffers::set_silent`] said.
    pub fn set_bus_silent(&mut self, bus: usize, silent: bool) {
        if let Some(b) = self.bus_silent.get_mut(bus) {
            *b = Some(silent);
        }
    }

    /// The planar channels of one bus.
    pub fn bus_mut(&mut self, bus: usize) -> Option<&mut [Vec<f32>]> {
        self.data.get_mut(bus).map(Vec::as_mut_slice)
    }

    /// Points the VST3 tables at the current data (no allocation).
    fn refresh(&mut self) {
        for (((d, p), b), own) in self.data.iter_mut().zip(self.ptrs.iter_mut()).zip(self.bufs.iter_mut()).zip(&self.bus_silent) {
            let silent = own.unwrap_or(self.silent);
            for (dc, pc) in d.iter_mut().zip(p.iter_mut()) {
                *pc = dc.as_mut_ptr();
            }
            b.numChannels = i32::try_from(p.len()).unwrap_or(0);
            b.silenceFlags = if silent { silence_flags(p.len()) } else { 0 };
            b.__field0.channelBuffers32 = p.as_mut_ptr();
        }
    }
}

/// `AudioBusBuffers::silenceFlags` with every one of `channels` channels silent (one bit per
/// channel; all 64 from 64 channels up).
pub(crate) fn silence_flags(channels: usize) -> u64 {
    u32::try_from(channels).ok().and_then(|c| 1u64.checked_shl(c)).map_or(u64::MAX, |b| b.wrapping_sub(1))
}

// ---- instances -----------------------------------------------------------------------------

/// One parameter as reported by `IEditController::getParameterInfo`, plus its plain range.
#[derive(Debug, Clone)]
pub(crate) struct RawParam {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub step_count: i32,
    pub default_normalized: f64,
    pub flags: i32,
}

pub(crate) const PARAM_HIDDEN: i32 = sv::ParameterInfo_::ParameterFlags_::kIsHidden;
pub(crate) const PARAM_READ_ONLY: i32 = sv::ParameterInfo_::ParameterFlags_::kIsReadOnly;
pub(crate) const PARAM_BYPASS: i32 = sv::ParameterInfo_::ParameterFlags_::kIsBypass;

/// The audio bus layout chosen by [`Instance::configure`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Layout {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub event_input: bool,
    /// The activated auxiliary (sidechain) audio input bus, when the plug-in declares one and
    /// accepted an arrangement for it.
    pub key_bus: Option<usize>,
}

fn apply_transport(c: &mut sv::ProcessContext, t: &Transport) {
    use sv::ProcessContext_::StatesAndFlags_::*;
    let mut state = kTempoValid | kTimeSigValid | kProjectTimeMusicValid | kBarPositionValid | kContTimeValid;
    if t.playing {
        state |= kPlaying;
    }
    if t.recording {
        state |= kRecording;
    }
    // The flag constants are the C default enum type: i32 on Windows, u32 elsewhere.
    #[allow(clippy::unnecessary_cast)]
    let state = state as u32;
    c.state = state;
    c.tempo = t.tempo;
    c.timeSigNumerator = i32::try_from(t.numerator).unwrap_or(4);
    c.timeSigDenominator = i32::try_from(t.denominator).unwrap_or(4);
    c.projectTimeSamples = t.samples;
    c.projectTimeMusic = t.quarters;
    c.barPositionMusic = t.bar_start;
}

/// A live plugin: component + audio processor (+ edit controller) and the host objects.
pub(crate) struct Instance {
    processor: Option<ComPtr<sv::IAudioProcessor>>,
    controller: Option<ComPtr<sv::IEditController>>,
    /// The controller is the component itself (single-component plugin).
    single: bool,
    connections: Option<(ComPtr<sv::IConnectionPoint>, ComPtr<sv::IConnectionPoint>)>,
    component_initialized: bool,
    controller_initialized: bool,
    active: bool,
    processing: bool,
    max_frames: usize,
    in_params: ComWrapper<ParamChanges>,
    out_params: ComWrapper<ParamChanges>,
    in_events: ComWrapper<EventList>,
    out_events: ComWrapper<EventList>,
    context: sv::ProcessContext,
    event_in: bool,
    editor: Arc<EditorLink>,
    // Field order matters for drop: `Drop::drop` shuts the plugin down first; then the component
    // and host objects are released, and the bundle (which keeps the code mapped) goes last.
    component: ComPtr<sv::IComponent>,
    host: ComWrapper<HostContext>,
    bundle: Arc<Bundle>,
}

// SAFETY: an `Instance` is used through `&mut self` only, so calls into the plugin are always
// serialised. SoundCraft creates instances on the engine/UI thread and then moves them into the
// mix engine (documented limitation: VST3 expects `setActive` and `getLatencySamples` on the UI
// thread; they may then happen on the audio thread). Edit-controller calls never do: parameter
// values reach the processor converted through curves sampled on the UI thread (`params.rs`).
unsafe impl Send for Instance {}

impl Instance {
    fn processor(&self) -> Option<&ComPtr<sv::IAudioProcessor>> {
        self.processor.as_ref()
    }

    /// A counted reference to the host context (editor handles read its edit queue).
    pub fn host_ref(&self) -> ComWrapper<HostContext> {
        self.host.clone()
    }

    /// The editor link shared with editor handles (`None` without an edit controller).
    pub fn editor(&self) -> Option<Arc<EditorLink>> {
        self.editor.has_controller().then(|| Arc::clone(&self.editor))
    }

    /// The same link, for state handles.
    pub fn state_link(&self) -> Arc<EditorLink> {
        Arc::clone(&self.editor)
    }

    /// Writes one state into a fresh memory stream.
    fn read_state(f: impl FnOnce(*mut sb::IBStream) -> sb::tresult) -> Option<Vec<u8>> {
        let stream = MemoryStream::new(Vec::new());
        let s = stream.as_com_ref::<sb::IBStream>()?;
        ok(f(s.as_ptr())).then(|| stream.take())
    }

    /// Restores a [`EditorLink::save_state`] blob (or a bare component state): component
    /// `setState`, then the controller's `setComponentState` and `setState`. `false` when the
    /// blob is malformed or a component/controller refuses its state.
    pub fn load_state(&self, data: &[u8]) -> bool {
        let Some((comp, ctrl)) = split_state(data) else { return false };
        let stream = MemoryStream::new(comp.to_vec());
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return false };
        // SAFETY: valid component; the stream lives on this frame for the call.
        if !ok(unsafe { self.component.setState(s.as_ptr()) }) {
            return false;
        }
        if let Some(c) = &self.controller {
            stream.lock().1 = 0;
            // SAFETY: valid controller; the same stream, rewound.
            let result = unsafe { c.setComponentState(s.as_ptr()) };
            // A controller may leave component-state synchronization unimplemented.
            if !ok(result) && result != sb::kNotImplemented {
                return false;
            }
            if !ctrl.is_empty() {
                let cs = MemoryStream::new(ctrl.to_vec());
                let Some(r) = cs.as_com_ref::<sb::IBStream>() else { return false };
                // SAFETY: valid controller; the stream lives on this frame for the call.
                if !ok(unsafe { c.setState(r.as_ptr()) }) {
                    return false;
                }
            }
        }
        true
    }

    /// Copies the component's state into the controller (`getState` → `setComponentState`).
    fn sync_controller_state(&self) {
        let Some(c) = &self.controller else { return };
        let stream = ComWrapper::new(MemoryStream { inner: Mutex::new((Vec::new(), 0)) });
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return };
        // SAFETY: valid component; the stream lives on this frame for the whole call.
        if ok(unsafe { self.component.getState(s.as_ptr()) }) {
            stream.lock().1 = 0;
            // SAFETY: valid controller; same stream, rewound.
            unsafe { c.setComponentState(s.as_ptr()) };
        }
    }

    pub fn params(&self) -> Vec<RawParam> {
        let Some(c) = &self.controller else { return Vec::new() };
        // SAFETY: valid controller.
        let n = unsafe { c.getParameterCount() }.clamp(0, MAX_PARAMS);
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: `ParameterInfo` is plain data; all-zero is valid.
            let mut info: sv::ParameterInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is writable.
            if !ok(unsafe { c.getParameterInfo(i, &mut info) }) {
                continue;
            }
            let d = info.defaultNormalizedValue;
            out.push(RawParam {
                id: info.id,
                title: read_c16(&info.title),
                units: read_c16(&info.units),
                step_count: info.stepCount.max(0),
                default_normalized: if d.is_finite() { d.clamp(0.0, 1.0) } else { 0.0 },
                flags: info.flags,
            });
        }
        out
    }

    /// Normalized → plain (`None` without a controller or for a non-finite answer). UI thread
    /// only (VST3): the audio thread converts through `ParamCurves` instead.
    pub fn to_plain(&self, id: u32, normalized: f64) -> Option<f64> {
        let c = self.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.normalizedParamToPlain(id, normalized.clamp(0.0, 1.0)) };
        v.is_finite().then_some(v)
    }

    pub fn normalized_value(&self, id: u32) -> Option<f64> {
        let c = self.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.getParamNormalized(id) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// The parameter each MIDI controller drives on each of the 16 channels (`IMidiMapping`):
    /// channel-major, each channel indexed by VST3's controller numbers (CCs 0–127, then
    /// [`AFTERTOUCH`] and [`PITCH_BEND`]). Main thread only.
    pub fn midi_mapping(&self) -> Vec<Option<u32>> {
        let Some(m) = self.controller.as_ref().and_then(|c| c.cast::<sv::IMidiMapping>()) else { return Vec::new() };
        (0..MIDI_CHANNELS)
            .flat_map(|ch| (0..=PITCH_BEND).map(move |n| (ch, n)))
            .map(|(ch, n)| {
                let mut id: sv::ParamID = 0;
                let (ch, n) = (i16::try_from(ch).unwrap_or(0), i16::try_from(n).unwrap_or(0));
                // SAFETY: valid mapping interface on the main thread; `id` is a local.
                ok(unsafe { m.getMidiControllerAssignment(0, ch, n, &mut id) }).then_some(id)
            })
            .collect()
    }

    pub fn value_text(&self, id: u32, normalized: f64) -> Option<String> {
        let c = self.controller.as_ref()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller; `s` is a writable String128.
        ok(unsafe { c.getParamStringByValue(id, normalized.clamp(0.0, 1.0), &mut s) }).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    pub fn latency(&self) -> u32 {
        // SAFETY: valid processor.
        self.processor().map_or(0, |p| unsafe { p.getLatencySamples() })
    }

    pub fn tail(&self) -> u32 {
        // SAFETY: valid processor.
        self.processor().map_or(0, |p| unsafe { p.getTailSamples() })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn bus_count(&self, media: sv::MediaTypes, dir: sv::BusDirections) -> i32 {
        // SAFETY: valid component.
        unsafe { self.component.getBusCount(media as sv::MediaType, dir as sv::BusDirection) }.clamp(0, MAX_BUSES)
    }

    fn arrangement(&self, dir: sv::BusDirections, index: i32) -> Option<sv::SpeakerArrangement> {
        let p = self.processor()?;
        let mut arr: sv::SpeakerArrangement = 0;
        // SAFETY: valid processor; `arr` is writable.
        ok(unsafe { p.getBusArrangement(dir as sv::BusDirection, index, &mut arr) }).then_some(arr)
    }

    fn bus_channels(&self, dir: sv::BusDirections, index: i32) -> u32 {
        if let Some(arr) = self.arrangement(dir, index) {
            return arr.count_ones().min(MAX_BUS_CHANNELS);
        }
        // SAFETY: plain data; all-zero is valid.
        let mut info: sv::BusInfo = unsafe { std::mem::zeroed() };
        // SAFETY: valid component; index below the bus count; `info` is writable.
        if ok(unsafe { self.component.getBusInfo(sv::MediaTypes_::kAudio as sv::MediaType, dir as sv::BusDirection, index, &mut info) }) {
            u32::try_from(info.channelCount).unwrap_or(0).min(MAX_BUS_CHANNELS)
        } else {
            0
        }
    }

    /// Whether the component declares an auxiliary audio input (a sidechain) to activate.
    pub fn declares_key_input(&self) -> bool {
        self.aux_input_bus(self.bus_count(sv::MediaTypes_::kAudio, sv::BusDirections_::kInput)).is_some()
    }

    /// The first audio input bus after the main one that the plug-in declares auxiliary (`kAux`).
    fn aux_input_bus(&self, count: i32) -> Option<i32> {
        (1..count).find(|&i| {
            // SAFETY: plain data; all-zero is valid.
            let mut info: sv::BusInfo = unsafe { std::mem::zeroed() };
            // SAFETY: valid component; index below the bus count; `info` is writable.
            let got = unsafe {
                self.component.getBusInfo(sv::MediaTypes_::kAudio as sv::MediaType, sv::BusDirections_::kInput as sv::BusDirection, i, &mut info)
            };
            ok(got) && info.busType == sv::BusTypes_::kAux as sv::BusType
        })
    }

    /// Stops processing, deactivates, negotiates the bus layout (main buses mono or stereo as
    /// asked, falling back to stereo and then to the plugin's own choice), activates the main
    /// buses, sets up 32-bit processing and activates the component.
    pub fn configure(&mut self, sample_rate: f64, max_frames: usize, mono: bool) -> Result<Layout, Vst3Error> {
        self.shutdown();
        let audio = sv::MediaTypes_::kAudio;
        let event = sv::MediaTypes_::kEvent;
        let (din, dout) = (sv::BusDirections_::kInput, sv::BusDirections_::kOutput);
        let nin = self.bus_count(audio, din);
        let nout = self.bus_count(audio, dout);
        let current = |s: &Self, dir: sv::BusDirections, n: i32| -> Vec<sv::SpeakerArrangement> {
            (0..n).map(|i| s.arrangement(dir, i).unwrap_or(sv::SpeakerArr::kStereo)).collect()
        };
        let want = if mono { sv::SpeakerArr::kMono } else { sv::SpeakerArr::kStereo };
        let p = self.processor().ok_or_else(|| Vst3Error::Process("no audio processor".into()))?.clone();
        let aux = self.aux_input_bus(nin);
        let mut tried_ok = false;
        'arrangements: for main in [want, sv::SpeakerArr::kStereo] {
            // The sidechain bus follows the main bus; failing that, it keeps its own arrangement.
            for follow in [true, false] {
                let mut ins = current(self, din, nin);
                let mut outs = current(self, dout, nout);
                if let Some(a) = ins.first_mut() {
                    *a = main;
                }
                if let Some(a) = outs.first_mut() {
                    *a = main;
                }
                if follow {
                    if let Some(a) = aux.and_then(|i| ins.get_mut(usize::try_from(i).unwrap_or(usize::MAX))) {
                        *a = main;
                    }
                } else if aux.is_none() {
                    break;
                }
                // SAFETY: valid processor; the arrays hold exactly `nin`/`nout` arrangements and
                // outlive the call.
                if ok(unsafe { p.setBusArrangements(ins.as_mut_ptr(), nin, outs.as_mut_ptr(), nout) }) {
                    tried_ok = true;
                    break 'arrangements;
                }
            }
            if main == sv::SpeakerArr::kStereo {
                break;
            }
        }
        if !tried_ok {
            log::debug!("vst3: plugin refused mono/stereo bus arrangements; using its own");
            if aux.is_some() {
                log::warn!("vst3: plugin refused an arrangement for its sidechain input; the key is not delivered");
            }
        }
        let inputs: Vec<u32> = (0..nin).map(|i| self.bus_channels(din, i)).collect();
        let key_bus = aux.filter(|_| tried_ok).and_then(|i| usize::try_from(i).ok()).filter(|&i| inputs.get(i).is_some_and(|&c| c > 0));
        let layout =
            Layout { inputs, outputs: (0..nout).map(|i| self.bus_channels(dout, i)).collect(), event_input: self.bus_count(event, din) > 0, key_bus };
        // Main buses on, plus the sidechain bus when it has an arrangement; other aux buses off
        // (VST3 still gets buffers for every bus).
        for (dir, n) in [(din, nin), (dout, nout)] {
            for i in 0..n {
                let on = i == 0 || (dir == din && layout.key_bus == usize::try_from(i).ok());
                // SAFETY: valid component; index below the bus count; the component is inactive here
                // (`shutdown` above, `setActive(1)` below), as the SDK requires for bus activation.
                unsafe { self.component.activateBus(audio as sv::MediaType, dir as sv::BusDirection, i, u8::from(on)) };
            }
        }
        if layout.event_input {
            // SAFETY: valid component; event input bus 0 exists.
            unsafe { self.component.activateBus(event as sv::MediaType, din as sv::BusDirection, 0, 1) };
        }
        self.event_in = layout.event_input;
        // SDK enum constants are signed on Windows and unsigned on Unix.
        #[allow(clippy::unnecessary_cast)]
        let sample32 = sv::SymbolicSampleSizes_::kSample32 as i32;
        // SAFETY: valid processor.
        if !ok(unsafe { p.canProcessSampleSize(sample32) }) {
            return Err(Vst3Error::Process("plugin cannot process 32-bit float".into()));
        }
        let max = i32::try_from(max_frames).unwrap_or(i32::MAX);
        let mut setup = sv::ProcessSetup {
            #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
            processMode: sv::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: sample32,
            maxSamplesPerBlock: max,
            sampleRate: sample_rate,
        };
        // SAFETY: valid processor, inactive; `setup` is a valid struct for the call.
        if !ok(unsafe { p.setupProcessing(&mut setup) }) {
            return Err(Vst3Error::Process("setupProcessing failed".into()));
        }
        // SAFETY: valid component, configured above.
        if !ok(unsafe { self.component.setActive(1) }) {
            return Err(Vst3Error::Process("setActive failed".into()));
        }
        self.active = true;
        self.max_frames = max_frames;
        self.context.sampleRate = sample_rate;
        self.context.tempo = 120.0;
        self.context.timeSigNumerator = 4;
        self.context.timeSigDenominator = 4;
        // The flag constants are the C default enum type: i32 on Windows, u32 elsewhere.
        #[allow(clippy::unnecessary_cast)]
        let flags = (sv::ProcessContext_::StatesAndFlags_::kTempoValid | sv::ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;
        self.context.state = flags;
        self.host.latency_changed.store(false, Ordering::Relaxed);
        Ok(layout)
    }

    /// `setProcessing(false)` + `setActive(false)` when needed.
    pub fn shutdown(&mut self) {
        if self.processing {
            if let Some(p) = self.processor() {
                // SAFETY: valid processor in the processing state.
                unsafe { p.setProcessing(0) };
            }
            self.processing = false;
        }
        if self.active {
            // SAFETY: valid, active component.
            unsafe { self.component.setActive(0) };
            self.active = false;
        }
    }

    /// Runs one `process` call over `frames` frames starting at block time `offset`: `points`
    /// (all inside this call) become one parameter queue each, ascending (see
    /// [`ParamChanges::fill`]; `points` is sorted in place), and `notes` (sorted by time) have
    /// their times made relative to `offset`. Returns the parameter points dropped because the
    /// slab ran out.
    pub fn process(
        &mut self,
        frames: usize,
        inputs: &mut BusBuffers,
        outputs: &mut BusBuffers,
        points: &mut [ParamPoint],
        notes: &[NoteEvent],
        offset: u32,
        project_time: i64,
        transport: Option<&Transport>,
    ) -> Result<u32, sb::tresult> {
        if !self.active {
            return Err(sb::kNotInitialized);
        }
        if frames == 0 || frames > self.max_frames || frames > inputs.frames || frames > outputs.frames {
            return Err(sb::kInvalidArgument);
        }
        let p = self.processor.clone().ok_or(sb::kNoInterface)?;
        if !self.processing {
            // Many plugins return kNotImplemented here; that is not an error.
            // SAFETY: valid, active processor.
            unsafe { p.setProcessing(1) };
            self.processing = true;
        }
        let dropped = self.in_params.fill(points, offset);
        self.out_params.clear();
        let event_in = self.event_in;
        self.in_events.with(|v| {
            v.clear();
            if event_in {
                for n in notes.iter().take(EVENT_CAP) {
                    v.push(n.to_raw(offset));
                }
            }
        });
        self.out_events.with(Vec::clear);
        inputs.refresh();
        outputs.refresh();
        self.context.continousTimeSamples = project_time;
        match transport {
            Some(t) => apply_transport(&mut self.context, t),
            None => self.context.projectTimeSamples = project_time,
        }
        let mut data = sv::ProcessData {
            #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
            processMode: sv::ProcessModes_::kRealtime as i32,
            #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
            symbolicSampleSize: sv::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: i32::try_from(frames).unwrap_or(0),
            numInputs: i32::try_from(inputs.bufs.len()).unwrap_or(0),
            numOutputs: i32::try_from(outputs.bufs.len()).unwrap_or(0),
            inputs: if inputs.bufs.is_empty() { std::ptr::null_mut() } else { inputs.bufs.as_mut_ptr() },
            outputs: if outputs.bufs.is_empty() { std::ptr::null_mut() } else { outputs.bufs.as_mut_ptr() },
            inputParameterChanges: self.in_params.as_com_ref::<sv::IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputParameterChanges: self.out_params.as_com_ref::<sv::IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            inputEvents: self.in_events.as_com_ref::<sv::IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputEvents: self.out_events.as_com_ref::<sv::IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            processContext: &mut self.context,
        };
        // SAFETY: valid, active processor. Every channel pointer refers to `frames` (or more) valid
        // f32s owned by `inputs`/`outputs`, borrowed for the whole call; the bus tables, the
        // parameter/event objects and the context are owned by `self` and untouched until the
        // call returns.
        let r = unsafe { p.process(&mut data) };
        if ok(r) { Ok(dropped) } else { Err(r) }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Close any editor and cut its handles off before the controller goes away.
        if self.editor.kill() {
            self.bundle.mark_leaked();
        }
        self.shutdown();
        if let Some((a, b)) = self.connections.take() {
            // SAFETY: the pair was connected in `instantiate`; disconnect before terminating.
            unsafe {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
        }
        if let Some(c) = self.controller.take() {
            // SAFETY: valid controller; detach our handler before it goes away.
            unsafe { c.setComponentHandler(std::ptr::null_mut()) };
            if self.controller_initialized {
                // SAFETY: separately created controller, initialised once; terminated once.
                unsafe { c.terminate() };
            }
        }
        self.processor = None;
        if self.component_initialized {
            // SAFETY: valid component, inactive; terminated exactly once.
            unsafe { self.component.terminate() };
            self.component_initialized = false;
        }
    }
}

/// Splits a state blob into (component, controller) parts. A blob without SoundCraft's container
/// header is taken as a bare component state.
fn split_state(data: &[u8]) -> Option<(&[u8], &[u8])> {
    crate::preset::split_state(data)
}

// ---- editor (IPlugView) --------------------------------------------------------------------

/// The `IPlugFrame` handed to a plugin view: `resizeView` resizes our host window.
pub(crate) struct PlugFrame {
    /// The host window (an `NSWindow*` on macOS), not owned; null when there is none.
    window: std::sync::atomic::AtomicPtr<c_void>,
}

impl Class for PlugFrame {
    type Interfaces = (sb::IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut sb::IPlugView, new_size: *mut sb::ViewRect) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(r) = (unsafe { new_size.as_mut() }) else { return sb::kInvalidArgument };
        let (w, h) = rect_size(r);
        window::resize(self.window.load(Ordering::Relaxed), w, h);
        // SAFETY: the plugin passes its own live view (or null, handled).
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            // SAFETY: valid view; `r` is the plugin's own writable rect.
            unsafe { v.onSize(r) };
        }
        sb::kResultOk
    }
}

/// Width and height of a view rect, clamped to a sane window size.
fn rect_size(r: &sb::ViewRect) -> (f64, f64) {
    let w = r.right.saturating_sub(r.left).clamp(1, 16_384);
    let h = r.bottom.saturating_sub(r.top).clamp(1, 16_384);
    (f64::from(w), f64::from(h))
}

#[derive(Default)]
struct EditorInner {
    /// `None` once the instance is gone.
    component: Option<ComPtr<sv::IComponent>>,
    /// `None` once the instance is gone, or without an edit controller.
    controller: Option<ComPtr<sv::IEditController>>,
    view: Option<ComPtr<sb::IPlugView>>,
    frame: Option<ComWrapper<PlugFrame>>,
    window: Option<window::HostWindow>,
}

/// The link between an instance and its editor and state handles. The instance may process on
/// the audio thread while the editor (the edit controller's `IPlugView`) is driven and the state
/// read from the main thread, which is VST3's own threading model (controller, views and
/// `getState` on the UI thread). All such calls happen under this lock, and the instance closes
/// the view and drops the component and controller references under the same lock before it
/// terminates, so a handle never calls into a dead plugin.
pub(crate) struct EditorLink {
    /// GUI/controller calls belong to the instance's creating thread, regardless of its name.
    main: std::thread::ThreadId,
    inner: Mutex<EditorInner>,
    /// The editor window's header strip (issue #68): kept across opens, read by the window.
    header: Arc<transit_rt::editor_window::HeaderCell>,
}

impl Default for EditorLink {
    fn default() -> Self {
        Self { main: std::thread::current().id(), inner: Mutex::new(EditorInner::default()), header: transit_rt::editor_window::HeaderCell::new() }
    }
}

impl EditorLink {
    fn on_main_thread(&self) -> bool {
        self.main == std::thread::current().id() && window::on_main_thread()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EditorInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_controller(&self) -> bool {
        self.lock().controller.is_some()
    }

    /// The header strip's model (any thread).
    pub fn header(&self) -> &transit_rt::editor_window::HeaderCell {
        &self.header
    }

    fn create_view(g: &EditorInner) -> Result<ComPtr<sb::IPlugView>, String> {
        let c = g.controller.as_ref().ok_or_else(|| "the plugin is gone".to_string())?;
        // SAFETY: valid controller; the name is a static NUL-terminated string.
        let raw = unsafe { c.createView(c"editor".as_ptr()) };
        // SAFETY: a non-null view comes with one reference owned by the caller.
        unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| "this plugin has no editor".to_string())
    }

    /// Creates the view, checks it supports this platform's window type and reads its size,
    /// without attaching it anywhere. For diagnostics and tests.
    pub fn probe(&self) -> Result<(u32, u32), String> {
        if !self.on_main_thread() {
            return Err("plugin editor belongs to another thread".into());
        }
        let g = self.lock();
        let view = EditorLink::create_view(&g)?;
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "plugin editors are not supported on this platform".to_string())?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let mut r = sb::ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: valid view; `r` is writable.
        if !ok(unsafe { view.getSize(&mut r) }) {
            return Err("the editor reports no size".into());
        }
        let (w, h) = rect_size(&r);
        Ok((w as u32, h as u32))
    }

    /// Opens (or raises) the editor in a host window. Main thread only.
    pub fn open(&self, title: &str) -> Result<(), String> {
        if !self.on_main_thread() {
            return Err("plugin editors must be opened on the main thread".into());
        }
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "VST3 editors are not hosted on this platform yet".to_string())?;
        let mut g = self.lock();
        if let Some(w) = &g.window {
            if w.is_visible() {
                w.raise();
                return Ok(());
            }
            // The user closed the window: start over.
            EditorLink::teardown(&mut g);
        }
        let view = EditorLink::create_view(&g)?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let mut r = sb::ViewRect { left: 0, top: 0, right: 400, bottom: 300 };
        // SAFETY: valid view; `r` is writable. On failure the default size stays.
        unsafe { view.getSize(&mut r) };
        let (w, h) = rect_size(&r);
        let shown = self.header.view().title;
        let title = if shown.is_empty() { title } else { shown.as_str() };
        let win = window::HostWindow::new(title, w, h, Some(Arc::clone(&self.header)))?;
        let frame = ComWrapper::new(PlugFrame { window: std::sync::atomic::AtomicPtr::new(win.raw()) });
        if let Some(f) = frame.as_com_ref::<sb::IPlugFrame>() {
            // SAFETY: valid view; the frame outlives the view's attachment (kept in `g.frame`
            // and detached in `teardown`).
            unsafe { view.setFrame(f.as_ptr()) };
        }
        // SAFETY: valid view; the parent is the live content view of `win`, of the type named.
        if !ok(unsafe { view.attached(win.parent(), platform) }) {
            // SAFETY: valid view; detach our frame again.
            unsafe { view.setFrame(std::ptr::null_mut()) };
            win.close();
            return Err("the editor could not attach to its window".into());
        }
        win.raise();
        g.view = Some(view);
        g.frame = Some(frame);
        g.window = Some(win);
        Ok(())
    }

    fn teardown(g: &mut EditorInner) {
        if let Some(view) = g.view.take() {
            // SAFETY: valid, attached view; main thread (callers check). `removed` before the
            // window goes, and the frame is detached before it is released.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
        }
        g.frame = None;
        if let Some(w) = g.window.take() {
            w.close();
        }
    }

    /// Hands a host-set value to the controller before a state save (`setParamNormalized`,
    /// under the lock like [`EditorLink::save_state`]). No-op once the instance is gone.
    pub fn sync_param(&self, id: u32, n: f64) {
        if !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller (the instance clears it under this lock before it terminates).
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }

    /// The component state plus the controller state, in SoundCraft's container (see
    /// `STATE_MAGIC`). `None` when the component refuses `getState` or the instance is gone.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        let component = g.component.as_ref()?;
        // SAFETY: valid component (the instance clears it under this lock before it terminates);
        // the stream lives in `read_state` for the whole call.
        let comp = Instance::read_state(|s| unsafe { component.getState(s) })?;
        let ctrl = match &g.controller {
            // SAFETY: valid controller; as above.
            Some(c) => Instance::read_state(|s| unsafe { c.getState(s) }).unwrap_or_default(),
            None => Vec::new(),
        };
        crate::preset::join_state(&comp, &ctrl)
    }

    /// Closes the editor. Main thread only (no-op elsewhere).
    pub fn close(&self) {
        if self.on_main_thread() {
            EditorLink::teardown(&mut self.lock());
        }
    }

    pub fn is_open(&self) -> bool {
        let g = self.lock();
        match &g.window {
            Some(w) if self.on_main_thread() => w.is_visible(),
            Some(_) => true,
            None => false,
        }
    }

    /// Main-thread housekeeping: tears the view down when the user closed its window.
    pub fn idle(&self) {
        if !self.on_main_thread() {
            return;
        }
        let mut g = self.lock();
        match &g.window {
            Some(w) if !w.is_visible() => EditorLink::teardown(&mut g),
            // The strip's changes and the size follow (issue #68).
            Some(w) => w.sync(),
            None => {}
        }
    }

    /// Normalized → plain through the controller (main thread).
    pub fn to_plain(&self, id: u32, n: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.normalizedParamToPlain(id, n.clamp(0.0, 1.0)) };
        v.is_finite().then_some(v)
    }

    /// Plain → normalized through the controller (main thread).
    pub fn to_normalized(&self, id: u32, plain: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        if !plain.is_finite() {
            return None;
        }
        // SAFETY: valid controller.
        let v = unsafe { c.plainParamToNormalized(id, plain) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// Shows a value change in the editor (`setParamNormalized`, main thread).
    pub fn set_normalized(&self, id: u32, n: f64) {
        if !self.on_main_thread() || !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller, main thread.
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }

    /// Called by the instance before it terminates: closes the editor and cuts the handles off.
    /// Returns whether plugin objects had to be leaked.
    fn kill(&self) -> bool {
        let mut g = self.lock();
        let mut leaked = false;
        if g.view.is_some() || g.window.is_some() {
            if self.on_main_thread() {
                EditorLink::teardown(&mut g);
            } else {
                // Window calls are main-thread only; leak the view and window rather than crash.
                log::warn!("vst3: editor still open while its plugin is destroyed off the main thread");
                if let Some(v) = g.view.take() {
                    std::mem::forget(v);
                    leaked = true;
                }
                // `HostWindow` has no destructor: dropping it leaks its retain on purpose.
                g.window = None;
                if let Some(f) = g.frame.take() {
                    std::mem::forget(f);
                }
            }
        }
        g.component = None;
        g.controller = None;
        leaked
    }
}

/// The platform host window for plugin views.
#[cfg(target_os = "macos")]
mod window {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSFloatingWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use std::ffi::{c_char, c_void};

    pub const PLATFORM_TYPE: Option<*const c_char> = Some(vst3::Steinberg::kPlatformTypeNSView);

    pub fn on_main_thread() -> bool {
        MainThreadMarker::new().is_some()
    }

    /// An `NSWindow` (titled, closable, floating above the app's windows) whose content view is
    /// the plugin view's parent. Holds one retain of the window; main thread only.
    pub struct HostWindow {
        window: *mut NSWindow,
    }

    // SAFETY: the pointer is only dereferenced on the main thread (every method checks
    // `MainThreadMarker`); the struct merely carries the retain between threads' locks.
    unsafe impl Send for HostWindow {}

    impl HostWindow {
        fn get(&self) -> Option<&NSWindow> {
            MainThreadMarker::new()?;
            // SAFETY: we hold a retain of this window (from `new`) until `close`.
            unsafe { self.window.as_ref() }
        }

        /// _header: no strip on this platform (the plug-in's view fills the window).
        pub fn new(
            title: &str,
            w: f64,
            h: f64,
            _header: Option<std::sync::Arc<transit_rt::editor_window::HeaderCell>>,
        ) -> Result<HostWindow, String> {
            let mtm = MainThreadMarker::new().ok_or_else(|| "not on the main thread".to_string())?;
            let _app = NSApplication::sharedApplication(mtm);
            let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(w, h));
            // Plugin editors are transient windows. In particular they must not be
            // minimizable: minimized NSWindows become persistent thumbnails in the macOS Dock,
            // which made every editor visited in a session look like another running app.
            let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            // SAFETY: a freshly allocated window on the main thread; valid rect and style.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false)
            };
            // SAFETY: we keep our own retain and release it in `close`, so AppKit must not
            // release the window when the user closes it.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.setExcludedFromWindowsMenu(true);
            window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
            window.center();
            Ok(HostWindow { window: Retained::into_raw(window) })
        }

        /// The raw window pointer (for the plug frame's resizes).
        pub fn raw(&self) -> *mut c_void {
            self.window.cast()
        }

        /// The content view (`NSView*`) the plugin view attaches to; null when unavailable.
        pub fn parent(&self) -> *mut c_void {
            self.get().and_then(|w| w.contentView()).map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v).cast_mut().cast())
        }

        pub fn raise(&self) {
            if let Some(w) = self.get() {
                w.makeKeyAndOrderFront(None);
            }
        }

        pub fn is_visible(&self) -> bool {
            self.get().is_some_and(|w| w.isVisible())
        }

        /// Nothing to keep in step: no strip here.
        pub fn sync(&self) {}

        pub fn close(self) {
            if let Some(w) = self.get() {
                w.orderOut(None);
                w.close();
            }
            if MainThreadMarker::new().is_some() {
                // SAFETY: balances the retain taken in `new`; nothing uses the pointer after this.
                drop(unsafe { Retained::from_raw(self.window) });
            }
        }
    }

    /// Resizes a host window's content (`window` from [`HostWindow::raw`]); main thread only.
    pub fn resize(window: *mut c_void, w: f64, h: f64) {
        if MainThreadMarker::new().is_none() {
            return;
        }
        // SAFETY: null or a window the editor link keeps retained while the frame is attached.
        if let Some(win) = unsafe { window.cast::<NSWindow>().as_ref() } {
            win.setContentSize(NSSize::new(w, h));
        }
    }
}

/// The Windows host window: `transit_rt`'s editor window (issue #68), a top-level window on the
/// plug-in host thread whose client area is a header strip above the plugin view's `HWND` parent
/// and nothing else. Closing it only hides it; the editor link notices in `idle`, detaches the
/// view, then destroys the window.
#[cfg(windows)]
mod window {
    use std::ffi::c_char;

    pub use transit_rt::editor_window::{EditorWindow as HostWindow, resize_content as resize};

    pub const PLATFORM_TYPE: Option<*const c_char> = Some(vst3::Steinberg::kPlatformTypeHWND);

    /// Windows permits GUI creation on any thread; the editor link checks its creating thread.
    pub fn on_main_thread() -> bool {
        true
    }
}
/// Other platforms: no editor windows yet (opening reports "unsupported").
#[cfg(not(any(target_os = "macos", windows)))]
mod window {
    use std::ffi::{c_char, c_void};

    pub const PLATFORM_TYPE: Option<*const c_char> = None;

    pub fn on_main_thread() -> bool {
        true
    }

    pub struct HostWindow;

    impl HostWindow {
        pub fn new(
            _title: &str,
            _w: f64,
            _h: f64,
            _header: Option<std::sync::Arc<transit_rt::editor_window::HeaderCell>>,
        ) -> Result<HostWindow, String> {
            Err("VST3 editors are not hosted on this platform yet".into())
        }
        pub fn raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn parent(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn raise(&self) {}
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn sync(&self) {}
        pub fn close(self) {}
    }

    pub fn resize(_window: *mut c_void, _w: f64, _h: f64) {}
}

#[cfg(test)]
#[path = "host_object_tests.rs"]
mod host_object_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn editor_thread_is_its_creator_not_a_name() {
        let link = Arc::new(EditorLink::default());
        assert!(link.on_main_thread(), "a host thread need not be named main");
        assert!(!std::thread::spawn(move || link.on_main_thread()).join().unwrap(), "another thread cannot drive this editor");
    }

    fn gain_instance() -> Instance {
        crate::set_scan_extra_dirs_only(true);
        let descriptors = crate::scan_paths(&[fixture::unregistered_fixture_dir().to_path_buf()]);
        let d = descriptors.iter().find(|d| d.name == "Fixture Gain").unwrap();
        Bundle::load(&d.path).unwrap().instantiate(&crate::parse_id(&d.id).unwrap()).unwrap()
    }

    #[test]
    fn processing_failures_do_not_allocate() {
        let mut inst = gain_instance();
        let mut inputs = BusBuffers::new(&[2], 64);
        let mut outputs = BusBuffers::new(&[2], 64);
        for case in 0..4 {
            if case == 1 {
                inst.configure(48_000.0, 64, false).unwrap();
            }
            let processor = if case == 2 { inst.processor.take() } else { None };
            if case == 3 {
                assert!(inst.load_state(&(-3.0f64).to_le_bytes()));
            }
            let frames = if case == 1 { 65 } else { 64 };
            let (failed, stats) = allocation_probe::measure(|| {
                let result = inst.process(frames, &mut inputs, &mut outputs, &mut [], &[], 0, 0, None);
                result.is_err()
            });
            if case == 2 {
                inst.processor = processor;
            }
            assert!(failed, "case {case}");
            assert!(stats.is_zero(), "case {case}: {stats:?}");
        }
    }

    #[test]
    fn controller_state_rejection_is_reported() {
        let inst = gain_instance();
        assert!(!inst.load_state(&(-1.0f64).to_le_bytes()), "setComponentState refused");
        let mut blob = STATE_MAGIC.to_vec();
        blob.extend_from_slice(&8u32.to_le_bytes());
        blob.extend_from_slice(&0.5f64.to_le_bytes());
        blob.extend_from_slice(&8u32.to_le_bytes());
        blob.extend_from_slice(&(-2.0f64).to_le_bytes());
        assert!(!inst.load_state(&blob), "setState refused");
    }

    /// `IHostApplication::createInstance` as a plug-in calls it (class id = interface id).
    fn create_host_object(host: &ComWrapper<HostContext>, cid: [u8; 16], iid: [u8; 16]) -> (sb::tresult, *mut c_void) {
        let (mut cid, mut iid) = (cid.map(|b| b as c_char), iid.map(|b| b as c_char));
        let mut obj = std::ptr::dangling_mut::<c_void>();
        // SAFETY: valid TUIDs and out-pointer.
        let r = unsafe { host.createInstance(&mut cid, &mut iid, &mut obj) };
        (r, obj)
    }

    #[test]
    fn host_creates_messages_and_attribute_lists() {
        let host = ComWrapper::new(HostContext::default());
        let (r, obj) = create_host_object(&host, sv::IMessage::IID, sv::IMessage::IID);
        assert_eq!(r, sb::kResultOk);
        // SAFETY: an IMessage with one reference owned here.
        let msg = unsafe { ComPtr::from_raw(obj.cast::<sv::IMessage>()) }.expect("message");
        // SAFETY: the owned message keeps its attribute list alive; IDs/text are terminated or
        // null (rejected), and buffers have the reported sizes. Outputs are distinct and writable;
        // successful binary output is read before any replacement or release.
        unsafe {
            assert!(msg.getMessageID().is_null());
            msg.setMessageID(c"Hello".as_ptr());
            assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()), c"Hello");
            let attrs = vst3::ComRef::from_raw(msg.getAttributes()).expect("attributes");
            let text: Vec<u16> = "abcdef".encode_utf16().chain(Some(0)).collect();
            assert_eq!(attrs.setString(c"s".as_ptr(), text.as_ptr()), sb::kResultOk);
            let mut out = [0xffffu16; 4];
            assert_eq!(attrs.getString(c"s".as_ptr(), out.as_mut_ptr(), 8), sb::kResultOk);
            assert_eq!(out, [u16::from(b'a'), u16::from(b'b'), u16::from(b'c'), 0], "truncated and terminated");
            assert_eq!(attrs.setBinary(c"b".as_ptr(), [1u8, 2, 3].as_ptr().cast(), 3), sb::kResultOk);
            let (mut p, mut n) = (std::ptr::null(), 0u32);
            assert_eq!(attrs.getBinary(c"b".as_ptr(), &mut p, &mut n), sb::kResultOk);
            assert_eq!(std::slice::from_raw_parts(p.cast::<u8>(), n as usize), [1, 2, 3]);
            let mut i = 0i64;
            assert_eq!(attrs.getInt(c"b".as_ptr(), &mut i), sb::kResultFalse, "wrong type");
            assert_eq!(attrs.getInt(std::ptr::null(), &mut i), sb::kInvalidArgument, "null id is invalid");
        }
        let (r, obj) = create_host_object(&host, sv::IAttributeList::IID, FUnknown::IID);
        assert_eq!(r, sb::kResultOk);
        // SAFETY: an FUnknown with one reference owned here.
        let list = unsafe { ComPtr::from_raw(obj.cast::<FUnknown>()) }.and_then(|u| u.cast::<sv::IAttributeList>()).expect("list");
        // SAFETY: valid list.
        unsafe {
            assert_eq!(list.setFloat(c"f".as_ptr(), 0.5), sb::kResultOk);
            let mut f = 0.0;
            assert_eq!(list.getFloat(c"f".as_ptr(), &mut f), sb::kResultOk);
            assert_eq!(f, 0.5);
        }
        // Unknown classes and interfaces the class lacks: refused, with a null object.
        for (cid, iid) in [(sv::IComponentHandler::IID, sv::IComponentHandler::IID), (sv::IMessage::IID, sv::IAttributeList::IID)] {
            assert_eq!(create_host_object(&host, cid, iid), (sb::kResultFalse, std::ptr::null_mut()));
        }
    }

    #[test]
    fn silence_flags_set_one_bit_per_channel() {
        assert_eq!(silence_flags(0), 0);
        assert_eq!(silence_flags(1), 0b1);
        assert_eq!(silence_flags(2), 0b11);
        assert_eq!(silence_flags(6), 0b11_1111);
        assert_eq!(silence_flags(64), u64::MAX);
        let mut b = BusBuffers::new(&[2, 6], 4);
        b.set_silent(true);
        b.refresh();
        assert_eq!(b.bufs.iter().map(|x| x.silenceFlags).collect::<Vec<_>>(), vec![0b11, 0b11_1111]);
        b.set_silent(false);
        b.refresh();
        assert!(b.bufs.iter().all(|x| x.silenceFlags == 0));
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
    fn the_transport_fills_the_process_context() {
        use sv::ProcessContext_::StatesAndFlags_::*;
        // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
        let mut c: sv::ProcessContext = unsafe { std::mem::zeroed() };
        let t = Transport {
            playing: true,
            tempo: 90.0,
            numerator: 3,
            denominator: 4,
            samples: 96_000,
            quarters: 3.5,
            bar_start: 3.0,
            ..Transport::default()
        };
        apply_transport(&mut c, &t);
        let playing = kPlaying as u32;
        assert_eq!(c.state & playing, playing);
        assert_eq!(c.state & kRecording as u32, 0);
        assert_eq!((c.tempo, c.timeSigNumerator, c.timeSigDenominator), (90.0, 3, 4));
        assert_eq!((c.projectTimeSamples, c.projectTimeMusic, c.barPositionMusic), (96_000, 3.5, 3.0));
        apply_transport(&mut c, &Transport { playing: false, ..t });
        assert_eq!(c.state & playing, 0);
    }

    #[test]
    fn state_container_parses_strictly() {
        let mut blob = b"SCV3".to_vec();
        blob.extend_from_slice(&3u32.to_le_bytes());
        blob.extend_from_slice(b"abc");
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(b"xy");
        assert_eq!(split_state(&blob), Some((&b"abc"[..], &b"xy"[..])));
        assert_eq!(split_state(b"raw component"), Some((&b"raw component"[..], &[][..])), "bare component state");
        assert_eq!(split_state(b""), None);
        let mut trailing = blob.clone();
        trailing.push(0);
        assert_eq!(split_state(&trailing), None);
        assert_eq!(split_state(&blob[..blob.len() - 1]), None);
        let mut huge = b"SCV3".to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(split_state(&huge), None);
        assert_eq!(split_state(b"SCV3"), None);
    }

    #[test]
    fn strings_are_bounded() {
        let mut b = [0u16; 4];
        write_c16("SoundCraft", &mut b);
        assert_eq!(read_c16(&b), "Sou");
        let full: Vec<u16> = "abcd".encode_utf16().collect();
        assert_eq!(read_c16(&full), "abcd");
        let c: Vec<c_char> = b"Fx|EQ\0junk".iter().map(|&x| x as c_char).collect();
        assert_eq!(read_c8(&c), "Fx|EQ");
        write_c16("x", &mut []);
    }

    #[test]
    fn parameter_points_keep_every_offset_in_order() {
        let pc = ParamChanges::new(8, 6);
        let pt = |time, id, class, seq, value| ParamPoint { time, id, class, seq, value, release: false };
        // Two parameters, several offsets; at offset 32 of parameter 1 a controller follows automation.
        let mut points =
            [pt(40, 1, 0, 4, 0.8), pt(0, 1, 1, 0, 1.0), pt(32, 1, 1, 3, 0.0), pt(5, 1, 0, 1, 0.2), pt(32, 1, 0, 2, 0.6), pt(9, 2, 0, 5, 0.3)];
        assert_eq!(pc.fill(&mut points, 0), 0);
        let read = |i: usize| {
            let q = &pc.queues[i];
            (q.id.load(Ordering::Relaxed), (0..q.points.load(Ordering::Relaxed)).map(|k| q.point(k).unwrap()).collect::<Vec<_>>())
        };
        assert_eq!(read(0), (1, vec![(0, 1.0), (5, 0.2), (32, 0.0), (40, 0.8)]));
        assert_eq!(read(1), (2, vec![(9, 0.3)]));
        // Exhaustion: four points for parameter 1 and one for 2 in a slab of 3 keep the last of 1.
        let small = ParamChanges::new(8, 3);
        let mut points = [pt(0, 1, 0, 0, 0.1), pt(1, 1, 0, 1, 0.2), pt(2, 1, 0, 2, 0.3), pt(3, 1, 0, 3, 0.4), pt(0, 2, 0, 4, 0.5)];
        assert_eq!(small.fill(&mut points, 0), 3);
        let q = &small.queues[0];
        assert_eq!((q.points.load(Ordering::Relaxed), q.point(0)), (1, Some((3, 0.4))));
        assert_eq!(small.queues[1].point(0), Some((0, 0.5)), "every parameter keeps a point");
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // SDK enum signedness differs by target.
    fn host_objects_behave() {
        let pc = ParamChanges::new(2, 16);
        let pt = |time, id, seq, value| ParamPoint { time, id, class: 0, seq, value, release: false };
        let mut points = [pt(2, 5, 0, 0.25), pt(2, 5, 1, 0.5), pt(2, 6, 2, 1.0), pt(2, 7, 3, 1.0)];
        assert_eq!(pc.fill(&mut points, 2), 1, "the third parameter finds no queue");
        // SAFETY: test calls through our own implementation.
        unsafe {
            assert_eq!(pc.getParameterCount(), 2, "full pool drops extra params");
            assert!(pc.getParameterData(2).is_null());
            assert!(pc.getParameterData(-1).is_null());
            let q = pc.queues[0].as_com_ref::<sv::IParamValueQueue>().unwrap();
            let (mut o, mut v) = (9, 0.0);
            assert_eq!(q.getPointCount(), 1, "one offset, the last value");
            assert_eq!(q.getPoint(0, &mut o, &mut v), sb::kResultOk);
            assert_eq!((o, v), (0, 0.5));
            assert_ne!(q.getPoint(1, &mut o, &mut v), sb::kResultOk);
            assert_ne!(q.getPoint(-1, &mut o, &mut v), sb::kResultOk);
            assert_ne!(q.getPoint(0, std::ptr::null_mut(), &mut v), sb::kResultOk);
        }
        let el = EventList::new(1);
        let mut e = NoteEvent::On { time: 5, key: 60, velocity: 1.0, channel: 3, id: 9 }.to_raw(2);
        // SAFETY: as above.
        unsafe {
            assert_eq!(el.addEvent(&mut e), sb::kResultOk);
            assert_ne!(el.addEvent(&mut e), sb::kResultOk, "bounded");
            assert_eq!(el.getEventCount(), 1);
            let mut out: sv::Event = std::mem::zeroed();
            assert_eq!(el.getEvent(0, &mut out), sb::kResultOk);
            assert_eq!(out.sampleOffset, 3);
            assert_eq!(out.__field0.noteOn.pitch, 60);
            assert_eq!((out.__field0.noteOn.channel, out.__field0.noteOn.noteId), (3, 9));
            assert_ne!(el.getEvent(1, &mut out), sb::kResultOk);
        }
        let s = MemoryStream { inner: Mutex::new((Vec::new(), 0)) };
        let mut buf = *b"hello";
        // SAFETY: as above; buffers are valid for the given sizes.
        unsafe {
            let mut n = 0;
            assert_eq!(s.write(buf.as_mut_ptr().cast(), 5, &mut n), sb::kResultOk);
            assert_eq!(n, 5);
            let mut pos = 0i64;
            assert_eq!(s.seek(1, sb::IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos), sb::kResultOk);
            let mut out = [0u8; 8];
            assert_eq!(s.read(out.as_mut_ptr().cast(), 8, &mut n), sb::kResultOk);
            assert_eq!((n, &out[..4]), (4, &b"ello"[..]));
            assert_eq!(s.seek(-100, sb::IBStream_::IStreamSeekMode_::kIBSeekEnd as i32, &mut pos), sb::kResultOk);
            assert_eq!(pos, 0);
            assert_eq!(s.write(std::ptr::null_mut(), 5, &mut n), sb::kResultFalse);
        }
        let h = HostContext::default();
        let mut name: sv::String128 = [0; 128];
        // SAFETY: as above.
        unsafe {
            assert_eq!(h.getName(&mut name), sb::kResultOk);
            assert_eq!(h.restartComponent(sv::RestartFlags_::kLatencyChanged as i32), sb::kResultOk);
        }
        assert_eq!(read_c16(&name), "SoundCraft");
        assert!(h.latency_changed.load(Ordering::Relaxed));
        assert!(!h.titles_changed.load(Ordering::Relaxed) && !h.values_changed.load(Ordering::Relaxed), "latency alone leaves both");
        // SAFETY: as above.
        assert_eq!(unsafe { h.restartComponent(sv::RestartFlags_::kParamValuesChanged as i32) }, sb::kResultOk);
        assert!(h.values_changed.load(Ordering::Relaxed), "values flag recorded");
        assert!(!h.titles_changed.load(Ordering::Relaxed), "values alone leave the curves");
        // SAFETY: as above.
        assert_eq!(unsafe { h.restartComponent(sv::RestartFlags_::kParamTitlesChanged as i32) }, sb::kResultOk);
        assert!(h.titles_changed.load(Ordering::Relaxed), "titles re-sample the curves");
        assert_eq!(hex(&[0xAB; 16]), "AB".repeat(16));
    }
}

#[cfg(test)]
#[path = "../../rt/src/testing.rs"]
#[allow(dead_code)]
mod allocation_probe;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: allocation_probe::CountingAlloc = allocation_probe::CountingAlloc;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod fixture;
