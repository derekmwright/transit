//! A scripted backend for tests: no device, deterministic blocks, controllable stamps and
//! failures on demand. The test drives each callback with [`FakeBackend::pump_render`] and
//! [`FakeBackend::pump_capture`], supplying the stamp and the sink or source, so a test can use
//! the real conversion of an adapter or plain `f32` slices ([`SliceSink`], [`SliceSource`]).

use crate::{
    Achieved, Backend, BackendFactory, CaptureBlock, CaptureCallback, CaptureSource, Direction, Endpoint, ErrorCallback, Format, InputWish, Mode,
    OpenError, OpenRequest, OutputWish, Period, Plan, RenderBlock, RenderCallback, RenderSink, SCRATCH_FRAMES, SampleFormat, Stamp, Stream,
    StreamError, StreamStats, WorkerPriority,
};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

#[derive(Default)]
struct State {
    render: Option<RenderCallback>,
    capture: Option<CaptureCallback>,
    render_error: Option<ErrorCallback>,
    capture_error: Option<ErrorCallback>,
    fail_open: Option<OpenError>,
    fail_start: Option<OpenError>,
    opens: Vec<(Direction, OpenRequest)>,
    started: [bool; 2],
    /// The snapshot each direction's stream reports (output, input): a pump counts a wake; a test
    /// can script the rest.
    stats: [StreamStats; 2],
}

/// The scripted backend. Clones share one script and one set of streams.
#[derive(Clone)]
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
    name: String,
    system: String,
    attempts: Vec<OpenRequest>,
    capture_channels: u16,
    duplex: bool,
    /// Channel names (inputs, outputs) the device reports.
    names: (Vec<String>, Vec<String>),
    /// The latency each opened stream reports.
    latency: Option<Duration>,
}

impl FakeBackend {
    /// A system named "Fake" with one device that opens in `format`.
    pub fn new(format: Format) -> Self {
        let request = OpenRequest { format, period: Period::Default, mode: Mode::Shared, origin: crate::HostOrigin::default(), calibration: None };
        Self {
            state: Arc::default(),
            name: "Fake device".into(),
            system: "Fake".into(),
            attempts: vec![request],
            capture_channels: 0,
            duplex: false,
            names: (Vec::new(), Vec::new()),
            latency: None,
        }
    }
    /// The system and device names (for example "ASIO" and a driver's registry name).
    pub fn named(mut self, system: &str, device: &str) -> Self {
        (self.system, self.name) = (system.into(), device.into());
        self
    }
    /// The channel names the device reports (inputs, outputs), as `getChannelInfo` would.
    pub fn with_channel_names(mut self, inputs: Vec<String>, outputs: Vec<String>) -> Self {
        self.names = (inputs, outputs);
        self
    }
    /// The latency every stream reports in its [`Achieved`] and [`Stream::latency`].
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = Some(latency);
        self
    }
    /// Plan these requests, in order, instead of one.
    pub fn with_attempts(mut self, attempts: Vec<OpenRequest>) -> Self {
        self.attempts = attempts;
        self
    }
    /// The device has this many inputs, and (with `duplex`) they open together with the output.
    pub fn with_capture(mut self, channels: u16, duplex: bool) -> Self {
        (self.capture_channels, self.duplex) = (channels, duplex);
        self
    }
    /// The next open of either direction fails with `error`.
    pub fn fail_next_open(&self, error: OpenError) {
        self.lock().fail_open = Some(error);
    }
    /// The next stream start fails with `error`.
    pub fn fail_next_start(&self, error: OpenError) {
        self.lock().fail_start = Some(error);
    }
    /// Every open so far, in order.
    pub fn opens(&self) -> Vec<(Direction, OpenRequest)> {
        self.lock().opens.clone()
    }
    /// Whether a stream of `direction` is open and started.
    pub fn running(&self, direction: Direction) -> bool {
        let state = self.lock();
        state.started[slot(direction)] && if direction == Direction::Output { state.render.is_some() } else { state.capture.is_some() }
    }
    /// Run one render callback. False when no started render stream exists.
    pub fn pump_render(&self, frames: usize, stamp: Stamp, sink: &mut dyn RenderSink) -> bool {
        let mut state = self.lock();
        let (started, format) = (state.started[slot(Direction::Output)], self.attempts.first().map(|a| a.format));
        let (Some(render), Some(format), true) = (state.render.as_mut(), format, started) else { return false };
        render(&mut RenderBlock::new(frames, stamp, format, sink));
        state.stats[slot(Direction::Output)].wakes += 1;
        true
    }
    /// Run one capture callback. False when no started capture stream exists.
    pub fn pump_capture(&self, frames: usize, stamp: Stamp, format: Format, source: &mut dyn CaptureSource) -> bool {
        let mut state = self.lock();
        let started = state.started[slot(Direction::Input)];
        let (Some(capture), true) = (state.capture.as_mut(), started) else { return false };
        capture(&mut CaptureBlock::new(frames, stamp, format, source));
        state.stats[slot(Direction::Input)].wakes += 1;
        true
    }
    /// Script the snapshot the stream of `direction` reports from [`Stream::stats`].
    pub fn set_stream_stats(&self, direction: Direction, stats: StreamStats) {
        self.lock().stats[slot(direction)] = stats;
    }
    /// Deliver a stream fault to the error callback of `direction`.
    pub fn inject_error(&self, direction: Direction, error: StreamError) -> bool {
        let mut state = self.lock();
        let callback = if direction == Direction::Output { state.render_error.as_mut() } else { state.capture_error.as_mut() };
        callback.map(|callback| callback(error)).is_some()
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn endpoint(&self, direction: Direction) -> Endpoint {
        let (inputs, outputs) = (self.capture_channels, self.attempts.first().map_or(0, |a| a.format.channels));
        let _ = direction;
        Endpoint::new(self.name.clone(), self.name.clone(), self.system.clone(), inputs, outputs, Vec::new(), Arc::new(()))
    }
    fn open(&self, direction: Direction, request: &OpenRequest) -> Result<(Box<dyn Stream>, Achieved), OpenError> {
        let mut state = self.lock();
        state.opens.push((direction, *request));
        if let Some(error) = state.fail_open.take() {
            return Err(error);
        }
        Ok((
            Box::new(FakeStream { state: Arc::clone(&self.state), direction, latency: self.latency }),
            Achieved { buffer_frames: request.period.frames(), latency: self.latency, ..Achieved::requested(request.format, request.period) },
        ))
    }
}

fn slot(direction: Direction) -> usize {
    usize::from(direction == Direction::Input)
}

impl Backend for FakeBackend {
    fn system(&self) -> &str {
        &self.system
    }
    fn endpoints(&self, direction: Direction) -> Result<Vec<Endpoint>, OpenError> {
        Ok(vec![self.endpoint(direction)])
    }
    fn plan_render(&self, _wish: &OutputWish) -> Result<Plan, String> {
        Ok(Plan { endpoint: self.endpoint(Direction::Output), attempts: self.attempts.clone() })
    }
    fn plan_capture(&self, _wish: &InputWish) -> Result<Plan, String> {
        let format = self.capture_format(&self.endpoint(Direction::Input), None)?;
        let request = OpenRequest { format, ..self.attempts.first().copied().ok_or("no input")? };
        Ok(Plan { endpoint: self.endpoint(Direction::Input), attempts: vec![request] })
    }
    fn capture_with_render(&self, _render: &Endpoint) -> bool {
        self.duplex && self.capture_channels > 0
    }
    fn channel_names(&self, _endpoint: &Endpoint, direction: Direction) -> Vec<String> {
        match direction {
            Direction::Input => self.names.0.clone(),
            Direction::Output => self.names.1.clone(),
        }
    }
    fn capture_format(&self, _endpoint: &Endpoint, rate: Option<u32>) -> Result<Format, String> {
        let first = self.attempts.first().ok_or("no input")?.format;
        if self.capture_channels == 0 {
            return Err("the audio input has no channels".into());
        }
        Ok(Format::new(rate.unwrap_or(first.rate), self.capture_channels, SampleFormat::F32))
    }
    fn open_render(
        &self,
        _endpoint: &Endpoint,
        request: &OpenRequest,
        render: RenderCallback,
        error: ErrorCallback,
    ) -> Result<(Box<dyn Stream>, Achieved), OpenError> {
        let opened = self.open(Direction::Output, request)?;
        let mut state = self.lock();
        (state.render, state.render_error) = (Some(render), Some(error));
        Ok(opened)
    }
    fn open_capture(
        &self,
        _endpoint: &Endpoint,
        request: &OpenRequest,
        capture: CaptureCallback,
        error: ErrorCallback,
    ) -> Result<(Box<dyn Stream>, Achieved), OpenError> {
        let opened = self.open(Direction::Input, request)?;
        let mut state = self.lock();
        (state.capture, state.capture_error) = (Some(capture), Some(error));
        Ok(opened)
    }
}

impl BackendFactory for FakeBackend {
    fn backend(&self, _system: Option<&str>) -> Option<Box<dyn Backend>> {
        Some(Box::new(self.clone()))
    }
}

struct FakeStream {
    state: Arc<Mutex<State>>,
    direction: Direction,
    latency: Option<Duration>,
}

impl FakeStream {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Stream for FakeStream {
    fn start(&self) -> Result<(), OpenError> {
        let mut state = self.lock();
        if let Some(error) = state.fail_start.take() {
            return Err(error);
        }
        state.started[slot(self.direction)] = true;
        Ok(())
    }
    fn stop(&self) -> Result<(), OpenError> {
        self.lock().started[slot(self.direction)] = false;
        Ok(())
    }
    fn latency(&self) -> Option<Duration> {
        self.latency
    }
    fn thread_priority(&self) -> WorkerPriority {
        WorkerPriority::Unobserved
    }
    fn stats(&self) -> Option<StreamStats> {
        Some(self.lock().stats[slot(self.direction)])
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut state = self.lock();
        state.started[slot(self.direction)] = false;
        if self.direction == Direction::Output {
            (state.render, state.render_error) = (None, None);
        } else {
            (state.capture, state.capture_error) = (None, None);
        }
    }
}

/// A render sink over plain `f32` samples, chunked like an adapter's scratch.
pub struct SliceSink<'a> {
    pub data: &'a mut [f32],
    pub channels: usize,
}

impl RenderSink for SliceSink<'_> {
    fn render(&mut self, fill: &mut dyn FnMut(&mut [f32])) {
        for chunk in self.data.chunks_mut(SCRATCH_FRAMES * self.channels.max(1)) {
            fill(chunk);
        }
    }
}

/// A capture source over plain `f32` samples, chunked like an adapter's scratch.
pub struct SliceSource<'a> {
    pub data: &'a [f32],
    pub channels: usize,
}

impl CaptureSource for SliceSource<'_> {
    fn chunks(&mut self, each: &mut dyn FnMut(usize, &[f32])) {
        let channels = self.channels.max(1);
        for (index, chunk) in self.data.chunks(SCRATCH_FRAMES * channels).enumerate() {
            each(index * SCRATCH_FRAMES, chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> FakeBackend {
        FakeBackend::new(Format::new(48_000, 2, SampleFormat::F32))
    }

    #[test]
    fn scripted_blocks_reach_the_render_callback_with_their_stamps() {
        let fake = fake();
        let plan = fake.plan_render(&OutputWish::default()).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let render: RenderCallback = Box::new(move |block| {
            let stamp = block.stamp;
            block.render(&mut |chunk| chunk.fill(0.5));
            log.lock().unwrap().push((block.frames, stamp.backend_callback_ns));
        });
        let (stream, achieved) = fake.open_render(&plan.endpoint, &plan.attempts[0], render, Box::new(|_| {})).unwrap();
        assert_eq!(achieved.format.channels, 2);
        let mut data = vec![0.0f32; 8];
        let mut sink = SliceSink { data: &mut data, channels: 2 };
        assert!(!fake.pump_render(4, Stamp::INVALID, &mut sink), "not started yet");
        stream.start().unwrap();
        let stamp = Stamp { backend_callback_ns: -7, ..Stamp::INVALID };
        assert!(fake.pump_render(4, stamp, &mut sink));
        assert_eq!(*seen.lock().unwrap(), [(4, -7)]);
        assert_eq!(data, [0.5; 8]);
        drop(stream);
        assert!(!fake.running(Direction::Output));
    }

    #[test]
    fn failures_and_stream_errors_are_scripted() {
        let fake = fake();
        let plan = fake.plan_render(&OutputWish::default()).unwrap();
        fake.fail_next_open(OpenError::DeviceNotAvailable("gone".into()));
        let open = |fake: &FakeBackend, error: ErrorCallback| fake.open_render(&plan.endpoint, &plan.attempts[0], Box::new(|_| {}), error);
        assert_eq!(open(&fake, Box::new(|_| {})).err(), Some(OpenError::DeviceNotAvailable("gone".into())));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&errors);
        let (stream, _) = open(&fake, Box::new(move |e| log.lock().unwrap().push(e))).unwrap();
        fake.fail_next_start(OpenError::Other("busy".into()));
        assert_eq!(stream.start().unwrap_err().to_string(), "busy");
        stream.start().unwrap();
        assert!(fake.inject_error(Direction::Output, StreamError::DeviceNotAvailable));
        assert_eq!(*errors.lock().unwrap(), [StreamError::DeviceNotAvailable]);
        assert_eq!(fake.opens().len(), 2);
    }

    #[test]
    fn a_stream_reports_a_snapshot_that_counts_pumps_and_can_be_scripted() {
        let fake = fake();
        let plan = fake.plan_render(&OutputWish::default()).unwrap();
        let (stream, _) = fake.open_render(&plan.endpoint, &plan.attempts[0], Box::new(|_| {}), Box::new(|_| {})).unwrap();
        stream.start().unwrap();
        let mut data = [0.0f32; 8];
        let mut sink = SliceSink { data: &mut data, channels: 2 };
        assert!(fake.pump_render(4, Stamp::INVALID, &mut sink));
        assert!(fake.pump_render(4, Stamp::INVALID, &mut sink));
        assert_eq!(stream.stats().map(|s| s.wakes), Some(2));
        fake.set_stream_stats(Direction::Output, StreamStats { skipped_wakes: 3, ..Default::default() });
        assert_eq!(stream.stats().map(|s| (s.wakes, s.skipped_wakes)), Some((0, 3)));
    }

    #[test]
    fn capture_chunks_carry_frame_offsets() {
        let fake = fake().with_capture(2, true);
        let plan = fake.plan_capture(&InputWish::default()).unwrap();
        assert!(fake.capture_with_render(&plan.endpoint));
        let got = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&got);
        let capture: CaptureCallback = Box::new(move |block| block.chunks(&mut |offset, samples| log.lock().unwrap().push((offset, samples.len()))));
        let (stream, _) = fake.open_capture(&plan.endpoint, &plan.attempts[0], capture, Box::new(|_| {})).unwrap();
        stream.start().unwrap();
        let data = vec![0.25f32; (SCRATCH_FRAMES + 3) * 2];
        let mut source = SliceSource { data: &data, channels: 2 };
        assert!(fake.pump_capture(SCRATCH_FRAMES + 3, Stamp::INVALID, plan.attempts[0].format, &mut source));
        assert_eq!(*got.lock().unwrap(), [(0, SCRATCH_FRAMES * 2), (SCRATCH_FRAMES, 6)]);
    }
}
