//! The COM and Win32 calls of the backend: the only `unsafe` in the crate (ADR 0013, second
//! amendment). Every COM object lives on one thread that initialized COM as MTA: a short-lived
//! thread for enumeration and planning, the stream thread for a stream, the watcher thread for
//! device notifications. No interface pointer crosses threads (so there is no `unsafe impl Send`
//! for a COM type); what crosses is plain data, strings and kernel event handles.
#![allow(clippy::disallowed_macros, clippy::disallowed_methods)]

use crate::convert::Wire;
use crate::render_loop::{self, E_DEVICE_IN_USE, Fault, LoopConfig, Position, RenderDevice, Stamper, Wake};
use soundcraft_audio_backend as ab;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDCLNT_STREAMOPTIONS_RAW, AudioCategory_Media, AudioClientProperties, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioClient,
    IAudioClient2, IAudioClient3, IAudioClock, IAudioClock2, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows::core::{GUID, Interface, PCWSTR, PWSTR, implement};

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);
/// `PKEY_Devices_AudioDevice_RawProcessingSupported` (`devpkey.h`: {B3F8FA53-0004-438E-9003-51A46E139BFC}, 3).
const PKEY_RAW_SUPPORTED: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0xb3f8fa53_0004_438e_9003_51a46e139bfc), pid: 3 };
/// `AUDCLNT_BUFFERFLAGS_SILENT` for `ReleaseBuffer`.
const BUFFER_SILENT: u32 = 2;
/// `ERROR_NOT_FOUND` / `E_NOTFOUND`-style "no such endpoint" codes that mean the device is gone.
const E_ELEMENT_NOT_FOUND: i32 = 0x8007_0490_u32 as i32;
/// How long the control thread waits for the stream thread to open or start.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

fn open_error(what: &str, error: &windows::core::Error) -> ab::OpenError {
    let text = format!("{what}: {error}");
    match error.code().0 {
        E_DEVICE_IN_USE => ab::OpenError::InUse(text),
        E_ELEMENT_NOT_FOUND => ab::OpenError::DeviceNotAvailable(text),
        code => match Fault::from_hresult(code) {
            Fault::DeviceGone => ab::OpenError::DeviceNotAvailable(text),
            Fault::Other(_) => ab::OpenError::Other(text),
        },
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Initializes COM as MTA on the calling thread and uninitializes it on drop. Fails with
/// `RPC_E_CHANGED_MODE` on a thread that is already STA (winit's UI thread), so only threads
/// this crate spawns create COM objects.
struct ComGuard;

impl ComGuard {
    fn mta() -> Result<Self, ab::OpenError> {
        // SAFETY: `CoInitializeEx` must be called once per thread before any other COM call and
        // paired with `CoUninitialize` on that thread; a success code (`S_OK` or `S_FALSE`) is
        // balanced by the `Drop` below, and a failure code is not (checked here).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().map_err(|e| open_error("CoInitializeEx(MTA)", &e))?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` of `mta` on this same thread, and
        // runs after every COM object of the thread is dropped (guards are declared first).
        unsafe { CoUninitialize() };
    }
}

/// Run `work` on a fresh MTA thread and return its result (enumeration and planning, called
/// from control threads that may be STA). Threads are scoped, so `work` may borrow.
pub(crate) fn on_mta<R: Send>(work: impl FnOnce() -> Result<R, ab::OpenError> + Send) -> Result<R, ab::OpenError> {
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new().name("transit-wasapi-control".into()).spawn_scoped(scope, || {
            let _com = ComGuard::mta()?;
            work()
        });
        match thread.map(|t| t.join()) {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ab::OpenError::Other("the WASAPI control thread panicked".into())),
            Err(e) => Err(ab::OpenError::Other(format!("could not start the WASAPI control thread: {e}"))),
        }
    })
}

/// A kernel event handle. Kernel handles are process-wide and `SetEvent` and the wait calls are
/// thread-safe, so sharing the handle is not sharing a COM pointer.
struct Event(HANDLE);

// SAFETY: a Win32 event handle is an index into the process handle table, valid on every thread
// until `CloseHandle`; `SetEvent` and `WaitForMultipleObjects` may be called on it concurrently
// from any thread. It is closed once, in `Drop`, when the last `Arc` goes.
unsafe impl Send for Event {}
// SAFETY: as above: all shared uses are the thread-safe Win32 event calls.
unsafe impl Sync for Event {}

impl Event {
    fn new(manual_reset: bool) -> Result<Self, ab::OpenError> {
        // SAFETY: `CreateEventW` with no security attributes and no name creates an unnamed event
        // owned by this process; the returned handle is closed in `Drop`.
        unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }.map(Self).map_err(|e| open_error("CreateEventW", &e))
    }
    fn set(&self) {
        // SAFETY: `self.0` is a live event handle (closed only in `Drop`).
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `CreateEventW` and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The parsed shared-mode format of an endpoint.
#[derive(Clone, Copy)]
pub(crate) struct Fmt {
    pub(crate) rate: u32,
    pub(crate) channels: u16,
    pub(crate) sample: ab::SampleFormat,
    pub(crate) wire: Wire,
    pub(crate) valid_bits: u16,
    pub(crate) mask: u32,
}

impl Fmt {
    pub(crate) fn format(&self) -> ab::Format {
        let container = (self.wire.bytes() * 8) as u16;
        ab::Format {
            rate: self.rate,
            channels: self.channels,
            sample: self.sample,
            valid_bits: (self.valid_bits != container && self.valid_bits != 0).then_some(self.valid_bits as u8),
            channel_mask: Some(self.mask),
        }
    }
}

/// Parse a `WAVEFORMATEX` (plain or extensible) copied into `raw`.
fn parse_format(raw: &WAVEFORMATEXTENSIBLE) -> Result<Fmt, ab::OpenError> {
    let (tag, channels, rate, bits) = (raw.Format.wFormatTag, raw.Format.nChannels, raw.Format.nSamplesPerSec, raw.Format.wBitsPerSample);
    let extensible = tag == WAVE_FORMAT_EXTENSIBLE && raw.Format.cbSize >= 22;
    let (float, valid_bits, mask) = if extensible {
        // SAFETY: for a `WAVE_FORMAT_EXTENSIBLE` tag with `cbSize >= 22` the `Samples` union holds
        // `wValidBitsPerSample` (the documented interpretation; the other members are
        // `wSamplesPerBlock` and `wReserved`, all `u16`).
        let valid = unsafe { raw.Samples.wValidBitsPerSample };
        let sub = raw.SubFormat;
        (sub == SUBTYPE_FLOAT, valid, raw.dwChannelMask)
    } else {
        (tag == WAVE_FORMAT_IEEE_FLOAT, bits, 0)
    };
    if !extensible && tag != WAVE_FORMAT_IEEE_FLOAT && tag != WAVE_FORMAT_PCM {
        return Err(ab::OpenError::Other(format!("unsupported mix format tag {tag:#x}")));
    }
    let sub = raw.SubFormat;
    if extensible && !float && sub != SUBTYPE_PCM {
        return Err(ab::OpenError::Other("unsupported mix format subtype".into()));
    }
    let (sample, wire) = match (float, bits) {
        (true, 32) => (ab::SampleFormat::F32, Wire::F32),
        (false, 16) => (ab::SampleFormat::I16, Wire::I16),
        (false, 24) => (ab::SampleFormat::I24, Wire::I24),
        (false, 32) => (ab::SampleFormat::I32, Wire::I32),
        _ => return Err(ab::OpenError::Other(format!("unsupported mix sample size: {bits} bits, float {float}"))),
    };
    Ok(Fmt { rate, channels, sample, wire, valid_bits: valid_bits.max(1), mask })
}

/// `IAudioClient::GetMixFormat`, copied out and freed.
fn mix_format(client: &IAudioClient) -> Result<(WAVEFORMATEXTENSIBLE, Fmt), ab::OpenError> {
    // SAFETY: `GetMixFormat` returns a `CoTaskMemAlloc`ed `WAVEFORMATEX` (followed by `cbSize`
    // extra bytes) that the caller must free with `CoTaskMemFree`. We read at most
    // `size_of::<WAVEFORMATEX>() + cbSize` bytes, capped at the size of our copy, and free the
    // block exactly once after copying.
    let raw = unsafe {
        let p = client.GetMixFormat().map_err(|e| open_error("GetMixFormat", &e))?;
        let mut raw = WAVEFORMATEXTENSIBLE::default();
        let cb = usize::from(std::ptr::read_unaligned(std::ptr::addr_of!((*p).cbSize)));
        let len = (size_of::<WAVEFORMATEX>() + cb).min(size_of::<WAVEFORMATEXTENSIBLE>());
        std::ptr::copy_nonoverlapping(p.cast::<u8>(), (&raw mut raw).cast::<u8>(), len);
        CoTaskMemFree(Some(p.cast_const().cast()));
        raw
    };
    let fmt = parse_format(&raw)?;
    Ok((raw, fmt))
}

/// A float32 format at `rate` with the channels of `like`, for the auto-converting open.
fn float_format(like: &Fmt, rate: u32) -> (WAVEFORMATEXTENSIBLE, Fmt) {
    let mut raw = WAVEFORMATEXTENSIBLE::default();
    let block = like.channels * 4;
    raw.Format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_EXTENSIBLE,
        nChannels: like.channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 22,
    };
    raw.Samples.wValidBitsPerSample = 32;
    raw.dwChannelMask = like.mask;
    raw.SubFormat = SUBTYPE_FLOAT;
    (raw, Fmt { rate, sample: ab::SampleFormat::F32, wire: Wire::F32, valid_bits: 32, ..*like })
}

/// What `endpoints()` and `plan_render()` know of a render endpoint.
#[derive(Clone)]
pub(crate) struct EndpointInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) mix: Fmt,
    /// `GetSharedModeEnginePeriod` (min, max) in frames, where `IAudioClient3` exists.
    pub(crate) periods: Option<(u32, u32)>,
    /// Standard rates `IsFormatSupported` accepts in shared mode, besides the mix rate.
    pub(crate) rates: Vec<u32>,
}

const PROBE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

fn string_of(text: PWSTR) -> String {
    // SAFETY: `GetId` and the notification callbacks hand over a NUL-terminated UTF-16 string;
    // it is read once and, where the caller owns it (`GetId`), freed by `take_id`.
    unsafe { text.to_string() }.unwrap_or_default()
}

fn id_of(device: &IMMDevice) -> Result<String, ab::OpenError> {
    // SAFETY: `GetId` returns a `CoTaskMemAlloc`ed string owned by the caller; it is converted
    // and freed exactly once.
    unsafe {
        let id = device.GetId().map_err(|e| open_error("IMMDevice::GetId", &e))?;
        let text = string_of(id);
        CoTaskMemFree(Some(id.0.cast_const().cast()));
        Ok(text)
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, ab::OpenError> {
    // SAFETY: COM is initialized on this thread (`ComGuard`), and `MMDeviceEnumerator` is the
    // documented CLSID of `IMMDeviceEnumerator`.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| open_error("IMMDeviceEnumerator", &e))
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, ab::OpenError> {
    // SAFETY: `Activate` creates an interface of the requested type from a live `IMMDevice`;
    // no activation parameters are needed for `IAudioClient`.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(|e| open_error("Activate(IAudioClient)", &e))
}

fn friendly_name(device: &IMMDevice) -> String {
    // SAFETY: `OpenPropertyStore(STGM_READ)` and `GetValue` on a live device; the returned
    // `PROPVARIANT` owns its contents and clears them on drop.
    unsafe { device.OpenPropertyStore(STGM_READ).and_then(|s| s.GetValue(&PKEY_Device_FriendlyName)) }.map(|v| v.to_string()).unwrap_or_default()
}

fn raw_supported(device: &IMMDevice) -> bool {
    // SAFETY: as `friendly_name`; `bool::try_from` reads the variant by its `vt` tag.
    unsafe { device.OpenPropertyStore(STGM_READ).and_then(|s| s.GetValue(&PKEY_RAW_SUPPORTED)) }
        .ok()
        .and_then(|v| bool::try_from(&v).ok())
        .unwrap_or(false)
}

fn describe(device: &IMMDevice) -> Result<EndpointInfo, ab::OpenError> {
    let client = activate(device)?;
    let (mut raw, mix) = mix_format(&client)?;
    let periods = client.cast::<IAudioClient3>().ok().and_then(|c| {
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        // SAFETY: `raw` is a valid format of the mix format's layout for the call; the four
        // out-pointers are live `u32`s. `GetSharedModeEnginePeriod` is a query with no state.
        unsafe { c.GetSharedModeEnginePeriod((&raw mut raw).cast::<WAVEFORMATEX>(), &mut default, &mut fundamental, &mut min, &mut max) }.ok()?;
        Some((min, max))
    });
    let mut rates = Vec::new();
    for rate in PROBE_RATES.into_iter().filter(|r| *r != mix.rate) {
        let (probe, _) = float_format(&mix, rate);
        // SAFETY: `probe` is a valid extensible format for the call. In shared mode
        // `IsFormatSupported` returns `S_OK` for a format the engine accepts and `S_FALSE` with a
        // closest match (a `CoTaskMemAlloc`ed block, freed below) otherwise. No closest-match
        // pointer is requested, so nothing is allocated for us.
        let hr = unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, (&raw const probe).cast::<WAVEFORMATEX>(), None) };
        if hr.0 == 0 {
            rates.push(rate);
        }
    }
    Ok(EndpointInfo { id: id_of(device)?, name: friendly_name(device), mix, periods, rates })
}

/// The active endpoints of one direction, default first.
pub(crate) fn list_endpoints(output: bool) -> Result<Vec<EndpointInfo>, ab::OpenError> {
    on_mta(|| {
        let flow: EDataFlow = if output { eRender } else { eCapture };
        let enumerator = enumerator()?;
        // SAFETY: calls on the live enumerator and the collection it returned, indices below
        // `GetCount`; every returned interface is released by `windows` on drop.
        let devices = unsafe {
            let all = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE).map_err(|e| open_error("EnumAudioEndpoints", &e))?;
            let count = all.GetCount().map_err(|e| open_error("GetCount", &e))?;
            (0..count).filter_map(|i| all.Item(i).ok()).collect::<Vec<_>>()
        };
        // SAFETY: a query on the live enumerator.
        let default = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }.ok().and_then(|d| id_of(&d).ok());
        let mut found: Vec<EndpointInfo> = devices.iter().filter_map(|d| describe(d).ok()).collect();
        found.sort_by_key(|e| Some(&e.id) != default.as_ref());
        Ok(found)
    })
}

/// What a stream thread needs to open one render stream.
pub(crate) struct OpenParams {
    pub(crate) id: String,
    pub(crate) request: ab::OpenRequest,
}

/// The opened, not yet started, stream: owned by the stream thread.
struct Live {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock2,
    clock_frequency: u64,
    event: Event,
    stop: Arc<Event>,
    fmt: Fmt,
    buffer_frames: u32,
    period_frames: u32,
    latency_100ns: i64,
}

impl Live {
    fn open(params: &OpenParams, stop: Arc<Event>) -> Result<Self, ab::OpenError> {
        let device = {
            let wide = wide(&params.id);
            // SAFETY: `GetDevice` reads the NUL-terminated UTF-16 id in `wide`, valid for the call.
            unsafe { enumerator()?.GetDevice(PCWSTR(wide.as_ptr())) }.map_err(|e| open_error("GetDevice", &e))?
        };
        // Raw mode (APOs bypassed) where the endpoint supports it; a failure degrades to non-raw.
        let want_raw = raw_supported(&device);
        let client = activate(&device)?;
        if want_raw {
            apply_raw(&client);
        }
        let (mix_raw, mix) = mix_format(&client)?;
        let requested = params.request.format.rate;
        if requested != mix.rate && requested != 0 {
            // The session rate differs from the endpoint's: the engine converts.
            let (converted, fmt) = float_format(&mix, requested);
            let frames = Self::initialize_plain(&client, &converted, &fmt, true)?;
            return Self::finish(client, frames, fmt, stop);
        }
        match Self::initialize_low_latency(&client, mix_raw, params.request.period) {
            Ok(frames) => Self::finish(client, frames, mix, stop),
            Err(e) => {
                log::info!("IAudioClient3 low-latency open failed ({e}); using event-driven IAudioClient");
                // A failed `Initialize` cannot be retried on the same client.
                let client = activate(&device)?;
                if want_raw {
                    apply_raw(&client);
                }
                let frames = Self::initialize_plain(&client, &mix_raw, &mix, false)?;
                Self::finish(client, frames, mix, stop)
            }
        }
    }

    /// `IAudioClient3`: the requested period (else the minimum) within the engine's range.
    fn initialize_low_latency(client: &IAudioClient, mut mix_raw: WAVEFORMATEXTENSIBLE, period: ab::Period) -> Result<u32, ab::OpenError> {
        let client3 = client.cast::<IAudioClient3>().map_err(|e| open_error("IAudioClient3", &e))?;
        let format = (&raw mut mix_raw).cast::<WAVEFORMATEX>();
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        // SAFETY: `format` points at the mix format copy, valid for both calls; the out-pointers
        // are live `u32`s. `InitializeSharedAudioStream` takes a period within
        // `[min, max]` that is a multiple of `fundamental` and the exact mix format, and may be
        // called once on a client that has not been initialized.
        unsafe {
            client3
                .GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)
                .map_err(|e| open_error("GetSharedModeEnginePeriod", &e))?;
            let step = fundamental.max(1);
            let frames = match period {
                ab::Period::Frames(n) => ((n + step / 2) / step * step).clamp(min, max.max(min)),
                ab::Period::Default => min,
            };
            client3
                .InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, frames, format, None)
                .map_err(|e| open_error("InitializeSharedAudioStream", &e))?;
            Ok(frames)
        }
    }

    /// `IAudioClient::Initialize` event-driven at the engine's default period; with `convert`,
    /// the engine converts from `format` to the mix format (`AUTOCONVERTPCM`).
    fn initialize_plain(client: &IAudioClient, raw: &WAVEFORMATEXTENSIBLE, fmt: &Fmt, convert: bool) -> Result<u32, ab::OpenError> {
        let mut default_hns = 0i64;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | if convert { AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY } else { 0 };
        // SAFETY: `raw` is a valid format for the call; `GetDevicePeriod` fills the live `i64`;
        // `Initialize` is called once on an uninitialized client, shared mode with
        // `hnsPeriodicity` 0 (required for shared mode) and the default period as the buffer.
        unsafe {
            client.GetDevicePeriod(Some(&mut default_hns), None).map_err(|e| open_error("GetDevicePeriod", &e))?;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, default_hns, 0, (raw as *const WAVEFORMATEXTENSIBLE).cast::<WAVEFORMATEX>(), None)
                .map_err(|e| open_error("Initialize", &e))?;
        }
        Ok((default_hns.unsigned_abs() * u64::from(fmt.rate) / 10_000_000) as u32)
    }

    fn finish(client: IAudioClient, period_frames: u32, fmt: Fmt, stop: Arc<Event>) -> Result<Self, ab::OpenError> {
        let event = Event::new(false)?;
        // SAFETY: the client is initialized (event-driven); `SetEventHandle` takes a live event
        // handle that outlives the client (`event` is a field of `Live`, dropped after the
        // client's other users); `GetService` asks the initialized client for its services.
        let (render, clock, buffer_frames, latency) = unsafe {
            client.SetEventHandle(event.0).map_err(|e| open_error("SetEventHandle", &e))?;
            let render = client.GetService::<IAudioRenderClient>().map_err(|e| open_error("IAudioRenderClient", &e))?;
            let clock = client.GetService::<IAudioClock>().map_err(|e| open_error("IAudioClock", &e))?;
            let buffer_frames = client.GetBufferSize().map_err(|e| open_error("GetBufferSize", &e))?;
            (render, clock, buffer_frames, client.GetStreamLatency().unwrap_or(0))
        };
        let clock_frequency = unsafe_frequency(&clock)?;
        let clock = clock.cast::<IAudioClock2>().map_err(|e| open_error("IAudioClock2", &e))?;
        // SAFETY: before `Start` the whole buffer is ours: `GetBuffer(buffer_frames)` followed by
        // `ReleaseBuffer(buffer_frames, SILENT)` pre-fills it with silence, paired as required.
        unsafe {
            render.GetBuffer(buffer_frames).map_err(|e| open_error("GetBuffer", &e))?;
            render.ReleaseBuffer(buffer_frames, BUFFER_SILENT).map_err(|e| open_error("ReleaseBuffer", &e))?;
        }
        Ok(Self { client, render, clock, clock_frequency, event, stop, fmt, buffer_frames, period_frames, latency_100ns: latency })
    }

    /// Capture the `(QPC, Instant)` origin pair and start the stream.
    fn start(&self, origin: ab::HostOrigin) -> Result<Stamper, ab::OpenError> {
        let (mut ticks, mut frequency) = (0i64, 0i64);
        // SAFETY: `QueryPerformanceCounter` and `Frequency` fill live `i64`s and cannot fail on
        // Windows XP and later (the `Result` is still checked).
        unsafe {
            QueryPerformanceCounter(&mut ticks).map_err(|e| open_error("QueryPerformanceCounter", &e))?;
            QueryPerformanceFrequency(&mut frequency).map_err(|e| open_error("QueryPerformanceFrequency", &e))?;
        }
        let instant = std::time::Instant::now();
        let qpc_100ns = (ticks.unsigned_abs() as u128 * 10_000_000 / frequency.unsigned_abs().max(1) as u128) as u64;
        let origin_ns = match instant.checked_duration_since(origin.instant()) {
            Some(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            None => -i64::try_from(origin.instant().duration_since(instant).as_nanos()).unwrap_or(i64::MAX),
        };
        // SAFETY: `Start` on an initialized client whose event handle is set; it is called once.
        unsafe { self.client.Start() }.map_err(|e| open_error("Start", &e))?;
        Ok(Stamper::new(qpc_100ns, origin_ns, self.fmt.rate, self.latency_100ns))
    }

    fn halt(&self) {
        // SAFETY: `Stop` then `Reset` on a client that was started (or never started: both then
        // fail harmlessly); `Reset` requires the stream to be stopped, which it is.
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
    }
}

fn unsafe_frequency(clock: &IAudioClock) -> Result<u64, ab::OpenError> {
    // SAFETY: `GetFrequency` on a live clock service of an initialized client.
    unsafe { clock.GetFrequency() }.map_err(|e| open_error("GetFrequency", &e))
}

impl RenderDevice for Live {
    fn wait(&mut self, timeout_ms: u32) -> Wake {
        // The stop event is first so that it wins when both are signaled.
        let handles = [self.stop.0, self.event.0];
        // SAFETY: both handles are live events owned by this stream; `WaitForMultipleObjects`
        // with `bWaitAll` false returns the index of the first signaled handle, `WAIT_TIMEOUT`,
        // or `WAIT_FAILED`. It does not allocate.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
        match woke.0 {
            0 => Wake::Stop,
            1 => Wake::Event,
            0x102 => Wake::Timeout,
            _ => Wake::Failed(Fault::Other(-1)),
        }
    }

    fn padding(&mut self) -> Result<u32, Fault> {
        let mut padding = 0u32;
        // SAFETY: `GetCurrentPadding` on the started client with a live `u32`. The raw vtable is
        // called so that a failure is a bare `HRESULT`, never a `windows::core::Error` (which
        // captures `IErrorInfo` and can allocate) on the audio thread.
        let hr = unsafe { (Interface::vtable(&self.client).GetCurrentPadding)(Interface::as_raw(&self.client), &mut padding) };
        if hr.is_ok() { Ok(padding) } else { Err(Fault::from_hresult(hr.0)) }
    }

    fn position(&mut self) -> Result<Position, Fault> {
        let (mut device, mut qpc) = (0u64, 0u64);
        // SAFETY: `IAudioClock2::GetDevicePosition` with two live `u64`s on the started stream's
        // clock; raw vtable call for the same allocation-free reason as `padding`. The position
        // is in units of `GetFrequency()` per second and the QPC value in 100 ns units.
        let hr = unsafe { (Interface::vtable(&self.clock).GetDevicePosition)(Interface::as_raw(&self.clock), &mut device, &mut qpc) };
        if hr.is_err() {
            return Err(Fault::from_hresult(hr.0));
        }
        let frames = if self.clock_frequency == u64::from(self.fmt.rate) || self.clock_frequency == 0 {
            device
        } else {
            (u128::from(device) * u128::from(self.fmt.rate) / u128::from(self.clock_frequency)) as u64
        };
        Ok(Position { frames, qpc_100ns: qpc })
    }

    fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), Fault> {
        let mut data: *mut u8 = std::ptr::null_mut();
        // SAFETY: `GetBuffer(frames)` with `frames <= buffer size - padding` returns a pointer to
        // `frames * nBlockAlign` writable bytes that stay valid and exclusively ours until the
        // paired `ReleaseBuffer(frames)` below, called exactly once before any other `GetBuffer`.
        // Raw vtable calls keep failures as bare `HRESULT`s (no `windows::core::Error`).
        unsafe {
            let hr = (Interface::vtable(&self.render).GetBuffer)(Interface::as_raw(&self.render), frames, &mut data);
            if hr.is_err() {
                return Err(Fault::from_hresult(hr.0));
            }
            let flags = if data.is_null() {
                BUFFER_SILENT
            } else {
                let len = frames as usize * usize::from(self.fmt.channels) * self.fmt.wire.bytes();
                fill(std::slice::from_raw_parts_mut(data, len));
                0
            };
            let hr = (Interface::vtable(&self.render).ReleaseBuffer)(Interface::as_raw(&self.render), frames, flags);
            if hr.is_err() {
                return Err(Fault::from_hresult(hr.0));
            }
        }
        Ok(())
    }
}

/// The audio thread's published scheduling state, as `WorkerPriority` codes.
const PRIO_PENDING: u8 = 0;
const PRIO_PRO_AUDIO: u8 = 1;
const PRIO_RAISED: u8 = 2;
const PRIO_FAILED: u8 = 3;
const PRIO_RELEASED: u8 = 4;
const PRIO_UNOBSERVED: u8 = 5;

enum Command {
    Start(Sender<Result<(), ab::OpenError>>),
}

/// Everything the stream thread reports once it has opened the stream.
struct Opened {
    achieved: ab::Achieved,
}

/// One native render stream: a dedicated MTA thread owns every COM object and runs the loop.
pub(crate) struct RenderStream {
    stop: Arc<Event>,
    commands: Mutex<Option<Sender<Command>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    priority: Arc<AtomicU8>,
    latency: Option<Duration>,
}

impl RenderStream {
    pub(crate) fn open(params: OpenParams, render: ab::RenderCallback, error: ab::ErrorCallback) -> Result<(Self, ab::Achieved), ab::OpenError> {
        let stop = Arc::new(Event::new(true)?);
        let priority = Arc::new(AtomicU8::new(PRIO_PENDING));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<Opened, ab::OpenError>>(1);
        let (command_tx, command_rx) = mpsc::channel::<Command>();
        let thread = {
            let (stop, priority) = (Arc::clone(&stop), Arc::clone(&priority));
            std::thread::Builder::new()
                .name("transit-wasapi-render".into())
                .spawn(move || stream_thread(params, stop, priority, ready_tx, command_rx, render, error))
                .map_err(|e| ab::OpenError::Other(format!("could not start the WASAPI stream thread: {e}")))?
        };
        let opened = match ready_rx.recv_timeout(CONTROL_TIMEOUT) {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                stop.set();
                return Err(ab::OpenError::Other("opening the WASAPI stream timed out".into()));
            }
        };
        let latency = opened.achieved.latency;
        let stream = Self { stop, commands: Mutex::new(Some(command_tx)), thread: Mutex::new(Some(thread)), priority, latency };
        Ok((stream, opened.achieved))
    }
}

impl ab::Stream for RenderStream {
    fn start(&self) -> Result<(), ab::OpenError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let sent = self.commands.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|c| c.send(Command::Start(reply_tx)));
        match sent {
            Some(Ok(())) => reply_rx
                .recv_timeout(CONTROL_TIMEOUT)
                .unwrap_or_else(|_| Err(ab::OpenError::DeviceNotAvailable("the WASAPI stream thread did not answer".into()))),
            _ => Err(ab::OpenError::Other("the WASAPI stream has stopped; open it again".into())),
        }
    }

    /// Stops the stream for good: the thread stops and resets the client and exits. A stopped
    /// stream is reopened, not restarted.
    fn stop(&self) -> Result<(), ab::OpenError> {
        self.stop.set();
        Ok(())
    }

    fn latency(&self) -> Option<Duration> {
        self.latency
    }

    fn thread_priority(&self) -> ab::WorkerPriority {
        match self.priority.load(Ordering::Acquire) {
            PRIO_PENDING => ab::WorkerPriority::Pending,
            PRIO_PRO_AUDIO => ab::WorkerPriority::MmcssProAudio,
            PRIO_RAISED => ab::WorkerPriority::RaisedPriority,
            PRIO_FAILED => ab::WorkerPriority::PromotionFailed,
            PRIO_RELEASED => ab::WorkerPriority::Released,
            _ => ab::WorkerPriority::Unobserved,
        }
    }
}

impl Drop for RenderStream {
    fn drop(&mut self) {
        self.stop.set();
        self.commands.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(thread) = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = thread.join();
        }
    }
}

fn stream_thread(
    params: OpenParams,
    stop: Arc<Event>,
    priority: Arc<AtomicU8>,
    ready: mpsc::SyncSender<Result<Opened, ab::OpenError>>,
    commands: Receiver<Command>,
    mut render: ab::RenderCallback,
    mut error: ab::ErrorCallback,
) {
    // Declared first, so dropped last: COM stays initialized until every COM object is released.
    let com = match ComGuard::mta() {
        Ok(com) => com,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // MMCSS "Pro Audio" on this thread (soundcraft-rt), reverted on this thread at exit.
    let promotion = soundcraft_rt::promote_audio_thread();
    priority.store(
        match promotion {
            soundcraft_rt::Promotion::Mmcss { .. } => PRIO_PRO_AUDIO,
            soundcraft_rt::Promotion::Priority(_) => PRIO_RAISED,
            soundcraft_rt::Promotion::Denied(_) => PRIO_FAILED,
            _ => PRIO_UNOBSERVED,
        },
        Ordering::Release,
    );
    let origin = params.request.origin;
    let finish = |priority: &AtomicU8| {
        soundcraft_rt::demote(promotion);
        if promotion.is_promoted() {
            priority.store(PRIO_RELEASED, Ordering::Release);
        }
    };
    let mut live = match Live::open(&params, stop) {
        Ok(live) => live,
        Err(e) => {
            finish(&priority);
            let _ = ready.send(Err(e));
            drop(com);
            return;
        }
    };
    let achieved = ab::Achieved {
        format: live.fmt.format(),
        period: ab::Period::Frames(live.period_frames),
        latency: Some(Duration::from_nanos(live.latency_100ns.unsigned_abs().saturating_mul(100))),
        device_position: Some(0),
    };
    let _ = ready.send(Ok(Opened { achieved }));
    // Wait for `start`; a dropped control side ends the thread.
    if let Ok(Command::Start(reply)) = commands.recv() {
        match live.start(origin) {
            Ok(stamper) => {
                let _ = reply.send(Ok(()));
                let frames_ms = (u64::from(live.period_frames) * 1000 / u64::from(live.fmt.rate.max(1))) as u32;
                let config = LoopConfig {
                    buffer_frames: live.buffer_frames,
                    format: live.fmt.format(),
                    wire: live.fmt.wire,
                    stamper,
                    origin,
                    // Four periods, but never so short that a scheduling hiccup reads as a dead device.
                    timeout_ms: (frames_ms * 4).max(250),
                };
                let mut scratch = vec![0.0f32; ab::SCRATCH_FRAMES * usize::from(live.fmt.channels).max(1)];
                let exit = render_loop::run(&mut live, &config, &mut scratch, &mut render, &mut error);
                log::debug!("WASAPI render loop ended: {exit:?}");
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }
    live.halt();
    drop(live);
    finish(&priority);
    drop(com);
}

/// The notification client: callbacks arrive on COM's threads and only post events.
#[implement(IMMNotificationClient)]
struct Notifier {
    events: Sender<ab::DeviceEvent>,
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        if state != DEVICE_STATE_ACTIVE {
            // SAFETY: the callback's id is a NUL-terminated UTF-16 string valid for the call.
            let _ = self.events.send(ab::DeviceEvent::Unplugged(unsafe { id.to_string() }.unwrap_or_default()));
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        // SAFETY: as in `OnDeviceStateChanged`.
        let _ = self.events.send(ab::DeviceEvent::Unplugged(unsafe { id.to_string() }.unwrap_or_default()));
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> windows::core::Result<()> {
        if role == eConsole {
            let direction = if flow == eRender { ab::Direction::Output } else { ab::Direction::Input };
            let _ = self.events.send(ab::DeviceEvent::DefaultChanged(direction));
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// A registered `IMMNotificationClient` on its own MTA thread; dropping unregisters it (on that
/// thread, outside any handler) before the client is released.
pub(crate) struct Watcher {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub(crate) fn start(mut handler: Box<dyn FnMut(ab::DeviceEvent) + Send>) -> Result<Self, ab::OpenError> {
        let done = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), ab::OpenError>>(1);
        let flag = Arc::clone(&done);
        let thread = std::thread::Builder::new()
            .name("transit-wasapi-watch".into())
            .spawn(move || {
                let setup = (|| {
                    let com = ComGuard::mta()?;
                    let enumerator = enumerator()?;
                    let (events, inbox) = mpsc::channel();
                    let client: IMMNotificationClient = Notifier { events }.into();
                    // SAFETY: registers a live notification client on a live enumerator, on the
                    // MTA thread that owns both; it is unregistered below, on this thread, before
                    // either is released.
                    unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
                        .map_err(|e| open_error("RegisterEndpointNotificationCallback", &e))?;
                    Ok::<_, ab::OpenError>((com, enumerator, client, inbox))
                })();
                let (com, enumerator, client, inbox) = match setup {
                    Ok(parts) => parts,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while !flag.load(Ordering::Acquire) {
                    if let Ok(event) = inbox.recv_timeout(Duration::from_millis(100)) {
                        handler(event);
                    }
                }
                // SAFETY: unregisters the client registered above, on the same enumerator and
                // thread, outside any notification handler.
                let _ = unsafe { enumerator.UnregisterEndpointNotificationCallback(&client) };
                drop((client, enumerator));
                drop(com);
            })
            .map_err(|e| ab::OpenError::Other(format!("could not start the device watcher: {e}")))?;
        match ready_rx.recv_timeout(CONTROL_TIMEOUT) {
            Ok(Ok(())) => Ok(Self { done, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ab::OpenError::Other("starting the device watcher timed out".into())),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `IAudioClient2::SetClientProperties(RAW)`; must precede `Initialize` and the mix-format query.
fn apply_raw(client: &IAudioClient) {
    let Ok(client2) = client.cast::<IAudioClient2>() else { return };
    let properties = AudioClientProperties {
        cbSize: size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Media,
        Options: AUDCLNT_STREAMOPTIONS_RAW,
    };
    // SAFETY: `SetClientProperties` is called before `Initialize`, as required; `properties` is a
    // valid struct whose `cbSize` is its size, live for the call.
    if let Err(e) = unsafe { client2.SetClientProperties(&raw const properties) } {
        log::info!("WASAPI raw mode unavailable: {e}");
    }
}
