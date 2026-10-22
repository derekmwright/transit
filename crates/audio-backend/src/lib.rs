//! The audio backend trait (ADR 0016): what the Player needs from an audio system, and nothing
//! of any one system's types. A [`Backend`] lists endpoints, plans and opens render and capture
//! streams, and delivers device blocks to separate render and capture callbacks. Every block
//! carries a [`Stamp`] in signed, origin-relative nanoseconds (the clock service's input).
//!
//! The crate has no dependencies and no `unsafe`. Adapters (cpal today) live elsewhere; the
//! [`fake`] backend drives tests with scripted blocks, stamps and failures.
//!
//! Callbacks run on the audio thread of the backend: they never allocate, lock or panic.
//! Opening, planning, starting and stopping are control-thread calls.

pub mod fake;
mod time;

pub use time::{Calibration, HostOrigin, Quality, Stamp};

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

/// Frames of conversion scratch an adapter allocates per channel before a stream starts. A
/// device block larger than this reaches the callback in several chunks of one callback scope.
pub const SCRATCH_FRAMES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

/// The sample format of a device buffer. Callbacks see `f32` regardless; the adapter converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    I8,
    I16,
    I24,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

/// Buffer layout of a device block. Every backend delivers interleaved frames today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    Interleaved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub sample: SampleFormat,
    /// Significant bits when fewer than the container holds (24 valid bits in a 32-bit
    /// container); `None` means the whole container.
    pub valid_bits: Option<u8>,
    /// The speaker mask of the channels where the system reports one (WASAPI `dwChannelMask`);
    /// `None` means unspecified and 0 means direct output (channel i is device channel i).
    pub channel_mask: Option<u32>,
}

impl Format {
    /// A format with whole-container samples and no channel mask.
    pub const fn new(rate: u32, channels: u16, sample: SampleFormat) -> Self {
        Self { rate, channels, sample, valid_bits: None, channel_mask: None }
    }
}

/// The device buffer size in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Default,
    Frames(u32),
}

impl Period {
    /// The frames when fixed.
    pub fn frames(self) -> Option<u32> {
        match self {
            Period::Frames(n) => Some(n),
            Period::Default => None,
        }
    }
}

/// How the device is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mode {
    /// Through the system mixer (or the driver's own sharing): other apps keep playing.
    Shared,
    /// The device is held by this process: other apps are silenced while the stream is open. Any
    /// format the device accepts in this mode ([`Backend::probe_exclusive`]); the period is the
    /// driver's (its default unless one is requested; [`Achieved::min_period`] reports its minimum).
    Exclusive,
}

/// The standard sample rates, in the order exclusive formats are probed after the wished rate.
pub const STANDARD_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// The exclusive-mode formats to probe, in preference order: for each rate (`rate_first`, then
/// [`STANDARD_RATES`], none twice), for each channel count in `channels` (none twice), the sample
/// formats F32, I32 with 24 valid bits, I32, packed I24, I16. A stereo format leaves the channel
/// mask to the backend (`None`, the front pair); any other count asks for direct output (`Some(0)`).
pub fn exclusive_candidates(rate_first: u32, channels: &[u16]) -> Vec<Format> {
    let mut rates = vec![rate_first];
    rates.extend(STANDARD_RATES.iter().copied().filter(|r| *r != rate_first));
    let mut counts: Vec<u16> = Vec::with_capacity(channels.len());
    for &c in channels {
        if !counts.contains(&c) {
            counts.push(c);
        }
    }
    let samples: [(SampleFormat, Option<u8>); 5] =
        [(SampleFormat::F32, None), (SampleFormat::I32, Some(24)), (SampleFormat::I32, None), (SampleFormat::I24, None), (SampleFormat::I16, None)];
    let mut out = Vec::with_capacity(rates.len() * counts.len() * samples.len());
    for &rate in &rates {
        for &channels in &counts {
            let channel_mask = if channels == 2 { None } else { Some(0) };
            for (sample, valid_bits) in samples {
                out.push(Format { rate, channels, sample, valid_bits, channel_mask });
            }
        }
    }
    out
}

/// A request to open one stream. `origin` is the process origin that stamps are relative to.
#[derive(Debug, Clone, Copy)]
pub struct OpenRequest {
    pub format: Format,
    pub period: Period,
    pub mode: Mode,
    pub origin: HostOrigin,
    /// The loopback calibration of this device pair, when the user has measured one. Only the
    /// native WASAPI backend applies it.
    pub calibration: Option<Calibration>,
}

/// What a stream opened with. An adapter may not be able to honor the request exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Achieved {
    pub format: Format,
    /// The real period when the system reports it, else what was requested.
    pub period: Period,
    /// The stream latency the system reports, where it says.
    pub latency: Option<Duration>,
    /// The device frame position at open, where the system reports one.
    pub device_position: Option<u64>,
    /// The smallest period the device reports for this mode, in frames, where it says (WASAPI
    /// exclusive: `GetDevicePeriod`'s minimum). A driver may not sustain it; it is offered, not used.
    pub min_period: Option<u32>,
}

impl Achieved {
    /// The request honored as asked, with nothing more reported.
    pub const fn requested(format: Format, period: Period) -> Self {
        Self { format, period, latency: None, device_position: None, min_period: None }
    }
}

/// A range of formats an endpoint offers: any rate in `min_rate..=max_rate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRange {
    pub sample: SampleFormat,
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    /// The device buffer range in frames, when it says.
    pub period: Option<(u32, u32)>,
}

/// An opaque adapter-owned device reference.
pub type Handle = Arc<dyn Any + Send + Sync>;

/// One device of an audio system. The `handle` is the adapter's own reference to the device: the
/// Player passes the endpoint back to open a stream on exactly it, as ASIO duplex requires.
#[derive(Clone)]
pub struct Endpoint {
    /// A stable identifier within its system.
    pub id: String,
    pub name: String,
    /// The audio system's name ("WASAPI", "ASIO", ...).
    pub system: String,
    pub inputs: u16,
    pub outputs: u16,
    /// Candidate formats where the system lists them.
    pub formats: Vec<FormatRange>,
    handle: Handle,
}

impl Endpoint {
    pub fn new(id: String, name: String, system: String, inputs: u16, outputs: u16, formats: Vec<FormatRange>, handle: Handle) -> Self {
        Self { id, name, system, inputs, outputs, formats, handle }
    }
    /// The device reference of the adapter, when `T` is its type.
    pub fn handle<T: Any>(&self) -> Option<&T> {
        self.handle.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("id", &self.id).field("name", &self.name).field("system", &self.system).finish_non_exhaustive()
    }
}

/// What the user asked of the output.
#[derive(Debug, Clone, Default)]
pub struct OutputWish {
    /// The device name; `None` or an unavailable one means the system default.
    pub device: Option<String>,
    /// Rates to try in order (the chosen rate, then the session rate).
    pub rates: Vec<u32>,
    /// The channel count of the session.
    pub channels: u16,
    pub period: Option<u32>,
}

/// What the user asked of the input.
#[derive(Debug, Clone, Default)]
pub struct InputWish {
    pub device: Option<String>,
    pub rate: Option<u32>,
    pub period: Option<u32>,
}

/// The endpoint to open and the requests to try on it, in order.
#[derive(Debug, Clone)]
pub struct Plan {
    pub endpoint: Endpoint,
    pub attempts: Vec<OpenRequest>,
}

/// Why a stream did not open (or start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The device is gone or unavailable. The text comes from the audio system.
    DeviceNotAvailable(String),
    /// Another client holds the device exclusively (`AUDCLNT_E_DEVICE_IN_USE`).
    InUse(String),
    /// The endpoint's settings forbid exclusive use (`AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED`: the
    /// "Allow applications to take exclusive control" box in its Advanced properties is cleared).
    ExclusiveDisallowed(String),
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::DeviceNotAvailable(text) | OpenError::InUse(text) | OpenError::ExclusiveDisallowed(text) | OpenError::Other(text) => {
                f.write_str(text)
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// A fault on a running stream. `Copy` and text-free: it is reported from the audio thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    DeviceNotAvailable,
    Other,
}

/// Device changes a backend reports outside any stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    Unplugged(String),
    DefaultChanged(Direction),
}

/// Scheduling state of the device worker of a stream (observed, not requested).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerPriority {
    /// There is no device worker (for example the silent transport clock).
    NoWorker,
    /// This backend exposes no observed result. This is neither success nor failure.
    /// Includes ASIO's driver-owned callback and the current non-Windows backends.
    Unobserved,
    /// The WASAPI worker has not yet completed its startup registration attempt.
    Pending,
    /// Successful MMCSS Audio registration on this worker, before data callbacks.
    /// Does not imply Pro Audio priority, glitch-free playback or a hardware deadline margin.
    MmcssAudio,
    /// Successful MMCSS "Pro Audio" registration on this worker, before data callbacks.
    MmcssProAudio,
    /// MMCSS was unavailable and the worker runs at a raised thread priority instead.
    RaisedPriority,
    /// The worker tried registration and the helper reported failure; playback may continue.
    PromotionFailed,
    /// The worker explicitly released its successful registration at exit.
    Released,
    /// Explicit release was attempted but the helper reported failure.
    ReleaseFailed,
}

/// A running (or startable) stream. Dropping it closes the device.
pub trait Stream {
    fn start(&self) -> Result<(), OpenError>;
    fn stop(&self) -> Result<(), OpenError>;
    /// The reported latency of the stream, where the backend can say.
    fn latency(&self) -> Option<Duration>;
    /// The observed scheduling state of the device worker (the result of the MMCSS or priority hook).
    fn thread_priority(&self) -> WorkerPriority;
}

/// Where the samples of a render block go: the adapter lends scratch, converts to the device
/// format after `fill` and splits a block larger than [`SCRATCH_FRAMES`] into chunks.
pub trait RenderSink {
    /// Call `fill` with each `f32` interleaved chunk to produce, in order.
    fn render(&mut self, fill: &mut dyn FnMut(&mut [f32]));
}

/// Where the samples of a capture block come from, converted to `f32`.
pub trait CaptureSource {
    /// Call `each(frame_offset, samples)` for each interleaved chunk, in order.
    fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32]));
}

/// One device callback of a render stream.
pub struct RenderBlock<'a> {
    pub frames: usize,
    /// Time of this block; signed and relative to the origin of the request.
    pub stamp: Stamp,
    /// The device frame position, where the backend reports one.
    pub device_position: Option<u64>,
    pub sample: SampleFormat,
    pub channels: u16,
    pub layout: Layout,
    sink: &'a mut dyn RenderSink,
}

impl<'a> RenderBlock<'a> {
    pub fn new(frames: usize, stamp: Stamp, format: Format, sink: &'a mut dyn RenderSink) -> Self {
        Self { frames, stamp, device_position: None, sample: format.sample, channels: format.channels, layout: Layout::Interleaved, sink }
    }
    /// Produce the `f32` samples of the block chunk by chunk.
    pub fn render(&mut self, fill: &mut dyn FnMut(&mut [f32])) {
        self.sink.render(fill);
    }
}

/// One device callback of a capture stream.
pub struct CaptureBlock<'a> {
    pub frames: usize,
    pub stamp: Stamp,
    pub device_position: Option<u64>,
    pub sample: SampleFormat,
    pub channels: u16,
    pub layout: Layout,
    source: &'a mut dyn CaptureSource,
}

impl<'a> CaptureBlock<'a> {
    pub fn new(frames: usize, stamp: Stamp, format: Format, source: &'a mut dyn CaptureSource) -> Self {
        Self { frames, stamp, device_position: None, sample: format.sample, channels: format.channels, layout: Layout::Interleaved, source }
    }
    /// Read the samples of the block as `f32`, chunk by chunk; `each` gets the frame offset of the chunk.
    pub fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32])) {
        self.source.chunks(each);
    }
}

pub type RenderCallback = Box<dyn FnMut(&mut RenderBlock<'_>) + Send + 'static>;
pub type CaptureCallback = Box<dyn FnMut(&mut CaptureBlock<'_>) + Send + 'static>;
pub type ErrorCallback = Box<dyn FnMut(StreamError) + Send + 'static>;

/// One audio system.
pub trait Backend {
    /// The name of the audio system ("WASAPI", "ASIO", ...).
    fn system(&self) -> &str;
    /// The devices of the system for one direction (can take a moment: call on a control thread).
    fn endpoints(&self, direction: Direction) -> Result<Vec<Endpoint>, OpenError>;
    /// Choose the output endpoint and the formats to try ([`OutputWish`]).
    fn plan_render(&self, wish: &OutputWish) -> Result<Plan, String>;
    /// Choose the input endpoint and the formats to try ([`InputWish`]).
    fn plan_capture(&self, wish: &InputWish) -> Result<Plan, String>;
    /// Whether the input must open together with `render` on its own device, before either runs
    /// (ASIO: one driver, one set of buffers for both directions).
    fn capture_with_render(&self, render: &Endpoint) -> bool;
    /// The input format `endpoint` offers at `rate` (else its default), for the I/O map and a
    /// duplex open.
    fn capture_format(&self, endpoint: &Endpoint, rate: Option<u32>) -> Result<Format, String>;
    /// Open a render stream, not started. The adapter owns the sample-format fan-out.
    fn open_render(
        &self,
        endpoint: &Endpoint,
        request: &OpenRequest,
        render: RenderCallback,
        error: ErrorCallback,
    ) -> Result<(Box<dyn Stream>, Achieved), OpenError>;
    /// Open a capture stream, not started. For a duplex backend the caller opens this before the
    /// render stream, on the render endpoint, and starts it separately.
    fn open_capture(
        &self,
        endpoint: &Endpoint,
        request: &OpenRequest,
        capture: CaptureCallback,
        error: ErrorCallback,
    ) -> Result<(Box<dyn Stream>, Achieved), OpenError>;
    /// Report device changes to `handler`. Returns false when the backend cannot.
    fn on_device_change(&self, _handler: Box<dyn FnMut(DeviceEvent) + Send>) -> bool {
        false
    }
    /// The `candidates` (see [`exclusive_candidates`]) that `endpoint` accepts in [`Mode::Exclusive`],
    /// in their order. A control-thread call that may take a moment and fails while another app
    /// holds the device ([`OpenError::InUse`]) or the endpoint forbids exclusive use
    /// ([`OpenError::ExclusiveDisallowed`]). A backend without exclusive mode accepts none.
    fn probe_exclusive(&self, _endpoint: &Endpoint, _candidates: &[Format]) -> Result<Vec<Format>, OpenError> {
        Ok(Vec::new())
    }
}

/// Creates the backend of the audio system a configuration names (else the default one).
pub trait BackendFactory {
    fn backend(&self, system: Option<&str>) -> Option<Box<dyn Backend>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_candidates_go_rate_then_channels_then_sample_in_preference_order() {
        let got = exclusive_candidates(48_000, &[2, 8]);
        assert_eq!(got.len(), 6 * 2 * 5);
        let samples: Vec<(SampleFormat, Option<u8>)> = got[..5].iter().map(|f| (f.sample, f.valid_bits)).collect();
        assert_eq!(
            samples,
            [
                (SampleFormat::F32, None),
                (SampleFormat::I32, Some(24)),
                (SampleFormat::I32, None),
                (SampleFormat::I24, None),
                (SampleFormat::I16, None)
            ]
        );
        assert!(got[..5].iter().all(|f| f.rate == 48_000 && f.channels == 2 && f.channel_mask.is_none()), "stereo leaves the mask to the backend");
        assert!(got[5..10].iter().all(|f| f.rate == 48_000 && f.channels == 8 && f.channel_mask == Some(0)), "other counts are direct out");
        let rates: Vec<u32> = got.iter().step_by(10).map(|f| f.rate).collect();
        assert_eq!(rates, [48_000, 44_100, 88_200, 96_000, 176_400, 192_000], "the wished rate first, then the standard ones once each");
    }

    #[test]
    fn exclusive_candidates_have_no_duplicates() {
        for (rate, channels) in [(48_000, vec![2]), (22_050, vec![1, 2, 2, 1]), (192_000, vec![6, 2])] {
            let got = exclusive_candidates(rate, &channels);
            for (i, a) in got.iter().enumerate() {
                assert!(!got[i + 1..].contains(a), "{a:?} is listed twice for {rate} {channels:?}");
            }
            assert_eq!(got[0].rate, rate);
        }
        assert_eq!(exclusive_candidates(22_050, &[1, 2, 2, 1]).len(), 7 * 2 * 5, "an odd rate is tried first, then all six standard ones");
        assert!(exclusive_candidates(48_000, &[]).is_empty());
    }
}
