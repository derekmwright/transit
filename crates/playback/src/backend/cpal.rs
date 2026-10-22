//! The cpal adapter of the audio backend trait (ADR 0016 slice a). This module and `devices.rs`
//! are the only users of cpal types: it owns device selection (`choose_output`,
//! `output_attempts`), the sample-format fan-out and conversion, the ASIO duplex order and the
//! process-wide ASIO device cache, the mapping of cpal timestamps to stamps, the observation of
//! the device worker's scheduling, and the platform cfgs (FreeBSD has no capture).
//!
//! CPAL owns Windows device-worker promotion before the first data callback and releases the
//! registration on the same worker at exit. The pinned fork exposes its actual helper result.
//! No transit-rt promotion is called on that worker. Render-pool workers have a separate
//! policy; their Pro Audio benchmark results do not describe this MMCSS Audio registration.

use crate::{devices, telemetry};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use soundcraft_model::MAX_DEVICE_CHANNELS;
use std::sync::{Arc, Mutex, PoisonError};
use transit_audio_backend as ab;

/// Sample types the adapter converts to and from `f32`.
pub(crate) trait Sample: cpal::SizedSample + cpal::FromSample<f32> {}
impl<T: cpal::SizedSample + cpal::FromSample<f32>> Sample for T {}

/// Creates the cpal backend of the audio system named (else the default one).
pub struct CpalFactory;

impl ab::BackendFactory for CpalFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn ab::Backend>> {
        devices::host_named(system).map(|host| Box::new(CpalBackend { host }) as Box<dyn ab::Backend>)
    }
}

/// One cpal host (audio system).
pub struct CpalBackend {
    host: &'static cpal::Host,
}

/// The device reference an endpoint carries.
struct CpalDevice(Mutex<cpal::Device>);

fn device_of(endpoint: &ab::Endpoint) -> Result<cpal::Device, ab::OpenError> {
    let device = endpoint.handle::<CpalDevice>().ok_or_else(|| ab::OpenError::Other("the endpoint is not a cpal device".into()))?;
    Ok(device.0.lock().unwrap_or_else(PoisonError::into_inner).clone())
}

fn endpoint(device: cpal::Device, name: String, system: &str, inputs: u16, outputs: u16, formats: Vec<ab::FormatRange>) -> ab::Endpoint {
    ab::Endpoint::new(name.clone(), name, system.to_string(), inputs, outputs, formats, Arc::new(CpalDevice(Mutex::new(device))))
}

fn sample_of(format: cpal::SampleFormat) -> Option<ab::SampleFormat> {
    use cpal::SampleFormat as C;
    Some(match format {
        C::I8 => ab::SampleFormat::I8,
        C::I16 => ab::SampleFormat::I16,
        C::I24 => ab::SampleFormat::I24,
        C::I32 => ab::SampleFormat::I32,
        C::I64 => ab::SampleFormat::I64,
        C::U8 => ab::SampleFormat::U8,
        C::U16 => ab::SampleFormat::U16,
        C::U32 => ab::SampleFormat::U32,
        C::U64 => ab::SampleFormat::U64,
        C::F32 => ab::SampleFormat::F32,
        C::F64 => ab::SampleFormat::F64,
        _ => return None,
    })
}

fn period_of(size: cpal::BufferSize) -> ab::Period {
    match size {
        cpal::BufferSize::Fixed(n) => ab::Period::Frames(n),
        cpal::BufferSize::Default => ab::Period::Default,
    }
}

fn request(config: &cpal::StreamConfig, format: cpal::SampleFormat) -> Result<ab::OpenRequest, String> {
    let sample = sample_of(format).ok_or_else(|| format!("unsupported device sample format: {format}"))?;
    Ok(ab::OpenRequest {
        format: ab::Format::new(config.sample_rate.0, config.channels, sample),
        period: period_of(config.buffer_size),
        mode: ab::Mode::Shared,
        origin: ab::HostOrigin::default(),
    })
}

fn stream_config(request: &ab::OpenRequest) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: request.format.channels,
        sample_rate: cpal::SampleRate(request.format.rate),
        buffer_size: match request.period {
            ab::Period::Frames(n) => cpal::BufferSize::Fixed(n),
            ab::Period::Default => cpal::BufferSize::Default,
        },
    }
}

fn stream_error(error: &cpal::StreamError) -> ab::StreamError {
    if matches!(error, cpal::StreamError::DeviceNotAvailable) { ab::StreamError::DeviceNotAvailable } else { ab::StreamError::Other }
}

fn open_error(error: cpal::BuildStreamError) -> ab::OpenError {
    match error {
        cpal::BuildStreamError::DeviceNotAvailable => ab::OpenError::DeviceNotAvailable(error.to_string()),
        error => ab::OpenError::Other(error.to_string()),
    }
}

#[allow(deprecated)]
fn name_of(device: &cpal::Device) -> Option<String> {
    device.name().ok()
}

/// What an output stream is opened with: the configuration, the buffer sizes it allows, the format.
type OutputChoice = (cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat);

/// Hosts whose channel order is the driver's physical order, so opening every channel puts
/// device channel 1 on output 1 (ADR 0015). Elsewhere (ALSA, CoreAudio, others) a wide config may
/// map the first channels to something other than the front pair (PipeWire's 32-channel
/// "default", aggregate devices).
// TODO(ADR 0015): validate each remaining host's channel order, then add it here.
fn physical_channel_order(host: &str) -> bool {
    host == devices::ASIO_SYSTEM || host == "WASAPI"
}

/// The best output configuration at one of `rates` (tried in order): the best sample format first
/// (f32, else 32- or 16-bit integers), then the most channels. On ASIO and WASAPI
/// ([`physical_channel_order`]) that is the widest offered from stereo up to
/// [`MAX_DEVICE_CHANNELS`] (ADR 0015: every output opens; paths other than Main stay silent in
/// this slice). On every other host the previous rule holds: stereo up to `want_ch`, the session's
/// width. A stereo-only device gets its two channels exactly as before. WASAPI shared mode lists
/// only the system mix format's channel count, so on Windows more than that is ASIO.
fn choose_output(configs: &[cpal::SupportedStreamConfigRange], rates: &[u32], host: &str, want_ch: u16) -> Option<OutputChoice> {
    let most = if physical_channel_order(host) { MAX_DEVICE_CHANNELS } else { usize::from(want_ch.max(2)) };
    let better = |c: &cpal::SupportedStreamConfigRange, x: &OutputChoice| {
        let (rc, rx) = (devices::format_rank(c.sample_format()).unwrap_or(usize::MAX), devices::format_rank(x.2).unwrap_or(usize::MAX));
        rc < rx || (rc == rx && c.channels() > x.0.channels)
    };
    for &rate in rates {
        let mut chosen: Option<OutputChoice> = None;
        for c in configs {
            if devices::format_rank(c.sample_format()).is_some()
                && c.min_sample_rate().0 <= rate
                && c.max_sample_rate().0 >= rate
                && c.channels() >= 2
                && usize::from(c.channels()) <= most
                && chosen.as_ref().is_none_or(|x| better(c, x))
            {
                chosen = Some((c.with_sample_rate(cpal::SampleRate(rate)).config(), *c.buffer_size(), c.sample_format()));
            }
        }
        if chosen.is_some() {
            return chosen;
        }
    }
    None
}

/// The stream configurations to try, in order: the chosen one with its buffer size, then with the
/// device's default buffer; and, when the chosen width is wider than the session's (every output
/// opened, ADR 0015), the same two at `session_width` (at least 2), so a driver that refuses a
/// wide buffer set still opens as it did before.
fn output_attempts(config: &cpal::StreamConfig, session_width: u16) -> Vec<cpal::StreamConfig> {
    let narrow = session_width.max(2);
    let widths: Vec<u16> = if config.channels > narrow { vec![config.channels, narrow] } else { vec![config.channels] };
    let mut attempts = Vec::new();
    for channels in widths {
        attempts.push(cpal::StreamConfig { channels, ..config.clone() });
        if config.buffer_size != cpal::BufferSize::Default {
            attempts.push(cpal::StreamConfig { channels, buffer_size: cpal::BufferSize::Default, ..config.clone() });
        }
    }
    attempts
}

/// Whether an input config can be opened: some drivers report a default input with no channels,
/// and building a stream with 0 channels reaches `unreachable!` in asio-sys.
fn input_usable(config: &cpal::SupportedStreamConfig) -> bool {
    config.channels() > 0
}

/// The input config at `rate` when the device offers it (so input and output run together: the
/// best format first, then the most channels), else the device default, in a format we read.
#[cfg(not(target_os = "freebsd"))]
fn input_config(device: &cpal::Device, rate: Option<u32>) -> Result<cpal::SupportedStreamConfig, String> {
    let at_rate = rate.and_then(|r| {
        device
            .supported_input_configs()
            .ok()?
            .filter(|c| {
                c.channels() > 0
                    && usize::from(c.channels()) <= MAX_DEVICE_CHANNELS
                    && devices::format_rank(c.sample_format()).is_some()
                    && c.min_sample_rate().0 <= r
                    && c.max_sample_rate().0 >= r
            })
            .min_by_key(|c| (devices::format_rank(c.sample_format()), std::cmp::Reverse(c.channels())))
            .map(|c| c.with_sample_rate(cpal::SampleRate(r)))
    });
    let cfg = match at_rate {
        Some(c) => c,
        None => device.default_input_config().map_err(|e| e.to_string())?,
    };
    if !input_usable(&cfg) {
        return Err("the audio input has no channels".to_string());
    }
    let format = cfg.sample_format();
    if devices::format_rank(format).is_none() {
        return Err(format!("unsupported input sample format {format:?}"));
    }
    Ok(cfg)
}

#[cfg(target_os = "freebsd")]
const NO_CAPTURE: &str = "recording is not supported on this platform yet";

fn ranges(configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>) -> Vec<ab::FormatRange> {
    configs
        .filter_map(|c| {
            let period = match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => Some((*min, *max)),
                cpal::SupportedBufferSize::Unknown => None,
            };
            Some(ab::FormatRange {
                sample: sample_of(c.sample_format())?,
                channels: c.channels(),
                min_rate: c.min_sample_rate().0,
                max_rate: c.max_sample_rate().0,
                period,
            })
        })
        .collect()
}

impl ab::Backend for CpalBackend {
    fn system(&self) -> &str {
        self.host.id().name()
    }

    fn endpoints(&self, direction: ab::Direction) -> Result<Vec<ab::Endpoint>, ab::OpenError> {
        let output = direction == ab::Direction::Output;
        let devices = if output { self.host.output_devices() } else { self.host.input_devices() }.map_err(|e| ab::OpenError::Other(e.to_string()))?;
        Ok(devices
            .filter_map(|device| {
                let name = name_of(&device)?;
                let formats = if output { ranges(device.supported_output_configs().ok()?) } else { ranges(device.supported_input_configs().ok()?) };
                let channels = formats.iter().map(|f| f.channels).max().unwrap_or(0);
                Some(endpoint(device, name, self.system(), if output { 0 } else { channels }, if output { channels } else { 0 }, formats))
            })
            .collect())
    }

    fn plan_render(&self, wish: &ab::OutputWish) -> Result<ab::Plan, String> {
        let device = device_on(self.host, wish.device.as_deref(), true).ok_or_else(|| "no output device".to_string())?;
        let name = name_of(&device).unwrap_or_else(|| "Audio device".into());
        let chosen = device
            .supported_output_configs()
            .ok()
            .and_then(|configs| choose_output(&configs.collect::<Vec<_>>(), &wish.rates, self.host.id().name(), wish.channels));
        let (config, format) = match chosen {
            Some((mut c, buffers, format)) => {
                c.buffer_size = devices::buffer_size(wish.period, &buffers);
                (c, format)
            }
            None => {
                let d = device.default_output_config().map_err(|e| e.to_string())?;
                if devices::format_rank(d.sample_format()).is_none() {
                    return Err(format!("unsupported device sample format {:?}", d.sample_format()));
                }
                let buffers = *d.buffer_size();
                let format = d.sample_format();
                let mut c = d.config();
                c.buffer_size = devices::buffer_size(wish.period, &buffers);
                (c, format)
            }
        };
        let attempts = output_attempts(&config, wish.channels).iter().map(|c| request(c, format)).collect::<Result<Vec<_>, _>>()?;
        Ok(ab::Plan { endpoint: endpoint(device, name, self.system(), 0, config.channels, Vec::new()), attempts })
    }

    #[cfg(not(target_os = "freebsd"))]
    fn plan_capture(&self, wish: &ab::InputWish) -> Result<ab::Plan, String> {
        let device = device_on(self.host, wish.device.as_deref(), false).ok_or("no audio input device")?;
        let cfg = input_config(&device, wish.rate)?;
        let buffer = devices::buffer_size(wish.period, cfg.buffer_size());
        // The chosen buffer size first; if the device refuses it, its default.
        let mut sizes = vec![buffer];
        if buffer != cpal::BufferSize::Default {
            sizes.push(cpal::BufferSize::Default);
        }
        let attempts = sizes
            .into_iter()
            .map(|buffer_size| request(&cpal::StreamConfig { buffer_size, ..cfg.config() }, cfg.sample_format()))
            .collect::<Result<_, _>>()?;
        let name = name_of(&device).unwrap_or_else(|| "Input".into());
        Ok(ab::Plan { endpoint: endpoint(device, name, self.system(), cfg.channels(), 0, Vec::new()), attempts })
    }

    #[cfg(target_os = "freebsd")]
    fn plan_capture(&self, _wish: &ab::InputWish) -> Result<ab::Plan, String> {
        Err(NO_CAPTURE.into())
    }

    /// An ASIO driver has one set of buffers for both directions, and cpal 0.16 recreates them
    /// (stopping the driver) whenever a direction is added; doing that while the output's
    /// callbacks run races cpal's own stream lock and can hang. Opened first, the input is in
    /// place when the output's build creates both directions' buffers at once, and arming never
    /// touches the streams. The input is the output's own driver (one ASIO driver per process),
    /// whatever input the configuration names. Other audio systems open the input on demand, as
    /// its own device.
    fn capture_with_render(&self, render: &ab::Endpoint) -> bool {
        self.system() == devices::ASIO_SYSTEM && device_of(render).is_ok_and(|device| device.default_input_config().is_ok_and(|c| input_usable(&c)))
    }

    #[cfg(not(target_os = "freebsd"))]
    fn capture_format(&self, endpoint: &ab::Endpoint, rate: Option<u32>) -> Result<ab::Format, String> {
        let device = device_of(endpoint).map_err(|e| e.to_string())?;
        let cfg = input_config(&device, rate)?;
        let sample = sample_of(cfg.sample_format()).ok_or("unsupported input sample format")?;
        Ok(ab::Format::new(cfg.sample_rate().0, cfg.channels(), sample))
    }

    #[cfg(target_os = "freebsd")]
    fn capture_format(&self, _endpoint: &ab::Endpoint, _rate: Option<u32>) -> Result<ab::Format, String> {
        Err(NO_CAPTURE.into())
    }

    fn open_render(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        render: ab::RenderCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        use ab::SampleFormat as S;
        let device = device_of(endpoint)?;
        let config = stream_config(request);
        let built = match request.format.sample {
            S::I8 => build_render::<i8>(&device, &config, request, render, error),
            S::I16 => build_render::<i16>(&device, &config, request, render, error),
            S::I24 => build_render::<cpal::I24>(&device, &config, request, render, error),
            S::I32 => build_render::<i32>(&device, &config, request, render, error),
            S::I64 => build_render::<i64>(&device, &config, request, render, error),
            S::U8 => build_render::<u8>(&device, &config, request, render, error),
            S::U16 => build_render::<u16>(&device, &config, request, render, error),
            S::U32 => build_render::<u32>(&device, &config, request, render, error),
            S::U64 => build_render::<u64>(&device, &config, request, render, error),
            S::F32 => build_render::<f32>(&device, &config, request, render, error),
            S::F64 => build_render::<f64>(&device, &config, request, render, error),
            _ => return Err(ab::OpenError::Other(format!("unsupported device sample format: {:?}", request.format.sample))),
        };
        let stream = built.map_err(open_error)?;
        Ok((Box::new(CpalStream(stream)), ab::Achieved::requested(request.format, request.period)))
    }

    #[cfg(not(target_os = "freebsd"))]
    fn open_capture(
        &self,
        endpoint: &ab::Endpoint,
        request: &ab::OpenRequest,
        capture: ab::CaptureCallback,
        error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        use ab::SampleFormat as S;
        let device = device_of(endpoint)?;
        let config = stream_config(request);
        let built = match request.format.sample {
            S::I32 => build_capture::<i32>(&device, &config, request, capture, error),
            S::I16 => build_capture::<i16>(&device, &config, request, capture, error),
            _ => build_capture::<f32>(&device, &config, request, capture, error),
        };
        let stream = built.map_err(open_error)?;
        Ok((Box::new(CpalStream(stream)), ab::Achieved::requested(request.format, request.period)))
    }

    #[cfg(target_os = "freebsd")]
    fn open_capture(
        &self,
        _endpoint: &ab::Endpoint,
        _request: &ab::OpenRequest,
        _capture: ab::CaptureCallback,
        _error: ab::ErrorCallback,
    ) -> Result<(Box<dyn ab::Stream>, ab::Achieved), ab::OpenError> {
        Err(ab::OpenError::Other(NO_CAPTURE.into()))
    }
}

struct CpalStream(cpal::Stream);

impl ab::Stream for CpalStream {
    fn start(&self) -> Result<(), ab::OpenError> {
        self.0.play().map_err(|e| match e {
            cpal::PlayStreamError::DeviceNotAvailable => ab::OpenError::DeviceNotAvailable(e.to_string()),
            e => ab::OpenError::Other(e.to_string()),
        })
    }
    fn stop(&self) -> Result<(), ab::OpenError> {
        self.0.pause().map_err(|e| ab::OpenError::Other(e.to_string()))
    }
    fn latency(&self) -> Option<std::time::Duration> {
        None
    }
    fn thread_priority(&self) -> ab::WorkerPriority {
        observe(&self.0)
    }
}

/// Callback-local storage, prepared before starting the stream. Keeping the conversion here lets
/// tests exercise the same chunking and conversion as the real device.
pub(crate) struct OutputCallback {
    scratch: Vec<f32>,
}

impl OutputCallback {
    pub(crate) fn new(channels: usize) -> Self {
        // Initialise the platform clock (including any frequency cache) before callbacks run.
        let _ = telemetry::Timer::start().elapsed();
        Self { scratch: vec![0.0; ab::SCRATCH_FRAMES * channels.max(1)] }
    }

    /// Render the device block chunk by chunk through the scratch and convert each chunk.
    pub(crate) fn convert<T: Sample>(&mut self, data: &mut [T], render: &mut dyn FnMut(&mut [f32])) {
        for out in data.chunks_mut(self.scratch.len()) {
            let Some(s) = self.scratch.get_mut(..out.len()) else { break };
            render(s);
            for (o, x) in out.iter_mut().zip(s.iter()) {
                *o = T::from_sample(*x);
            }
        }
    }

    /// One device callback scope, shared by all scratch conversion chunks; it must never be
    /// reconstructed inside a chunk.
    #[cfg(test)]
    pub(crate) fn fill_with<T: Sample>(
        &mut self,
        data: &mut [T],
        mut render: impl FnMut(&mut [f32], &mut crate::plan_bridge::CallbackBudget, &mut crate::action_lane::Budget),
    ) {
        let mut budget = crate::plan_bridge::callback_scope();
        let mut actions = crate::action_lane::callback_scope();
        self.convert(data, &mut |s| render(s, &mut budget, &mut actions));
    }
}

/// The render sink of one device callback.
pub(crate) struct ConvertSink<'a, T> {
    pub(crate) convert: &'a mut OutputCallback,
    pub(crate) data: &'a mut [T],
}

impl<T: Sample> ab::RenderSink for ConvertSink<'_, T> {
    fn render(&mut self, fill: &mut dyn FnMut(&mut [f32])) {
        self.convert.convert(self.data, fill);
    }
}

/// The capture source of one device callback: converts through `scratch` chunk by chunk.
pub(crate) struct ConvertSource<'a, T> {
    pub(crate) scratch: &'a mut [f32],
    pub(crate) channels: usize,
    pub(crate) data: &'a [T],
}

impl<T: cpal::SizedSample> ab::CaptureSource for ConvertSource<'_, T>
where
    f32: cpal::FromSample<T>,
{
    fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32])) {
        use cpal::Sample as _;
        let chunk = self.scratch.len();
        for (index, part) in self.data.chunks(chunk).enumerate() {
            let Some(s) = self.scratch.get_mut(..part.len()) else { return };
            for (o, x) in s.iter_mut().zip(part) {
                *o = f32::from_sample(*x);
            }
            each(index * (chunk / self.channels), s);
        }
    }
}

fn build_render<T: Sample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    request: &ab::OpenRequest,
    mut render: ab::RenderCallback,
    mut error: ab::ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let channels = usize::from(config.channels).max(1);
    let (format, origin) = (request.format, request.origin);
    let mut convert = OutputCallback::new(channels);
    let mut stamper = Stamper::default();
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            let stamp = stamper.output(frames, info, origin.now_ns());
            let mut sink = ConvertSink { convert: &mut convert, data };
            render(&mut ab::RenderBlock::new(frames, stamp, format, &mut sink));
        },
        move |e| error(stream_error(&e)),
        None,
    )
}

#[cfg(not(target_os = "freebsd"))]
fn build_capture<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    request: &ab::OpenRequest,
    mut capture: ab::CaptureCallback,
    mut error: ab::ErrorCallback,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let (format, origin) = (request.format, request.origin);
    // The scratch is allocated here, so the callback never allocates.
    let mut scratch = vec![0.0f32; ab::SCRATCH_FRAMES * channels];
    let mut stamper = Stamper::default();
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let frames = data.len() / channels;
            let stamp = stamper.input(frames, info, origin.now_ns());
            let mut source = ConvertSource { scratch: &mut scratch, channels, data };
            capture(&mut ab::CaptureBlock::new(frames, stamp, format, &mut source));
        },
        move |e| {
            log::warn!("input stream error: {e}");
            error(stream_error(&e));
        },
        None,
    )
}

/// The mapping from cpal timestamps to stamps: signed offsets from the first callback's instant.
/// The same extraction serves real callbacks and the fake-device tests.
#[derive(Default)]
pub(crate) struct Stamper {
    pub(crate) backend_origin: Option<cpal::StreamInstant>,
}

pub(crate) fn relative_ns(instant: cpal::StreamInstant, origin: cpal::StreamInstant) -> Option<i64> {
    if let Some(elapsed) = instant.duration_since(&origin) {
        i64::try_from(elapsed.as_nanos()).ok()
    } else {
        let elapsed = origin.duration_since(&instant)?;
        i64::try_from(-i128::try_from(elapsed.as_nanos()).ok()?).ok()
    }
}

impl Stamper {
    pub(crate) fn input(&mut self, frames: usize, info: &cpal::InputCallbackInfo, host_ns: Option<u64>) -> ab::Stamp {
        let time = info.timestamp();
        self.stamp(frames, time.callback, time.capture, host_ns)
    }
    pub(crate) fn output(&mut self, frames: usize, info: &cpal::OutputCallbackInfo, host_ns: Option<u64>) -> ab::Stamp {
        let time = info.timestamp();
        self.stamp(frames, time.callback, time.playback, host_ns)
    }
    fn stamp(&mut self, frames: usize, callback: cpal::StreamInstant, frame: cpal::StreamInstant, host_ns: Option<u64>) -> ab::Stamp {
        if frames == 0 {
            return ab::Stamp::INVALID;
        }
        let origin = *self.backend_origin.get_or_insert(callback);
        let callback = relative_ns(callback, origin);
        let frame = relative_ns(frame, origin);
        let valid = callback.is_some() && frame.is_some() && host_ns.is_some();
        ab::Stamp {
            host_callback_ns: host_ns.unwrap_or(0),
            backend_callback_ns: callback.unwrap_or(0),
            backend_frame_ns: frame.unwrap_or(0),
            // Pinned CPAL does not expose the validity flags or calibrated ADC/DAC offset.
            quality: if valid { ab::Quality::BackendEstimate } else { ab::Quality::Invalid },
            offset_uncertainty_ns: None,
        }
    }
}

fn observe(stream: &cpal::Stream) -> ab::WorkerPriority {
    #[cfg(target_os = "windows")]
    {
        use cpal::platform::StreamInner;
        match stream.as_inner() {
            StreamInner::Wasapi(stream) => observed_wasapi(stream.thread_priority_observation().status()),
            #[cfg(feature = "asio")]
            StreamInner::Asio(_) => ab::WorkerPriority::Unobserved,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = stream;
        ab::WorkerPriority::Unobserved
    }
}

#[cfg(target_os = "windows")]
fn observed_wasapi(status: cpal::platform::WasapiThreadPriorityStatus) -> ab::WorkerPriority {
    use cpal::platform::WasapiThreadPriorityStatus as Status;
    match status {
        Status::Unobserved => ab::WorkerPriority::Unobserved,
        Status::Pending => ab::WorkerPriority::Pending,
        Status::Promoted => ab::WorkerPriority::MmcssAudio,
        Status::PromotionFailed => ab::WorkerPriority::PromotionFailed,
        Status::Released => ab::WorkerPriority::Released,
        Status::ReleaseFailed => ab::WorkerPriority::ReleaseFailed,
        // The fork marks its API non-exhaustive. Unknown future states are not success.
        _ => ab::WorkerPriority::Unobserved,
    }
}

/// The device's ASIO buffers are shared by both directions: the output's `cpal::Device` is kept
/// here so the input opens from the same value.
static ASIO_OUTPUT: Mutex<Option<cpal::Device>> = Mutex::new(None);

/// Release the buffer-sharing device after closing the old input and output, before opening
/// their replacements. Streams retain their own driver references; this only removes the cache's
/// ownership. Otherwise the process-wide ASIO loader cannot load a different driver.
pub fn release_asio_device_cache() {
    *ASIO_OUTPUT.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// The named output (or input) device of `host`, else its default device.
///
/// On ASIO the input of the open driver is taken from the output's own `cpal::Device`. cpal keeps
/// the driver's buffers per `Device` value, and the SDK makes one set of buffers for the whole
/// driver: an input opened from a freshly listed `Device` did not know about the running output,
/// so it remade the buffers for the input alone and the output went silent (arming a track
/// silenced the SSL 2+). From the same `Device`, cpal makes the buffers for both.
pub(crate) fn device_on(host: &cpal::Host, name: Option<&str>, output: bool) -> Option<cpal::Device> {
    let mut open = ASIO_OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    devices::cached_device(&mut open, host.id().name() == devices::ASIO_SYSTEM, name, output, name_of, || devices::find_device(host, name, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfigRange};

    fn range(channels: u16, format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            cpal::SampleRate(44_100),
            cpal::SampleRate(96_000),
            SupportedBufferSize::Range { min: 64, max: 1024 },
            format,
        )
    }

    #[test]
    fn choose_output_prefers_float_then_width_within_the_hosts_rule() {
        let configs = [range(2, SampleFormat::I16), range(2, SampleFormat::F32), range(10, SampleFormat::I32), range(8, SampleFormat::F32)];
        let pick = |host, want| choose_output(&configs, &[48_000], host, want).map(|(c, _, f)| (c.channels, f));
        assert_eq!(pick("ALSA", 2), Some((2, SampleFormat::F32)), "other hosts stop at the session width");
        assert_eq!(pick("ALSA", 8), Some((8, SampleFormat::F32)));
        assert_eq!(pick("ASIO", 2), Some((8, SampleFormat::F32)), "physical-order hosts open every channel, best format first");
        assert_eq!(pick("WASAPI", 2), Some((8, SampleFormat::F32)));
        assert_eq!(choose_output(&configs, &[192_000], "ASIO", 2).map(|c| c.0.channels), None);
        assert_eq!(choose_output(&configs, &[192_000, 48_000], "ALSA", 2).map(|c| c.0.sample_rate.0), Some(48_000), "rates are tried in order");
        assert!(choose_output(&[range(2, SampleFormat::U8)], &[48_000], "ASIO", 2).is_none(), "unsupported formats are skipped");
    }

    #[test]
    fn output_attempts_retry_narrower_and_with_the_default_buffer() {
        let wide = cpal::StreamConfig { channels: 10, sample_rate: cpal::SampleRate(48_000), buffer_size: cpal::BufferSize::Fixed(128) };
        let seen: Vec<_> = output_attempts(&wide, 2).iter().map(|c| (c.channels, c.buffer_size)).collect();
        let (fixed, default) = (cpal::BufferSize::Fixed(128), cpal::BufferSize::Default);
        assert_eq!(seen, [(10, fixed), (10, default), (2, fixed), (2, default)]);
        let stereo = cpal::StreamConfig { channels: 2, buffer_size: cpal::BufferSize::Default, ..wide };
        assert_eq!(output_attempts(&stereo, 2).len(), 1);
    }

    /// A device whose input reports no channels is not an input (asio-sys would hit `unreachable!`).
    #[test]
    fn an_input_with_no_channels_is_unusable() {
        let cfg = |ch| cpal::SupportedStreamConfig::new(ch, cpal::SampleRate(48_000), cpal::SupportedBufferSize::Unknown, SampleFormat::F32);
        assert!(!input_usable(&cfg(0)));
        assert!(input_usable(&cfg(2)));
    }

    #[test]
    fn capture_converts_integer_devices_to_f32_in_chunks_with_frame_offsets() {
        use ab::CaptureSource as _;
        let data: Vec<i16> = (0..(ab::SCRATCH_FRAMES * 2 + 6) as i32).map(|i| (i * 7 - 9_000) as i16).collect();
        let mut scratch = vec![0.0f32; ab::SCRATCH_FRAMES * 2];
        let mut source = ConvertSource { scratch: &mut scratch, channels: 2, data: &data };
        let mut seen = Vec::new();
        source.chunks(&mut |offset, samples| seen.push((offset, samples.to_vec())));
        assert_eq!(seen.iter().map(|(o, s)| (*o, s.len())).collect::<Vec<_>>(), [(0, ab::SCRATCH_FRAMES * 2), (ab::SCRATCH_FRAMES, 6)]);
        let want: Vec<f32> = data.iter().map(|s| cpal::Sample::from_sample(*s)).collect();
        assert_eq!(seen.into_iter().flat_map(|(_, s)| s).collect::<Vec<_>>(), want);
    }

    #[test]
    fn conversion_chunks_a_long_block_through_one_scratch() {
        let mut callback = OutputCallback::new(2);
        let mut data = vec![0i16; 20_000 * 2];
        let mut chunks = Vec::new();
        callback.convert(&mut data, &mut |s| {
            chunks.push(s.len());
            s.fill(0.5);
        });
        assert_eq!(chunks, [16_384, 16_384, 7_232]);
        let half: i16 = cpal::Sample::from_sample(0.5f32);
        assert!(data.iter().all(|s| *s == half));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn only_observed_promotion_reports_mmcss_audio() {
        use cpal::platform::WasapiThreadPriorityStatus as Status;
        let states = [
            (Status::Unobserved, ab::WorkerPriority::Unobserved),
            (Status::Pending, ab::WorkerPriority::Pending),
            (Status::Promoted, ab::WorkerPriority::MmcssAudio),
            (Status::PromotionFailed, ab::WorkerPriority::PromotionFailed),
            (Status::Released, ab::WorkerPriority::Released),
            (Status::ReleaseFailed, ab::WorkerPriority::ReleaseFailed),
        ];
        for (status, expected) in states {
            let actual = observed_wasapi(status);
            assert_eq!(actual, expected, "fork status: {status:?}");
            assert_eq!(actual == ab::WorkerPriority::MmcssAudio, status == Status::Promoted);
        }
    }
}
