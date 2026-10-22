//! The COM, registry and Win32 calls of the backend and the calls through the `IASIO` vtable: the
//! only `unsafe` in the crate (with its test double, [`fake`], in the same file).
//!
//! Threading rule: a [`Driver`] is created, initialized, called and released on one thread, the
//! ASIO host thread of [`crate::host`], which initialized COM as a single-threaded apartment. The
//! drivers register `ThreadingModel=Apartment`, so their objects belong to the creating STA, and
//! the ASIO SDK requires every `IASIO` call on the thread that called `init`. `Driver` holds a raw
//! pointer and is therefore neither `Send` nor `Sync`: the compiler keeps it on that thread.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use crate::abi::{self, AsioChannelInfo, IAsio, IAsioVtbl};
use crate::driver::{BufferSize, DriverEntry, parse_guid};
use crate::host::Loader;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use transit_audio_backend as ab;
use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstanceEx, CoInitializeEx, CoUninitialize, MULTI_QI,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    TranslateMessage, WINDOW_EX_STYLE, WS_OVERLAPPED,
};
use windows_core::{HRESULT, Interface, PCWSTR, w};

/// The registry key that lists the installed drivers (64-bit view: the process is 64-bit).
const ASIO_KEY: PCWSTR = w!("SOFTWARE\\ASIO");

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// A loaded `IASIO` object holding one reference. Not `Send`: see the module documentation.
pub(crate) struct Driver {
    obj: NonNull<IAsio>,
}

impl Driver {
    /// Takes over a driver object.
    ///
    /// # Safety
    ///
    /// `obj` is null or points to a live object whose first field is a pointer to a valid
    /// [`IAsioVtbl`] (an `IASIO` interface pointer), carrying one reference that the returned
    /// `Driver` owns and releases in `Drop`. The caller is on the thread that created the object
    /// and the object is used only on that thread from now on.
    pub(crate) unsafe fn from_raw(obj: *mut c_void) -> Option<Self> {
        NonNull::new(obj.cast::<IAsio>()).map(|obj| Self { obj })
    }

    fn this(&self) -> *mut IAsio {
        self.obj.as_ptr()
    }

    fn vtbl(&self) -> &IAsioVtbl {
        // SAFETY: by `from_raw`'s contract the object is live until `Drop` releases our reference,
        // and a COM object's vtable pointer is valid and unchanged for the object's lifetime.
        unsafe { &*(*self.this()).vtbl }
    }

    /// `IASIO::init(sysHandle)`: true on `ASIOTrue`.
    pub(crate) fn init(&mut self, sys_ref: *mut c_void) -> bool {
        let init = self.vtbl().init;
        // SAFETY: `init(void *sysHandle)` on the thread that created the object (the STA host
        // thread). `sys_ref` is the host's message-only window, which outlives every driver the
        // host loads (the window is destroyed after the last driver is released).
        unsafe { init(self.this(), sys_ref) != abi::ASIO_FALSE }
    }

    /// `getDriverName`, through a buffer larger than the SDK's 32 bytes.
    pub(crate) fn driver_name(&mut self) -> String {
        let mut name = [0u8; abi::DRIVER_NAME_BUFFER];
        let get = self.vtbl().getDriverName;
        // SAFETY: `getDriverName(char *name)` writes a NUL-terminated string of at most 32 bytes
        // (`iasiodrv.h`); the buffer is 128 bytes and lives across the call. Host thread.
        unsafe { get(self.this(), name.as_mut_ptr()) };
        abi::c_string(&name)
    }

    pub(crate) fn driver_version(&mut self) -> i32 {
        let get = self.vtbl().getDriverVersion;
        // SAFETY: `long getDriverVersion()` takes no pointers. Host thread.
        unsafe { get(self.this()) }
    }

    /// `getErrorMessage`, through a buffer larger than the SDK's 124 bytes.
    pub(crate) fn error_message(&mut self) -> String {
        let mut text = [0u8; abi::ERROR_MESSAGE_BUFFER];
        let get = self.vtbl().getErrorMessage;
        // SAFETY: `getErrorMessage(char *string)` writes a NUL-terminated string of at most 124
        // bytes (`iasiodrv.h`); the buffer is 256 bytes and lives across the call. Host thread.
        unsafe { get(self.this(), text.as_mut_ptr()) };
        abi::c_string(&text)
    }

    pub(crate) fn channels(&mut self) -> Result<(i32, i32), i32> {
        let (mut inputs, mut outputs) = (0, 0);
        let get = self.vtbl().getChannels;
        // SAFETY: `getChannels(long *, long *)` writes one `long` through each pointer; both are
        // locals that outlive the call. Host thread, after a successful `init`.
        check(unsafe { get(self.this(), &mut inputs, &mut outputs) })?;
        Ok((inputs, outputs))
    }

    pub(crate) fn latencies(&mut self) -> Result<(i32, i32), i32> {
        let (mut input, mut output) = (0, 0);
        let get = self.vtbl().getLatencies;
        // SAFETY: `getLatencies(long *, long *)` writes one `long` through each local pointer.
        // Host thread, after a successful `init`.
        check(unsafe { get(self.this(), &mut input, &mut output) })?;
        Ok((input, output))
    }

    pub(crate) fn buffer_size(&mut self) -> Result<BufferSize, i32> {
        let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
        let get = self.vtbl().getBufferSize;
        // SAFETY: `getBufferSize(long *, long *, long *, long *)` writes one `long` through each of
        // the four local pointers. Host thread, after a successful `init`.
        check(unsafe { get(self.this(), &mut min, &mut max, &mut preferred, &mut granularity) })?;
        Ok(BufferSize { min, max, preferred, granularity })
    }

    pub(crate) fn can_sample_rate(&mut self, rate: f64) -> Result<(), i32> {
        let can = self.vtbl().canSampleRate;
        // SAFETY: `canSampleRate(ASIOSampleRate)` takes a `double` by value on Windows
        // (`asiosys.h`: IEEE754_64FLOAT); no pointers. Host thread, after a successful `init`.
        check(unsafe { can(self.this(), rate) })
    }

    pub(crate) fn sample_rate(&mut self) -> Result<f64, i32> {
        let mut rate = 0.0;
        let get = self.vtbl().getSampleRate;
        // SAFETY: `getSampleRate(ASIOSampleRate *)` writes one `double` through the local pointer.
        // Host thread, after a successful `init`.
        check(unsafe { get(self.this(), &mut rate) })?;
        Ok(rate)
    }

    pub(crate) fn set_sample_rate(&mut self, rate: f64) -> Result<(), i32> {
        let set = self.vtbl().setSampleRate;
        // SAFETY: `setSampleRate(ASIOSampleRate)` takes a `double` by value; no pointers. Host
        // thread, after a successful `init`, with no buffers created in this slice.
        check(unsafe { set(self.this(), rate) })
    }

    pub(crate) fn channel_info(&mut self, channel: i32, input: bool) -> Result<AsioChannelInfo, i32> {
        let mut info = AsioChannelInfo::query(channel, input);
        let get = self.vtbl().getChannelInfo;
        // SAFETY: `getChannelInfo(ASIOChannelInfo *)` reads `channel` and `isInput` and fills the
        // rest of the 52-byte structure (layout asserted in `abi`); the local outlives the call.
        // Host thread, after a successful `init`, with `channel` below the `getChannels` count.
        check(unsafe { get(self.this(), &mut info) })?;
        Ok(info)
    }

    pub(crate) fn control_panel(&mut self) -> Result<(), i32> {
        let panel = self.vtbl().controlPanel;
        // SAFETY: `controlPanel()` takes no pointers; drivers show their panel modally or as their
        // own window, parented (if at all) to the `sysRef` given to `init`. Host thread.
        check(unsafe { panel(self.this()) })
    }

    pub(crate) fn stop(&mut self) -> Result<(), i32> {
        let stop = self.vtbl().stop;
        // SAFETY: `stop()` takes no pointers. Host thread. Only called when buffers exist.
        check(unsafe { stop(self.this()) })
    }

    pub(crate) fn dispose_buffers(&mut self) -> Result<(), i32> {
        let dispose = self.vtbl().disposeBuffers;
        // SAFETY: `disposeBuffers()` takes no pointers; called after `stop` and only when
        // `createBuffers` succeeded ("buffers are valid until ASIODisposeBuffers"). Host thread.
        check(unsafe { dispose(self.this()) })
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let release = self.vtbl().Release;
        // SAFETY: releases the one reference `from_raw` took over, exactly once, on the creating
        // thread; the pointer is not used afterwards (`self` is being dropped).
        unsafe { release(self.this()) };
    }
}

fn check(code: i32) -> Result<(), i32> {
    if code == abi::ASE_OK || code == abi::ASE_SUCCESS { Ok(()) } else { Err(code) }
}

/// The text of a failed `CoCreateInstance` of a driver: the HRESULT and the DLL.
pub(crate) fn create_error(entry: &DriverEntry, hr: HRESULT, message: &str) -> ab::OpenError {
    let dll = entry.dll.as_deref().unwrap_or("(no InprocServer32 registered)");
    ab::OpenError::Other(format!(
        "ASIO driver \"{}\": CoCreateInstance({}) failed: HRESULT {:#010x} ({message}); DLL {dll}",
        entry.name, entry.clsid_text, hr.0 as u32
    ))
}

/// Creates the driver object of `entry` in-process, with its CLSID as the interface id (the
/// ASIO convention). Call on the STA host thread.
pub(crate) fn create_driver(entry: &DriverEntry) -> Result<Driver, ab::OpenError> {
    let mut results = [MULTI_QI { pIID: &entry.clsid, pItf: ManuallyDrop::new(None), hr: HRESULT(0) }];
    // SAFETY: `CoCreateInstanceEx` with a CLSID that outlives the call, no aggregation, an
    // in-process server, no server info and one MULTI_QI whose `pIID` points to the same CLSID
    // (ASIO drivers answer their CLSID as the IASIO interface id). COM was initialized on this
    // thread (STA) by the host before any call.
    let created = unsafe { CoCreateInstanceEx(&entry.clsid, None, CLSCTX_INPROC_SERVER, None, &mut results) };
    // SAFETY: the interface slot is taken exactly once and `results` is not read again; a
    // returned interface carries one reference that moves into the `Option`.
    let itf = unsafe { ManuallyDrop::take(&mut results[0].pItf) };
    if let Err(e) = created {
        return Err(create_error(entry, e.code(), &e.message()));
    }
    if results[0].hr.is_err() {
        return Err(create_error(entry, results[0].hr, &windows_core::Error::from_hresult(results[0].hr).message()));
    }
    let Some(itf) = itf else { return Err(create_error(entry, HRESULT(0), "no interface returned")) };
    // SAFETY: `into_raw` hands over the one reference of `itf` without releasing it; the pointer
    // is the `IASIO` interface asked for by `pIID`, created on this thread and used only here.
    unsafe { Driver::from_raw(itf.into_raw()) }.ok_or_else(|| create_error(entry, HRESULT(0), "null interface"))
}

/// Initializes COM as a single-threaded apartment on the calling thread and uninitializes it on
/// drop. The host thread holds it for its whole life.
pub(crate) struct StaGuard;

impl StaGuard {
    pub(crate) fn new() -> Result<Self, ab::OpenError> {
        // SAFETY: `CoInitializeEx` is called once on this fresh thread before any COM call; a
        // success code (`S_OK`, `S_FALSE`) is balanced by `Drop` on the same thread, a failure is
        // returned and not balanced.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        hr.ok().map_err(|e| ab::OpenError::Other(format!("CoInitializeEx(STA) on the ASIO host thread: {e}")))?;
        Ok(Self)
    }
}

impl Drop for StaGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` of `new` on the same thread; the host
        // drops it after releasing every driver and destroying its window.
        unsafe { CoUninitialize() };
    }
}

/// The host thread's hidden message-only window (`HWND_MESSAGE`), passed to drivers as `sysRef`.
pub(crate) struct MessageWindow(HWND);

impl MessageWindow {
    pub(crate) fn new() -> Result<Self, ab::OpenError> {
        // SAFETY: creates a window of the system "STATIC" class (no class registration, no
        // window procedure of ours) as a child of `HWND_MESSAGE`, so it is never shown and gets
        // only posted and sent messages. The window belongs to this thread, which destroys it.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("transit ASIO host"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map(Self)
        .map_err(|e| ab::OpenError::Other(format!("CreateWindowExW(HWND_MESSAGE) for the ASIO host: {e}")))
    }

    /// The `sysRef` for `IASIO::init`: the window handle.
    pub(crate) fn sys_ref(&self) -> *mut c_void {
        self.0.0
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread by `new` and is destroyed once, here,
        // after every driver given it as `sysRef` has been released.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// An auto-reset event that wakes the host thread when a request is queued.
pub(crate) struct Event(HANDLE);

// SAFETY: a Win32 event handle is valid on every thread of the process until `CloseHandle`;
// `SetEvent` and the wait functions may be called on it concurrently from any thread. It is
// closed once, in `Drop`, when the last owner goes.
unsafe impl Send for Event {}
// SAFETY: as above: all shared uses are the thread-safe Win32 event calls.
unsafe impl Sync for Event {}

impl Event {
    pub(crate) fn new() -> Result<Self, ab::OpenError> {
        // SAFETY: an unnamed auto-reset event with default security, initially not signaled; the
        // handle is closed in `Drop`.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map(Self)
            .map_err(|e| ab::OpenError::Other(format!("CreateEventW for the ASIO host: {e}")))
    }

    pub(crate) fn set(&self) {
        // SAFETY: `self.0` is a live event handle (closed only in `Drop`).
        let _ = unsafe { SetEvent(self.0) };
    }

    /// Dispatches every queued window message of this thread, then sleeps until the event is set
    /// or a new message arrives. Called by the host thread between requests.
    pub(crate) fn pump_and_wait(&self) {
        let mut msg = MSG::default();
        // SAFETY: `PeekMessageW` with `PM_REMOVE` fills the local `MSG`; `TranslateMessage` and
        // `DispatchMessageW` get a message just retrieved on this thread (drivers may post to the
        // `sysRef` window or create windows of their own on the host thread).
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: as above.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // SAFETY: waits on one live event handle (a slice that outlives the call) or any input
        // for this thread's queue; INFINITE is safe because a request or shutdown always sets the
        // event.
        let _ = unsafe { MsgWaitForMultipleObjects(Some(&[self.0]), false, INFINITE, QS_ALLINPUT) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `CreateEventW` and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: PCWSTR) -> Result<Option<Self>, windows_core::Error> {
        let mut key = HKEY::default();
        // SAFETY: `RegOpenKeyExW` with a predefined or open parent key, a NUL-terminated path
        // that outlives the call and a local out-pointer; the opened key is closed in `Drop`.
        let status = unsafe { RegOpenKeyExW(parent, path, None, KEY_READ, &mut key) };
        match status {
            ERROR_SUCCESS => Ok(Some(Self(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            other => Err(windows_core::Error::from_hresult(other.to_hresult())),
        }
    }

    /// The subkey names, in registry order.
    fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: `RegEnumKeyExW` on an open key writes at most `len` UTF-16 units (with the
            // terminator) into the local buffer and the length into `len`.
            let status = unsafe { RegEnumKeyExW(self.0, index, Some(windows_core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
            match status {
                ERROR_SUCCESS => names.push(String::from_utf16_lossy(&name[..len as usize])),
                ERROR_MORE_DATA => log::warn!("ASIO: a driver key name longer than 255 characters is skipped"),
                ERROR_NO_MORE_ITEMS => break,
                other => {
                    log::warn!("ASIO: RegEnumKeyExW failed at index {index}: {}", windows_core::Error::from_hresult(other.to_hresult()));
                    break;
                }
            }
        }
        names
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `Key::open` and is closed exactly once, here.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// A string value (`REG_SZ`, or `REG_EXPAND_SZ` expanded) of `parent\subkey`, `None` when absent.
fn reg_string(parent: HKEY, subkey: &str, value: Option<&str>) -> Option<String> {
    let subkey = wide(subkey);
    let value = value.map(wide);
    let value_ptr = value.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut bytes = 0u32;
    // SAFETY: a size query: no data pointer, the needed byte count is written to a local. The
    // subkey and value names are NUL-terminated and outlive the call.
    let status = unsafe { RegGetValueW(parent, PCWSTR(subkey.as_ptr()), value_ptr, flags, None, None, Some(&mut bytes)) };
    if status != ERROR_SUCCESS || bytes == 0 {
        return None;
    }
    let mut data = vec![0u16; (bytes as usize).div_ceil(2) + 1];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: as above, with a buffer of `size` bytes that outlives the call; the function writes
    // at most `size` bytes including the terminator and updates `size`.
    let status = unsafe { RegGetValueW(parent, PCWSTR(subkey.as_ptr()), value_ptr, flags, None, Some(data.as_mut_ptr().cast()), Some(&mut size)) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (size as usize / 2).min(data.len());
    let end = data[..units].iter().position(|u| *u == 0).unwrap_or(units);
    Some(String::from_utf16_lossy(&data[..end]))
}

/// The installed drivers from `HKLM\SOFTWARE\ASIO`, without loading any. A key without a
/// parsable `CLSID` is skipped with a warning (it cannot be created).
pub(crate) fn registry_drivers() -> Result<Vec<DriverEntry>, ab::OpenError> {
    let root = match Key::open(HKEY_LOCAL_MACHINE, ASIO_KEY) {
        Ok(Some(key)) => key,
        Ok(None) => return Ok(Vec::new()),
        Err(e) => return Err(ab::OpenError::Other(format!("reading HKLM\\SOFTWARE\\ASIO: {e}"))),
    };
    let mut entries = Vec::new();
    for name in root.subkeys() {
        let Some(clsid_text) = reg_string(root.0, &name, Some("CLSID")) else {
            log::warn!("ASIO: driver \"{name}\" has no CLSID value; skipped");
            continue;
        };
        let Some(clsid) = parse_guid(&clsid_text) else {
            log::warn!("ASIO: driver \"{name}\" has an unreadable CLSID {clsid_text:?}; skipped");
            continue;
        };
        let description = reg_string(root.0, &name, Some("Description"));
        let server = format!("CLSID\\{clsid_text}\\InprocServer32");
        let dll = reg_string(HKEY_CLASSES_ROOT, &server, None);
        let threading_model = reg_string(HKEY_CLASSES_ROOT, &server, Some("ThreadingModel"));
        let dll_exists = dll.as_deref().is_some_and(|p| std::path::Path::new(p).is_file());
        entries.push(DriverEntry { name, clsid, clsid_text, description, dll, dll_exists, threading_model });
    }
    Ok(entries)
}

/// The real loader: the registry and `CoCreateInstance`.
pub(crate) struct ComLoader;

impl Loader for ComLoader {
    fn list(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
        registry_drivers()
    }
    fn create(&self, entry: &DriverEntry) -> Result<Driver, ab::OpenError> {
        create_driver(entry)
    }
}

/// A fake driver object behind the real vtable layout, for tests: a `#[repr(C)]` struct whose
/// first field is a pointer to an [`IAsioVtbl`] of Rust functions, created and released like a
/// driver and called through the same [`Driver`] wrapper.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::ThreadId;

    /// A latch a fake `init` waits on.
    #[derive(Default)]
    pub(crate) struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        pub(crate) fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }
    }

    /// What a fake driver reports.
    #[derive(Clone)]
    pub(crate) struct FakeSpec {
        pub driver_name: String,
        pub inputs: i32,
        pub outputs: i32,
        pub input_type: i32,
        pub output_type: i32,
        /// (min, max, preferred, granularity).
        pub buffer: (i32, i32, i32, i32),
        pub rates: Vec<f64>,
        pub current_rate: f64,
        pub latencies: (i32, i32),
        pub init_ok: bool,
        pub init_message: String,
        pub init_gate: Option<Arc<Gate>>,
        pub channels_error: Option<i32>,
        pub panel_error: Option<i32>,
    }

    impl FakeSpec {
        pub(crate) fn new(name: &str, inputs: i32, outputs: i32, sample_type: i32) -> Self {
            Self {
                driver_name: name.into(),
                inputs,
                outputs,
                input_type: sample_type,
                output_type: sample_type,
                buffer: (32, 2048, 256, -1),
                rates: vec![44_100.0, 48_000.0],
                current_rate: 48_000.0,
                latencies: (300, 400),
                init_ok: true,
                init_message: String::new(),
                init_gate: None,
                channels_error: None,
                panel_error: None,
            }
        }
    }

    /// What tests observe across all fake objects of one loader.
    #[derive(Default)]
    pub(crate) struct Observed {
        pub live: AtomicUsize,
        pub max_live: AtomicUsize,
        pub created: AtomicUsize,
        pub released: AtomicUsize,
        pub panels: AtomicUsize,
        pub threads: Mutex<Vec<ThreadId>>,
        pub sys_refs: Mutex<Vec<usize>>,
    }

    #[repr(C)]
    struct FakeObject {
        vtbl: *const IAsioVtbl,
        refs: AtomicU32,
        rate: Mutex<f64>,
        spec: FakeSpec,
        observed: Arc<Observed>,
    }

    /// The fake object behind `this`.
    ///
    /// # Safety
    ///
    /// `this` came from [`create`] and has not been released for the last time.
    unsafe fn object<'a>(this: *mut IAsio) -> &'a FakeObject {
        // SAFETY: the caller's contract: `this` is a live `Box<FakeObject>` whose first field is
        // the vtable pointer, so the `IAsio` pointer is the object pointer.
        let obj = unsafe { &*this.cast::<FakeObject>() };
        obj.observed.threads.lock().unwrap().push(std::thread::current().id());
        obj
    }

    unsafe extern "C" fn query_interface(_this: *mut IAsio, _riid: *const [u8; 16], object: *mut *mut c_void) -> i32 {
        // SAFETY: COM contract: `object` is a valid out-pointer.
        unsafe { *object = std::ptr::null_mut() };
        windows::Win32::Foundation::E_NOINTERFACE.0
    }
    unsafe extern "C" fn add_ref(this: *mut IAsio) -> u32 {
        // SAFETY: called through a live object's vtable.
        unsafe { object(this) }.refs.fetch_add(1, Ordering::AcqRel) + 1
    }
    unsafe extern "C" fn release(this: *mut IAsio) -> u32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        let left = obj.refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            obj.observed.live.fetch_sub(1, Ordering::AcqRel);
            obj.observed.released.fetch_add(1, Ordering::AcqRel);
            // SAFETY: the last reference: the box came from `Box::into_raw` in `create` and is
            // freed once; `obj` is not used after this.
            drop(unsafe { Box::from_raw(this.cast::<FakeObject>()) });
        }
        left
    }
    unsafe extern "C" fn init(this: *mut IAsio, sys: *mut c_void) -> i32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        obj.observed.sys_refs.lock().unwrap().push(sys as usize);
        if let Some(gate) = &obj.spec.init_gate {
            gate.wait();
        }
        i32::from(obj.spec.init_ok)
    }
    unsafe extern "C" fn get_driver_name(this: *mut IAsio, name: *mut u8) {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        let bytes = obj.spec.driver_name.as_bytes();
        let n = bytes.len().min(31);
        // SAFETY: the SDK contract: `name` holds at least 32 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), name, n);
            *name.add(n) = 0;
        }
    }
    unsafe extern "C" fn get_driver_version(_this: *mut IAsio) -> i32 {
        7
    }
    unsafe extern "C" fn get_error_message(this: *mut IAsio, text: *mut u8) {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        let bytes = obj.spec.init_message.as_bytes();
        let n = bytes.len().min(123);
        // SAFETY: the SDK contract: `text` holds at least 124 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), text, n);
            *text.add(n) = 0;
        }
    }
    unsafe extern "C" fn ok(_this: *mut IAsio) -> i32 {
        abi::ASE_OK
    }
    unsafe extern "C" fn get_channels(this: *mut IAsio, inputs: *mut i32, outputs: *mut i32) -> i32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        if let Some(code) = obj.spec.channels_error {
            return code;
        }
        // SAFETY: valid out-pointers by the SDK contract.
        unsafe {
            *inputs = obj.spec.inputs;
            *outputs = obj.spec.outputs;
        }
        abi::ASE_OK
    }
    unsafe extern "C" fn get_latencies(this: *mut IAsio, input: *mut i32, output: *mut i32) -> i32 {
        // SAFETY: called through a live object's vtable; valid out-pointers by the SDK contract.
        unsafe {
            let obj = object(this);
            *input = obj.spec.latencies.0;
            *output = obj.spec.latencies.1;
        }
        abi::ASE_OK
    }
    unsafe extern "C" fn get_buffer_size(this: *mut IAsio, min: *mut i32, max: *mut i32, preferred: *mut i32, granularity: *mut i32) -> i32 {
        // SAFETY: called through a live object's vtable; valid out-pointers by the SDK contract.
        unsafe {
            let (a, b, c, d) = object(this).spec.buffer;
            *min = a;
            *max = b;
            *preferred = c;
            *granularity = d;
        }
        abi::ASE_OK
    }
    unsafe extern "C" fn can_sample_rate(this: *mut IAsio, rate: f64) -> i32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        if obj.spec.rates.contains(&rate) { abi::ASE_OK } else { abi::ASE_NO_CLOCK }
    }
    unsafe extern "C" fn get_sample_rate(this: *mut IAsio, rate: *mut f64) -> i32 {
        // SAFETY: called through a live object's vtable; a valid out-pointer by the SDK contract.
        unsafe { *rate = *object(this).rate.lock().unwrap() };
        abi::ASE_OK
    }
    unsafe extern "C" fn set_sample_rate(this: *mut IAsio, rate: f64) -> i32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        if !obj.spec.rates.contains(&rate) {
            return abi::ASE_NO_CLOCK;
        }
        *obj.rate.lock().unwrap() = rate;
        abi::ASE_OK
    }
    unsafe extern "C" fn get_clock_sources(_this: *mut IAsio, _clocks: *mut abi::AsioClockSource, count: *mut i32) -> i32 {
        // SAFETY: a valid out-pointer by the SDK contract.
        unsafe { *count = 0 };
        abi::ASE_OK
    }
    unsafe extern "C" fn set_clock_source(_this: *mut IAsio, _reference: i32) -> i32 {
        abi::ASE_OK
    }
    unsafe extern "C" fn get_sample_position(_this: *mut IAsio, _position: *mut abi::AsioSamples, _stamp: *mut abi::AsioTimeStamp) -> i32 {
        abi::ASE_SP_NOT_ADVANCING
    }
    unsafe extern "C" fn get_channel_info(this: *mut IAsio, info: *mut AsioChannelInfo) -> i32 {
        // SAFETY: called through a live object's vtable; `info` is valid by the SDK contract.
        unsafe {
            let obj = object(this);
            let info = &mut *info;
            let (count, sample) = if info.isInput != 0 { (obj.spec.inputs, obj.spec.input_type) } else { (obj.spec.outputs, obj.spec.output_type) };
            if info.channel < 0 || info.channel >= count {
                return abi::ASE_INVALID_PARAMETER;
            }
            info.isActive = 0;
            info.channelGroup = 0;
            info.type_ = sample;
            info.name = [0; 32];
            info.name[0] = b'C';
        }
        abi::ASE_OK
    }
    unsafe extern "C" fn create_buffers(_this: *mut IAsio, _i: *mut abi::AsioBufferInfo, _c: i32, _f: i32, _cb: *mut abi::AsioCallbacks) -> i32 {
        abi::ASE_INVALID_MODE
    }
    unsafe extern "C" fn control_panel(this: *mut IAsio) -> i32 {
        // SAFETY: called through a live object's vtable.
        let obj = unsafe { object(this) };
        obj.observed.panels.fetch_add(1, Ordering::AcqRel);
        obj.spec.panel_error.unwrap_or(abi::ASE_OK)
    }
    unsafe extern "C" fn future(_this: *mut IAsio, _selector: i32, _opt: *mut c_void) -> i32 {
        abi::ASE_INVALID_PARAMETER
    }

    static VTBL: IAsioVtbl = IAsioVtbl {
        QueryInterface: query_interface,
        AddRef: add_ref,
        Release: release,
        init,
        getDriverName: get_driver_name,
        getDriverVersion: get_driver_version,
        getErrorMessage: get_error_message,
        start: ok,
        stop: ok,
        getChannels: get_channels,
        getLatencies: get_latencies,
        getBufferSize: get_buffer_size,
        canSampleRate: can_sample_rate,
        getSampleRate: get_sample_rate,
        setSampleRate: set_sample_rate,
        getClockSources: get_clock_sources,
        setClockSource: set_clock_source,
        getSamplePosition: get_sample_position,
        getChannelInfo: get_channel_info,
        createBuffers: create_buffers,
        disposeBuffers: ok,
        controlPanel: control_panel,
        future,
        outputReady: ok,
    };

    /// A new fake driver object with one reference.
    pub(crate) fn create(spec: FakeSpec, observed: &Arc<Observed>) -> Driver {
        let live = observed.live.fetch_add(1, Ordering::AcqRel) + 1;
        observed.max_live.fetch_max(live, Ordering::AcqRel);
        observed.created.fetch_add(1, Ordering::AcqRel);
        let rate = Mutex::new(spec.current_rate);
        let obj = Box::new(FakeObject { vtbl: &VTBL, refs: AtomicU32::new(1), rate, spec, observed: Arc::clone(observed) });
        // SAFETY: a fresh `FakeObject` whose first field is the vtable pointer, with one reference
        // that the `Driver` owns; released (and freed) through the vtable's `Release`.
        unsafe { Driver::from_raw(Box::into_raw(obj).cast()) }.expect("non-null")
    }

    /// A loader of fake drivers keyed by registry name.
    pub(crate) struct FakeLoader {
        pub drivers: Vec<(DriverEntry, FakeSpec)>,
        /// Names whose creation fails like a failed `CoCreateInstance`.
        pub create_fails: Vec<String>,
        pub observed: Arc<Observed>,
    }

    impl FakeLoader {
        pub(crate) fn new(drivers: Vec<(DriverEntry, FakeSpec)>) -> (Self, Arc<Observed>) {
            let observed = Arc::new(Observed::default());
            (Self { drivers, create_fails: Vec::new(), observed: Arc::clone(&observed) }, observed)
        }
    }

    impl Loader for FakeLoader {
        fn list(&self) -> Result<Vec<DriverEntry>, ab::OpenError> {
            Ok(self.drivers.iter().map(|(e, _)| e.clone()).collect())
        }
        fn create(&self, entry: &DriverEntry) -> Result<Driver, ab::OpenError> {
            if self.create_fails.contains(&entry.name) {
                return Err(create_error(entry, windows::Win32::Foundation::REGDB_E_CLASSNOTREG, "Class not registered"));
            }
            let (_, spec) = self.drivers.iter().find(|(e, _)| e.name == entry.name).expect("a fake driver of that name");
            Ok(create(spec.clone(), &self.observed))
        }
    }

    /// A registry entry for a fake driver.
    pub(crate) fn entry(name: &str, n: u128) -> DriverEntry {
        DriverEntry {
            name: name.into(),
            clsid: windows_core::GUID::from_u128(n),
            clsid_text: format!("{{{n:032X}}}"),
            description: Some(name.into()),
            dll: Some(format!("C:\\fake\\{n}.dll")),
            dll_exists: true,
            threading_model: Some("Apartment".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeSpec, Observed, create};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[test]
    fn the_wrapper_calls_every_queried_method_through_the_vtable_and_releases_once() {
        let observed = Arc::new(Observed::default());
        let mut spec = FakeSpec::new("Fake ASIO", 12, 10, abi::ASIOST_INT24_LSB);
        spec.init_message = "no hardware".into();
        let mut driver = create(spec, &observed);
        assert!(driver.init(0x1234 as *mut c_void));
        assert_eq!(driver.driver_name(), "Fake ASIO");
        assert_eq!(driver.driver_version(), 7);
        assert_eq!(driver.error_message(), "no hardware");
        assert_eq!(driver.channels(), Ok((12, 10)));
        assert_eq!(driver.latencies(), Ok((300, 400)));
        assert_eq!(driver.buffer_size(), Ok(BufferSize { min: 32, max: 2048, preferred: 256, granularity: -1 }));
        assert_eq!(driver.can_sample_rate(48_000.0), Ok(()));
        assert_eq!(driver.can_sample_rate(96_000.0), Err(abi::ASE_NO_CLOCK));
        assert_eq!(driver.set_sample_rate(44_100.0), Ok(()));
        assert_eq!(driver.sample_rate(), Ok(44_100.0));
        let info = driver.channel_info(11, true).expect("channel 11 in");
        assert_eq!(info.type_, abi::ASIOST_INT24_LSB);
        assert_eq!(driver.channel_info(10, false).err(), Some(abi::ASE_INVALID_PARAMETER));
        assert_eq!(driver.control_panel(), Ok(()));
        assert_eq!(observed.sys_refs.lock().unwrap().as_slice(), &[0x1234]);
        assert_eq!(observed.live.load(Ordering::Acquire), 1);
        drop(driver);
        assert_eq!(observed.live.load(Ordering::Acquire), 0);
        assert_eq!(observed.released.load(Ordering::Acquire), 1);
    }

    #[test]
    fn the_registry_lists_without_loading() {
        // Reads HKLM\SOFTWARE\ASIO on whatever machine runs the tests; an empty list is fine.
        let entries = registry_drivers().expect("the registry read does not fail");
        for e in &entries {
            assert!(!e.name.is_empty());
            assert_eq!(parse_guid(&e.clsid_text), Some(e.clsid));
        }
    }
}
