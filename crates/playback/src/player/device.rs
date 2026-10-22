//! Device facade for the prepared runtime. All factories, editors and reclamation
//! belong to this creating-thread owner; the device borrows its anchored audio state.
use super::audio::{ActionResult, Audio, Command, CountIn};
use super::{Notice, PlayerError, RuntimeControl, ServiceReport, SessionRequestId, sources};
use crate::record::start::Status;
use crate::{AudioConfig, EditorEdit, Fault, HeldInput, MeterSnapshot, OutputTrouble, PluginStates, Shared};
use crate::{action_lane, device_clock, devices, plan_bridge, record, scheduling, telemetry};
use soundcraft_mix::{Arrival, DeviceGeneration, Due, EngineEpoch, InstrumentEvent, Message, NoteId, PlanLimits, RenderTarget, Source};
use soundcraft_model::{IoMap, Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::cell::{Cell, RefCell, RefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use transit_audio_backend as ab;
use transit_rt::handoff::{Anchor, AudioAccess};

type Owner = RuntimeControl<Command, ActionResult>;
pub type PlayerNotice = Notice<Command, ActionResult>;

/// A prepared MIDI recording pass.
struct MidiPass {
    reader: record::start::Reader,
    tracks: Vec<TrackId>,
}

/// What a MIDI recording pass captured.
#[derive(Debug, Clone)]
pub struct MidiTake {
    /// The armed instrument tracks the pass recorded.
    pub tracks: Vec<TrackId>,
    /// The record-start latch the audio thread applied; None if it never started.
    pub start: Option<record::start::Point>,
    pub events: Vec<crate::midi_capture::CapturedEvent>,
    /// Events lost to a full queue.
    pub dropped: u64,
    pub device_rate: u32,
}

impl MidiTake {
    /// The events of the pass's tracks at or after the latch, each with the session samples
    /// since the latch: the device frames since the latch frame, converted at the session rate
    /// (the same latch the audio take is placed from). Events before the latch are not part of
    /// the take.
    pub fn timed(&self, session_rate: u32) -> Vec<(i64, crate::midi_capture::CapturedEvent)> {
        let Some(point) = self.start else { return Vec::new() };
        let rate = u128::from(self.device_rate.max(1));
        self.events
            .iter()
            .filter(|e| self.tracks.contains(&e.track))
            .filter_map(|e| {
                let frames = e.frame.checked_sub(point.output_frame)?;
                let samples = (u128::from(frames) * u128::from(session_rate) + rate / 2) / rate;
                Some((i64::try_from(samples).ok()?, *e))
            })
            .collect()
    }
}

/// Main-thread facade. RuntimeControl deliberately makes this type !Send/!Sync.
/// The remaining native lifecycle migration moves stream/writer joins off main;
/// it does not move this owner or its plug-ins to that worker.
pub struct Player {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    failure_signal: Option<plan_bridge::RuntimeIdentity>,
    owner: RefCell<Option<Owner>>,
    #[cfg(not(target_arch = "wasm32"))]
    midi: RefCell<Option<super::native::Native>>,
    #[cfg(not(target_arch = "wasm32"))]
    midi_error: Option<String>,
    record_start: RefCell<Option<record::start::Control>>,
    /// MIDI recording capture (control side) and the pass it is collecting.
    capture: RefCell<Option<crate::midi_capture::Capture>>,
    midi_pass: RefCell<Option<MidiPass>>,
    state: Option<Anchor<Audio>>,
    #[cfg(feature = "test-hooks")]
    test_output: RefCell<Option<DeviceOutput>>,
    stream: Option<Box<dyn ab::Stream>>,
    input: RefCell<HeldInput>,
    clock: RefCell<Option<device_clock::MaintainedClock>>,
    origin: device_clock::HostOrigin,
    epoch: EngineEpoch,
    audition: Option<sources::Token>,
    arrival: Cell<u64>,
    initial_error: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    watch: RefCell<Option<(u64, std::time::Instant)>>,
    #[cfg(not(target_arch = "wasm32"))]
    silent_worker: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(target_arch = "wasm32"))]
    silent_stop: Arc<AtomicBool>,
    pub device_name: String,
    /// The audio system (cpal host) of the open device: "ASIO", "WASAPI", ...; empty for the silent clock.
    pub device_system: String,
    /// The open device is served by the native WASAPI backend (ADR 0016), not cpal.
    pub native_backend: bool,
    /// Said when the chosen backend did not open and another did: "Native WASAPI failed: ...".
    pub backend_note: Option<String>,
    pub device_rate: u32,
    pub device_channels: u16,
    /// This machine's paths on the open device (ADR 0015).
    io: RefCell<Arc<IoMap>>,
    pub device_buffer: Option<u32>,
    pub silent: bool,
}

impl Player {
    pub fn new(session: Arc<Session>) -> Self {
        Self::with_config(session, &AudioConfig::default())
    }

    /// Device failure falls back to an explicitly silent runtime. Initialization
    /// or identity exhaustion instead returns a faulted facade that rejects work.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> Self {
        match Self::open(Arc::clone(&session), cfg) {
            Ok(player) => player,
            Err(error) => {
                log::warn!("audio output unavailable: {error}");
                Self::silent_runtime(session, error)
            }
        }
    }

    /// Prepare the real Player without discovering or opening any device.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn prepare_device_free(session: Arc<Session>) -> Result<Self, String> {
        let rate = session.sample_rate.hz();
        Self::prepare(session, rate, IoMap::default()).map(|(mut player, access, _clock)| {
            let output = DeviceOutput::new(access, &player, 2);
            *player.test_output.get_mut() = Some(output);
            player
        })
    }

    /// Consume one complete stereo block for device-free caller integration tests.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn step_device_free(&self) {
        crate::mark_audio_thread();
        if let Ok(mut output) = self.test_output.try_borrow_mut()
            && let Some(output) = output.as_mut()
        {
            output.fill_f32(&mut [0.0f32; 1024]);
        }
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            editors_open: AtomicBool::new(false),
            metro: crate::Metro::default(),
        })
    }

    fn empty(rate: u32) -> Self {
        Self {
            shared: Self::shared(),
            fault: Arc::default(),
            failure_signal: None,
            owner: RefCell::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            midi: RefCell::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            midi_error: None,
            record_start: RefCell::new(None),
            capture: RefCell::new(None),
            midi_pass: RefCell::new(None),
            state: None,
            #[cfg(feature = "test-hooks")]
            test_output: RefCell::new(None),
            stream: None,
            input: RefCell::new(HeldInput::No),
            clock: RefCell::new(None),
            origin: device_clock::HostOrigin::default(),
            epoch: EngineEpoch(0),
            audition: None,
            arrival: Cell::new(0),
            initial_error: None,
            #[cfg(not(target_arch = "wasm32"))]
            watch: RefCell::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            silent_worker: None,
            #[cfg(not(target_arch = "wasm32"))]
            silent_stop: Arc::new(AtomicBool::new(false)),
            device_name: "No audio device (silent)".into(),
            device_system: String::new(),
            native_backend: false,
            backend_note: None,
            device_rate: rate,
            device_channels: 0,
            io: RefCell::new(Arc::new(IoMap::default())),
            device_buffer: None,
            silent: true,
        }
    }

    /// The stream clock is allocated BEFORE the engine; its exact generation is
    /// shared by the target, event identity, and consumed-output timeline.
    fn prepare(session: Arc<Session>, device_rate: u32, io: IoMap) -> Result<(Self, AudioAccess<Audio>, device_clock::CallbackClock), String> {
        let (callback_clock, reader) = device_clock::stream_clock(device_clock::Direction::Output, device_rate)?;
        let key = reader.key();
        let target = RenderTarget {
            device: DeviceGeneration(key.generation),
            sample_rate: session.sample_rate.hz(),
            max_block: 512,
            out_channels: io.outputs,
            in_channels: io.inputs,
        };
        let mut player = Self::empty(device_rate);
        player.epoch = EngineEpoch(key.generation);
        player.origin = reader.host_origin();
        let (mut owner, ports) = Owner::new(session, None, target, PlanLimits::default()).map_err(|e| e.to_string())?;
        let audition = match owner.register_source(sources::AUDITION_PORT) {
            Ok(token) => token,
            Err(error) => {
                owner.retire(ports);
                return Err(error.to_string());
            }
        };
        let io = Arc::new(io);
        owner.use_io(Arc::clone(&io));
        *player.io.get_mut() = io;
        owner.service();
        if owner.faulted() {
            owner.retire(ports);
            return Err("initial audio plan preparation faulted".into());
        }
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let (mut audio, record_start) = match Audio::from_ports(ports, key, device_rate, Arc::clone(&player.shared)) {
            Ok(audio) => audio,
            Err(failure) => {
                owner.retire(*failure.ports);
                return Err(format!("invalid output timeline: {:?}", failure.error));
            }
        };
        *player.clock.get_mut() =
            device_clock::MaintainedClock::start(reader).map_err(|error| log::warn!("output clock maintenance unavailable: {error}")).ok();
        #[cfg(not(target_arch = "wasm32"))]
        audio.set_clock(player.clock.get_mut().as_mut().and_then(device_clock::MaintainedClock::take_audio_reader), player.origin);
        #[cfg(not(target_arch = "wasm32"))]
        match owner.midi_lane() {
            Ok((control, endpoint)) => match super::native::Native::new(control, player.origin) {
                Ok(native) => {
                    audio.set_midi(endpoint);
                    *player.midi.get_mut() = Some(native);
                }
                Err(error) => player.midi_error = Some(error.to_string()),
            },
            Err(error) => player.midi_error = Some(error.to_string()),
        }
        *player.record_start.get_mut() = Some(record_start);
        let (capture, sink) = crate::midi_capture::pair(crate::midi_capture::CAPACITY);
        audio.set_midi_capture(sink);
        *player.capture.get_mut() = Some(capture);
        let (anchor, access) = Anchor::new(audio);
        player.state = Some(anchor);
        player.failure_signal = Some(owner.failure_signal());
        *player.owner.get_mut() = Some(owner);
        player.audition = Some(audition);
        Ok((player, access, callback_clock))
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig) -> Result<Self, String> {
        #[cfg(all(windows, feature = "native-wasapi"))]
        if cfg.backend == crate::Backend::NativeWasapi && crate::devices::is_wasapi_system(cfg.system.as_deref()) {
            return Self::open_native_or_cpal(session, cfg, &transit_audio_wasapi::WasapiFactory, &crate::backend::cpal::CpalFactory);
        }
        Self::open_with(session, cfg, &crate::backend::cpal::CpalFactory)
    }

    /// The native WASAPI backend; when it cannot open (before `start()` succeeded) the cpal backend
    /// opens the same choices and the Player says why. The failed attempt's streams are dropped
    /// here, on the control thread: dropping a native stream joins its thread (see
    /// `transit-audio-wasapi`'s `RenderStream`) and must never happen on a callback thread.
    #[cfg(any(test, all(windows, feature = "native-wasapi")))]
    fn open_native_or_cpal(
        session: Arc<Session>,
        cfg: &AudioConfig,
        native: &dyn ab::BackendFactory,
        fallback: &dyn ab::BackendFactory,
    ) -> Result<Self, String> {
        match Self::open_with(Arc::clone(&session), cfg, native) {
            Ok(mut player) => {
                player.native_backend = true;
                Ok(player)
            }
            Err(reason) => {
                log::warn!("native WASAPI did not open ({reason}); using the cpal backend");
                let mut player = Self::open_with(session, cfg, fallback)?;
                player.backend_note = Some(native_fallback_note(&reason));
                Ok(player)
            }
        }
    }

    /// Open the output through the audio system `factory` makes. Every device call goes through
    /// the backend trait; the adapter owns the sample format, the ASIO duplex order and the
    /// timestamps of the clocks.
    fn open_with(session: Arc<Session>, cfg: &AudioConfig, factory: &dyn ab::BackendFactory) -> Result<Self, String> {
        let want_ch = u16::try_from(soundcraft_mix::main_channels(&session)).unwrap_or(2);
        let backend = factory.backend(cfg.system.as_deref()).ok_or_else(|| "no audio system".to_string())?;
        // Prefer a config at the chosen (else session) rate; else the device default (we resample).
        let rates = cfg.sample_rate.into_iter().chain(Some(session.sample_rate.hz())).collect();
        let plan = backend.plan_render(&ab::OutputWish { device: cfg.output.clone(), rates, channels: want_ch, period: cfg.buffer_size })?;
        let (endpoint, system, name) = (&plan.endpoint, backend.system().to_owned(), plan.endpoint.name.clone());
        let first = plan.attempts.first().ok_or("no usable output stream")?;
        let duplex = backend.capture_with_render(endpoint);
        // Every output the stream opened, and the duplex device's inputs, get default paths; the
        // app replaces them with this machine's saved table (`set_io`).
        let inputs = if duplex { backend.capture_format(endpoint, Some(first.format.rate)).map_or(0, |format| format.channels) } else { 0 };
        let wide = first.format.channels;
        let mut error = "no usable output stream".to_owned();
        for request in &plan.attempts {
            let (rate, channels) = (request.format.rate, request.format.channels);
            let io = IoMap::for_device(&system, &name, inputs, channels);
            let (mut player, access, clock) = Self::prepare(Arc::clone(&session), rate, io)?;
            *player.input.get_mut() = if duplex {
                let progress = player.record_start.get_mut().as_ref().map(record::start::Control::input_progress);
                let built = progress
                    .ok_or_else(|| "the record start latch is missing".to_owned())
                    .and_then(|progress| record::Recorder::build_on(&*backend, endpoint, rate, request.period, progress));
                match built {
                    Ok(recorder) => HeldInput::Held(Some(recorder)),
                    Err(error) => HeldInput::Failed(format!("the audio input did not open with the output ({error})")),
                }
            } else if system == devices::ASIO_SYSTEM {
                HeldInput::Failed("the ASIO output has no usable input".into())
            } else {
                HeldInput::No
            };
            let (render, on_error) = render_callbacks(&player, access, clock, usize::from(channels).max(1), rate);
            let (stream, achieved) = match backend.open_render(endpoint, request, render, on_error) {
                Ok(opened) => opened,
                Err(failure) => {
                    error = failure.to_string();
                    continue;
                }
            };
            player.stream = Some(stream);
            if let HeldInput::Held(Some(recorder)) = player.input.get_mut()
                && let Err(failure) = recorder.start()
            {
                // Keep the already-built ASIO direction owned until output closes;
                // removing it here could rebuild the shared driver's buffers.
                log::warn!("audio input did not start: {failure}");
                error = failure.to_string();
                continue;
            }
            let started = player.stream.as_ref().ok_or("output stream disappeared")?.start();
            if let Err(failure) = started {
                error = failure.to_string();
                continue;
            }
            log::info!("audio output opened with {} channels (asked for {})", achieved.format.channels, wide);
            player.device_name = name.clone();
            player.device_system = system.clone();
            player.device_channels = achieved.format.channels;
            player.device_buffer = achieved.period.frames();
            player.silent = false;
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(midi) = player.midi.get_mut() {
                midi.enable(transit_midi_io::MidiIo::new("transit"));
            }
            return Ok(player);
        }
        Err(error)
    }

    fn silent_runtime(session: Arc<Session>, error: String) -> Self {
        let rate = session.sample_rate.hz();
        let (mut player, access, _clock) = match Self::prepare(session, rate, IoMap::default()) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let mut player = Self::empty(rate);
                player.initial_error = Some(format!("{error}; silent runtime failed: {failure}"));
                player.fault.failed.store(true, Ordering::Release);
                return player;
            }
        };
        // A timer-driven fallback never claims a physical output clock map.
        *player.clock.get_mut() = None;
        player.initial_error = Some(error);
        #[cfg(not(target_arch = "wasm32"))]
        player.start_silent(access, |task| std::thread::Builder::new().name("soundcraft-silent-clock".into()).spawn(task));
        #[cfg(target_arch = "wasm32")]
        drop(access);
        player
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_silent(
        &mut self,
        access: AudioAccess<Audio>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) {
        let rate = self.device_rate;
        let mut callback = DeviceOutput::new(access, self, 2);
        let (shared, fault, stop) = (Arc::clone(&self.shared), Arc::clone(&self.fault), Arc::clone(&self.silent_stop));
        let failure_signal = self.failure_signal.clone();
        let spawn = spawn(Box::new(move || {
            let mut data = vec![0.0f32; 16_384];
            let mut last = std::time::Instant::now();
            let mut pacer = SilentPacer::default();
            while !stop.load(Ordering::Acquire) {
                std::thread::park_timeout(std::time::Duration::from_millis(10));
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let now = std::time::Instant::now();
                let frames = pacer.advance(now - last, rate, data.len() / 2);
                last = now;
                let Some(frames) = frames else {
                    if let Some(signal) = &failure_signal {
                        signal.fault();
                    }
                    fault.failed.store(true, Ordering::Release);
                    shared.playing.store(false, Ordering::Release);
                    break;
                };
                let len = frames * 2;
                fault.beats.fetch_add(1, Ordering::Relaxed);
                if let Some(data) = data.get_mut(..len) {
                    callback.fill_f32(data);
                }
            }
        }));
        match spawn {
            Ok(worker) => self.silent_worker = Some(worker),
            Err(error) => {
                self.fault.failed.store(true, Ordering::Release);
                if let Some(owner) = self.owner.get_mut() {
                    owner.fault();
                }
                self.initial_error = Some(format!("silent clock did not start: {error}"));
            }
        }
    }

    fn owner(&self) -> Result<RefMut<'_, Owner>, PlayerError> {
        let owner = self.owner.try_borrow_mut().map_err(|_| PlayerError::Busy)?;
        let mut owner = RefMut::filter_map(owner, Option::as_mut).map_err(|_| PlayerError::Faulted)?;
        if self.fault.failed.load(Ordering::Acquire) || self.fault.gone.load(Ordering::Acquire) {
            owner.fault();
        }
        Ok(owner)
    }
    pub fn initial_error(&self) -> Option<&str> {
        self.initial_error.as_deref()
    }
    pub fn update_session(&self, session: Arc<Session>) -> Result<SessionRequestId, PlayerError> {
        self.owner()?.update_session(session)
    }
    /// Restore an explicitly replaced document without reopening a same-rate device.
    pub fn replace_session(&self, session: Arc<Session>) -> Result<SessionRequestId, PlayerError> {
        self.owner()?.replace_session(session)
    }
    /// Includes view-only copy-on-write changes that do not advance Engine::revision.
    pub fn has_requested_session(&self, session: &Arc<Session>) -> bool {
        self.owner().is_ok_and(|owner| Arc::ptr_eq(owner.requested_session(), session))
    }
    /// The path table in use: this machine's, once the app has set it, else the defaults for the
    /// opened device.
    pub fn io_map(&self) -> Arc<IoMap> {
        Arc::clone(&self.io.borrow())
    }
    /// Use `io` as the path table. The opened output count stays the stream's (a different count
    /// is a reopen), so paths that do not fit are dropped; the table reaches the audio thread as
    /// a plan, with no device reopen.
    pub fn set_io(&self, mut io: IoMap) -> Result<SessionRequestId, PlayerError> {
        let opened = if self.silent { io.outputs } else { self.device_channels };
        io.reconcile(io.inputs, opened);
        let io = Arc::new(io);
        let id = self.owner()?.set_io(Arc::clone(&io))?;
        if let Ok(mut held) = self.io.try_borrow_mut() {
            *held = io;
        }
        Ok(id)
    }
    pub fn set_input(&self, input: Arc<record::InputRing>) -> Result<SessionRequestId, PlayerError> {
        self.owner()?.set_input(Some(input))
    }
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) -> Result<action_lane::Sequence, PlayerError> {
        self.owner()?.send_action(Command::Play { from, end, looped }, true)
    }
    /// Retarget (or clear) the loop of a running transport without moving the playhead.
    pub fn set_loop(&self, looped: Option<Range>) -> Result<action_lane::Sequence, PlayerError> {
        self.owner()?.send_action(Command::SetLoop(looped), true)
    }
    pub fn locate(&self, at: Samples) -> Result<action_lane::Sequence, PlayerError> {
        self.cancel_record_start()?;
        self.owner()?.send_action(Command::Locate(at), true)
    }
    /// Prepare input history and files on Main. The device applies the start in
    /// the same ordered transaction as Play, without waiting for a UI receipt.
    pub fn prepare_recording(&self, recorder: &mut record::Recorder, takes: Vec<(TrackId, Vec<usize>, std::path::PathBuf)>) -> Result<(), String> {
        self.owner().map_err(|error| error.to_string())?;
        if recorder.has_pending_take() {
            return Err("already recording".into());
        }
        let mut control = self.record_start.try_borrow_mut().map_err(|_| PlayerError::Busy.to_string())?;
        let control = control.as_mut().ok_or_else(|| PlayerError::Faulted.to_string())?;
        let reader = control.prepare().ok_or_else(|| PlayerError::SequenceExhausted.to_string())?;
        if let Err(error) = recorder.prepare(takes, reader) {
            control.cancel();
            return Err(error);
        }
        Ok(())
    }
    /// `from=None` punches in at the consumed playhead without flushing DSP.
    pub fn record(
        &self,
        recorder: &record::Recorder,
        from: Option<Samples>,
        end: Option<Samples>,
        looped: Option<Range>,
    ) -> Result<action_lane::Sequence, PlayerError> {
        self.record_with_count_in(recorder, from, end, looped, None)
    }
    /// As [`Player::record`], but with `count_in` the click plays first while the transport waits at
    /// `from`, and Play with the recording latch starts at the sample the count-in ends.
    pub fn record_with_count_in(
        &self,
        recorder: &record::Recorder,
        from: Option<Samples>,
        end: Option<Samples>,
        looped: Option<Range>,
        count_in: Option<CountIn>,
    ) -> Result<action_lane::Sequence, PlayerError> {
        let ticket = recorder.prepared_start().ok_or(PlayerError::NoPreparedRecording)?;
        self.start_recording(ticket, from, end, looped, count_in)
    }
    /// As [`Player::record_with_count_in`] for a pass that records MIDI only (no audio input).
    pub fn record_midi_with_count_in(
        &self,
        from: Option<Samples>,
        end: Option<Samples>,
        looped: Option<Range>,
        count_in: Option<CountIn>,
    ) -> Result<action_lane::Sequence, PlayerError> {
        let ticket = self.midi_pass.try_borrow().map_err(|_| PlayerError::Busy)?.as_ref().map(|pass| pass.reader.ticket());
        self.start_recording(ticket.ok_or(PlayerError::NoPreparedRecording)?, from, end, looped, count_in)
    }
    fn start_recording(
        &self,
        ticket: record::start::Ticket,
        from: Option<Samples>,
        end: Option<Samples>,
        looped: Option<Range>,
        count_in: Option<CountIn>,
    ) -> Result<action_lane::Sequence, PlayerError> {
        let result = self.owner()?.send_action(Command::Record { ticket, from, end, looped, count_in }, true);
        if result.is_err() {
            self.cancel_record_start()?;
        }
        result
    }
    /// Prepare a MIDI recording pass of `tracks` (armed instrument tracks): clear the capture and
    /// share the record-start latch of the audio take prepared for the same pass (`audio`), else
    /// issue a latch of its own. The latch is applied by the audio thread with Play, so the pass
    /// starts at the same sample as any audio take.
    pub fn prepare_midi_recording(&self, tracks: Vec<TrackId>, audio: Option<record::start::Reader>) -> Result<(), String> {
        self.owner().map_err(|error| error.to_string())?;
        let reader = match audio {
            Some(reader) => reader,
            None => {
                let mut control = self.record_start.try_borrow_mut().map_err(|_| PlayerError::Busy.to_string())?;
                let control = control.as_mut().ok_or_else(|| PlayerError::Faulted.to_string())?;
                control.prepare().ok_or_else(|| PlayerError::SequenceExhausted.to_string())?
            }
        };
        let mut capture = self.capture.try_borrow_mut().map_err(|_| PlayerError::Busy.to_string())?;
        capture.as_mut().ok_or_else(|| PlayerError::Faulted.to_string())?.clear();
        *self.midi_pass.try_borrow_mut().map_err(|_| PlayerError::Busy.to_string())? = Some(MidiPass { reader, tracks });
        Ok(())
    }
    /// Is a MIDI pass prepared and not yet finished?
    pub fn midi_recording_prepared(&self) -> bool {
        self.midi_pass.try_borrow().is_ok_and(|pass| pass.is_some())
    }
    /// The events captured for the pass so far that belong to it (after the latch, on its tracks),
    /// as `(samples since the latch, event)` at `session_rate`. Drains the queue into the take
    /// list without consuming it, so Stop still finds every event.
    pub fn midi_recording_progress(&self, session_rate: u32) -> Vec<(i64, crate::midi_capture::CapturedEvent)> {
        let (Ok(mut capture), Ok(pass)) = (self.capture.try_borrow_mut(), self.midi_pass.try_borrow()) else { return Vec::new() };
        let (Some(capture), Some(pass)) = (capture.as_mut(), pass.as_ref()) else { return Vec::new() };
        capture.poll();
        let Status::Started(point) = pass.reader.status() else { return Vec::new() };
        MidiTake { tracks: pass.tracks.clone(), start: Some(point), events: capture.events().to_vec(), dropped: 0, device_rate: self.device_rate }
            .timed(session_rate)
    }
    /// The pass so far as a take (latch if applied, events captured to now), for drawing the
    /// recording while it runs. Drains the queue into the take list; nothing is consumed.
    pub fn midi_recording_snapshot(&self) -> Option<MidiTake> {
        let (mut capture, pass) = (self.capture.try_borrow_mut().ok()?, self.midi_pass.try_borrow().ok()?);
        let (capture, pass) = (capture.as_mut()?, pass.as_ref()?);
        capture.poll();
        let start = match pass.reader.status() {
            Status::Started(point) => Some(point),
            _ => None,
        };
        Some(MidiTake { tracks: pass.tracks.clone(), start, events: capture.events().to_vec(), dropped: 0, device_rate: self.device_rate })
    }
    /// End the MIDI pass: everything captured, the latch and the loss count. None when no pass
    /// was prepared.
    pub fn finish_midi_recording(&self) -> Option<MidiTake> {
        let pass = self.midi_pass.try_borrow_mut().ok()?.take()?;
        let mut capture = self.capture.try_borrow_mut().ok()?;
        let (events, dropped) = capture.as_mut()?.finish();
        let start = match pass.reader.status() {
            Status::Started(point) => Some(point),
            _ => None,
        };
        Some(MidiTake { tracks: pass.tracks, start, events, dropped, device_rate: self.device_rate })
    }
    /// Stand in for the audio thread: add an event to the capture, as a device-free test.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_midi_capture(&self, event: crate::midi_capture::CapturedEvent) {
        if let Ok(mut capture) = self.capture.try_borrow_mut()
            && let Some(capture) = capture.as_mut()
        {
            capture.inject(event);
        }
    }
    fn cancel_record_start(&self) -> Result<(), PlayerError> {
        if let Some(control) = self.record_start.try_borrow().map_err(|_| PlayerError::Busy)?.as_ref() {
            control.cancel();
        }
        Ok(())
    }
    pub fn set_recording(&self, on: bool) -> Result<action_lane::Sequence, PlayerError> {
        if !on {
            self.cancel_record_start()?;
        }
        self.owner()?.send_action(Command::Recording(on), false)
    }
    pub fn stop(&self) -> Result<action_lane::Sequence, PlayerError> {
        self.cancel_record_start()?;
        let mut owner = self.owner()?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(midi) = &*self.midi.try_borrow().map_err(|_| PlayerError::Busy)? {
            midi.cutoff(&mut owner)?;
        }
        owner.stop()
    }
    pub fn all_notes_off(&self) -> Result<action_lane::Sequence, PlayerError> {
        let mut owner = self.owner()?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(midi) = &*self.midi.try_borrow().map_err(|_| PlayerError::Busy)? {
            midi.cutoff(&mut owner)?;
        }
        owner.all_notes_off()
    }
    pub fn note(&self, track: TrackId, pitch: u8, velocity: u8) -> Result<action_lane::Sequence, PlayerError> {
        if pitch > 127 || velocity > 127 {
            return Err(PlayerError::InvalidNote);
        }
        let mut owner = self.owner()?;
        let token = self.audition.ok_or(PlayerError::Faulted)?;
        if !owner.source_admitted(token) {
            return Err(PlayerError::Faulted);
        }
        let target = owner.binding(track, soundcraft_mix::INSTRUMENT_SLOT).ok_or(PlayerError::NoInstrument)?;
        let sequence = self.arrival.get().checked_add(1).ok_or(PlayerError::SequenceExhausted)?;
        self.arrival.set(sequence);
        let message = if velocity == 0 {
            Message::NoteOff { key: pitch, velocity: 0, id: NoteId::NONE }
        } else {
            Message::NoteOn { key: pitch, velocity, id: NoteId::NONE }
        };
        owner.send_note(
            Command::Note(InstrumentEvent {
                engine: self.epoch,
                target,
                due: Due::Next,
                transport: None,
                source: Source { port: token.port(), channel: 0, generation: token.generation() },
                message,
                arrival: Arrival { host_ns: self.origin.now_ns().unwrap_or(0), sequence },
            }),
            if velocity == 0 { action_lane::NoteKind::Off } else { action_lane::NoteKind::On },
        )
    }
    /// Stop / All Notes Off acknowledgments are monotonic frontiers. An admitted
    /// urgent request is applied only once its corresponding frontier reaches it.
    pub fn recovery_progress(&self) -> Result<(action_lane::Sequence, action_lane::Sequence), PlayerError> {
        Ok(self.owner()?.recovery_progress())
    }
    pub fn poll_notices(&self, out: &mut [Option<PlayerNotice>]) -> Result<usize, PlayerError> {
        Ok(self.owner()?.poll_notices(out))
    }
    pub fn service(&self) -> Result<ServiceReport, PlayerError> {
        let mut owner = self.owner()?;
        if self.fault.failed.load(Ordering::Acquire) || self.fault.gone.load(Ordering::Acquire) {
            owner.fault();
        }
        let report = owner.service();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(midi) = &mut *self.midi.try_borrow_mut().map_err(|_| PlayerError::Busy)? {
            midi.service(&mut owner, self.epoch);
        }
        Ok(report)
    }
    /// Notes played on armed MIDI inputs since the last call (the UI's step entry); empty without MIDI.
    pub fn take_played_notes(&self) -> Vec<super::PlayedNote> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(midi) = self.midi.try_borrow()
            && let Some(m) = midi.as_ref()
        {
            return m.take_notes();
        }
        Vec::new()
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn midi_status(&self) -> Result<super::MidiStatus, PlayerError> {
        let midi = self.midi.try_borrow().map_err(|_| PlayerError::Busy)?;
        let mut status =
            midi.as_ref().map_or_else(|| super::MidiStatus { error: self.midi_error.clone(), ..Default::default() }, super::native::Native::status);
        status.timing_quality = self.clock_report().and_then(|report| report.map).map(|map| map.quality);
        Ok(status)
    }
    pub fn idle(&self) -> Result<Vec<EditorEdit>, PlayerError> {
        self.service()?;
        let mut owner = self.owner()?;
        let edits = owner.idle_editors();
        self.shared.editors_open.store(owner.any_editor_open(), Ordering::Relaxed);
        Ok(edits)
    }
    /// Refuse a save until the exact caller snapshot has a confirmed active plan.
    pub fn capture_states(&self, expected: &Arc<Session>) -> Result<PluginStates, PlayerError> {
        self.owner()?.capture_states(expected)
    }
    /// Preserve confirmed live state before a device reopen, including after a
    /// fault. None requires an explicit document-only recovery notice to the user.
    pub fn capture_states_for_reopen(&self, expected: &Arc<Session>) -> Result<Option<PluginStates>, PlayerError> {
        self.owner()?.capture_states_for_reopen(expected)
    }
    /// Whether this slot's plug-in is still being made (the slot plays silence until it is in a
    /// plan). The UI shows "Loading…" for it.
    pub fn loading(&self, track: TrackId, slot: usize) -> bool {
        self.owner().is_ok_and(|owner| owner.loading(track, slot))
    }
    /// Whether any plug-in is still being made; the frame loop comes back soon while true.
    pub fn any_loading(&self) -> bool {
        self.owner().is_ok_and(|owner| owner.any_loading())
    }
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.owner().is_ok_and(|owner| owner.has_editor(track, slot))
    }
    /// Whether the live plug-in in this insert slot has a sidechain input (the device card's key
    /// control is offered for it).
    pub fn has_key_input(&self, track: TrackId, slot: usize) -> bool {
        self.owner().is_ok_and(|owner| owner.has_key_input(track, slot))
    }
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.owner().map_err(|e| e.to_string())?.open_editor(track, slot)?;
        self.shared.editors_open.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn close_editor(&self, track: TrackId, slot: usize) -> Result<(), PlayerError> {
        self.owner()?.close_editor(track, slot);
        Ok(())
    }
    pub fn close_editors(&self) -> Result<(), PlayerError> {
        self.owner()?.close_editors();
        self.shared.editors_open.store(false, Ordering::Relaxed);
        Ok(())
    }
    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.owner().is_ok_and(|owner| owner.editor_open(track, slot))
    }
    pub fn any_editor_open(&self) -> bool {
        self.shared.editors_open.load(Ordering::Relaxed)
    }
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) -> Result<(), PlayerError> {
        self.owner()?.editor_set_param(track, slot, id, value);
        Ok(())
    }
    pub fn set_speed(&self, speed: f32) {
        let value = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(value, Ordering::Relaxed);
    }
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }
    /// The click's level in dB, on top of Setup > Click/Countoff. Reaches the audio thread with
    /// one atomic store; no session change, no undo.
    pub fn set_metronome_db(&self, db: f32) {
        let gain = if db.is_finite() { 10f32.powf(db.clamp(-60.0, 12.0) / 20.0) } else { 1.0 };
        self.shared.metro.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
    /// A record count-in under way: (samples left, whole length) in project samples.
    pub fn count_in(&self) -> Option<(Samples, Samples)> {
        let left = self.shared.metro.count_in_left.load(Ordering::Relaxed);
        (left > 0).then(|| (left, self.shared.metro.count_in_total.load(Ordering::Relaxed)))
    }
    /// Why the recording start at the end of the last count-in failed, once; `None` when it did
    /// not. The transport stays parked and nothing records.
    pub fn take_start_failure(&self) -> Option<super::audio::StartFailure> {
        super::audio::StartFailure::from_code(self.shared.metro.start_failure.swap(0, Ordering::Relaxed))
    }
    /// Report a failed count-in start as the audio thread would, for device-free tests.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn report_start_failure(&self, failure: super::audio::StartFailure) {
        self.shared.metro.start_failure.store(failure as u8, Ordering::Relaxed);
    }
    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }
    pub fn copy_meters(&self, out: &mut MeterSnapshot) -> Result<(), PlayerError> {
        self.owner()?.copy_meters(out);
        Ok(())
    }
    /// The scope taps of the open device's plans; `None` while the player is busy or faulted.
    pub fn scope_hub(&self) -> Option<Arc<soundcraft_mix::ScopeHub>> {
        self.owner().ok().map(|o| o.scope_hub())
    }
    /// Switches the scope tap of an insert (by `Insert.uid`) on or off, with no recompile; false when
    /// the insert has no tap yet.
    pub fn scope_enable(&self, track: TrackId, uid: u64, on: bool) -> bool {
        self.scope_hub().is_some_and(|h| h.set_enabled(track, uid, on))
    }
    /// Copies the last `out.len()` frames of an insert's scope tap into `out` (see
    /// [`soundcraft_mix::ScopeTap::read`]); 0 while there is no tap.
    pub fn scope_frames(&self, track: TrackId, uid: u64, out: &mut [f32]) -> usize {
        self.scope_hub().map_or(0, |h| h.frames(track, uid, out))
    }
    /// The gain reduction in dB an insert last reported to its tap.
    pub fn scope_gain_reduction_db(&self, track: TrackId, uid: u64) -> f32 {
        self.scope_hub().map_or(0.0, |h| h.gain_reduction_db(track, uid))
    }
    pub fn meters(&self) -> Result<MeterSnapshot, PlayerError> {
        let mut out = MeterSnapshot::default();
        self.copy_meters(&mut out)?;
        Ok(out)
    }
    pub fn thread_priority(&self) -> scheduling::WorkerPriority {
        self.stream.as_ref().map_or(scheduling::WorkerPriority::NoWorker, |stream| stream.thread_priority())
    }
    /// Start the since-open peak load over; the output callback applies it at its next block.
    pub fn reset_peak_load(&self) {
        self.fault.telemetry.reset_peak();
    }
    pub fn callback_telemetry(&self) -> telemetry::CallbackTelemetry {
        self.fault.telemetry.snapshot()
    }
    pub fn clock_report(&self) -> Option<device_clock::ClockReport> {
        self.clock.try_borrow_mut().ok()?.as_mut().map(device_clock::MaintainedClock::report)
    }
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = self
            .watch
            .try_borrow_mut()
            .is_ok_and(|mut seen| crate::stalled(&mut seen, self.fault.beats.load(Ordering::Relaxed), self.is_playing() && !self.silent));
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        let trouble = self.fault.trouble(stalled);
        if trouble.is_some()
            && let Ok(mut owner) = self.owner()
        {
            owner.fault();
        }
        trouble
    }
    pub fn open_input(&self, config: &AudioConfig) -> Result<record::Recorder, String> {
        let mut input = self.input.try_borrow_mut().map_err(|_| "input transition is busy")?;
        match &mut *input {
            HeldInput::No => record::Recorder::open_with(config),
            HeldInput::Held(slot) => {
                let recorder = slot.take().ok_or("audio input is already open")?;
                recorder.monitor.clear_monitor();
                Ok(recorder)
            }
            HeldInput::Failed(error) => Err(error.clone()),
        }
    }
    /// On refusal the caller retains its recorder, including the device handle.
    pub fn close_input(&self, recorder: &mut Option<record::Recorder>) -> Result<(), PlayerError> {
        let mut input = self.input.try_borrow_mut().map_err(|_| PlayerError::Busy)?;
        if matches!(*input, HeldInput::Held(Some(_))) && recorder.is_some() {
            return Err(PlayerError::Busy);
        }
        if recorder.as_ref().is_some_and(record::Recorder::has_pending_take) {
            return Err(PlayerError::RecordingActive);
        }
        let Some(recorder) = recorder.take() else { return Ok(()) };
        if let HeldInput::Held(slot @ None) = &mut *input {
            *slot = Some(recorder);
        }
        Ok(())
    }
}

/// What the Player says when the native backend did not open and cpal did.
#[cfg(any(test, all(windows, feature = "native-wasapi")))]
pub(crate) fn native_fallback_note(reason: &str) -> String {
    format!("Native WASAPI failed: {reason}; using the cpal backend")
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(anchor) = &self.state {
            anchor.close();
        }
        #[cfg(not(target_arch = "wasm32"))]
        drop(self.midi.get_mut().take());
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.silent_stop.store(true, Ordering::Release);
            if let Some(worker) = &self.silent_worker {
                worker.thread().unpark();
            }
        }
        if let Some(stream) = &self.stream {
            let _ = stream.stop();
        }
        // Admission, not the backend's pause semantics, establishes quiescence.
        if let Some(anchor) = &self.state {
            anchor.quiesce();
        }
        // Retain the anchor through ordinary stream/worker teardown. A backend
        // may retain its callback even after a failed build, so endpoint loss is
        // not a prerequisite for reclaiming Audio on Main after quiescence.
        let input = std::mem::replace(self.input.get_mut(), HeldInput::No);
        drop(input);
        drop(self.stream.take());
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(worker) = self.silent_worker.take() {
            let _ = worker.join();
        }
        // Retained endpoints can only produce silence. Their eventual teardown
        // may free the empty handoff allocation, outside timed rendering; it
        // cannot destroy Audio, plug-ins or retained panic payloads.
        let audio = self.state.take().map(Anchor::take);
        if let (Some(owner), Some(audio)) = (self.owner.get_mut().take(), audio) {
            owner.retire(audio.into_ports());
        }
    }
}

fn render_held(
    access: &mut AudioAccess<Audio>,
    fault: &Fault,
    shared: &Shared,
    out: &mut [f32],
    channels: usize,
    plans: &mut plan_bridge::CallbackBudget,
    actions: &mut action_lane::Budget,
) -> bool {
    let Some(mut audio) = access.enter() else {
        fault.telemetry.unavailable_silence_frames.fetch_add((out.len() / channels.max(1)) as u64, Ordering::Relaxed);
        out.fill(0.0);
        return false;
    };
    if fault.failed.load(Ordering::Acquire) || fault.gone.load(Ordering::Acquire) {
        audio.fault();
    }
    if audio.fill(out, channels, plans, actions).is_err() {
        fault.failed.store(true, Ordering::Release);
    }
    if fault.failed.load(Ordering::Acquire) || fault.gone.load(Ordering::Acquire) {
        shared.playing.store(false, Ordering::Relaxed);
        fault.telemetry.fault_silence_frames.fetch_add((out.len() / channels.max(1)) as u64, Ordering::Relaxed);
    }
    true
}

/// The real backend and silent worker share this preallocated render adapter.
/// Its sole endpoint borrows Audio; the Player's anchor remains its final owner.
struct DeviceOutput {
    access: AudioAccess<Audio>,
    channels: usize,
    timing: telemetry::Accumulator,
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// Typed device buffers in tests convert through the scratch of the adapter; real streams
    /// convert inside their adapter.
    #[cfg(test)]
    convert: crate::OutputCallback,
}

impl DeviceOutput {
    fn new(access: AudioAccess<Audio>, player: &Player, channels: usize) -> Self {
        // Initialise the platform clock (including any frequency cache) before callbacks run.
        let _ = telemetry::Timer::start().elapsed();
        Self {
            access,
            channels: channels.max(1),
            timing: telemetry::Accumulator::default(),
            shared: Arc::clone(&player.shared),
            fault: Arc::clone(&player.fault),
            #[cfg(test)]
            convert: crate::OutputCallback::new(channels),
        }
    }

    /// One device callback: the callback scope is shared by every conversion chunk of the
    /// block; it must never be reconstructed inside a chunk.
    fn render(&mut self, block: &mut ab::RenderBlock<'_>) {
        let mut plans = plan_bridge::callback_scope();
        let mut actions = action_lane::callback_scope();
        let (access, fault, shared, channels) = (&mut self.access, &*self.fault, &*self.shared, self.channels);
        block.render(&mut |buffer| {
            render_held(access, fault, shared, buffer, channels, &mut plans, &mut actions);
        });
    }

    /// The same callback scope over `f32` samples with no device conversion (the silent clock).
    #[cfg(any(not(target_arch = "wasm32"), feature = "test-hooks"))]
    fn fill_f32(&mut self, data: &mut [f32]) {
        let mut plans = plan_bridge::callback_scope();
        let mut actions = action_lane::callback_scope();
        for buffer in data.chunks_mut(ab::SCRATCH_FRAMES * self.channels) {
            render_held(&mut self.access, &self.fault, &self.shared, buffer, self.channels, &mut plans, &mut actions);
        }
    }

    /// A typed device buffer through the conversion of the adapter.
    #[cfg(test)]
    fn fill<T: crate::backend::cpal::Sample>(&mut self, data: &mut [T]) {
        let mut plans = plan_bridge::callback_scope();
        let mut actions = action_lane::callback_scope();
        let (access, fault, shared, channels) = (&mut self.access, &*self.fault, &*self.shared, self.channels);
        self.convert.convert(data, &mut |buffer| {
            render_held(access, fault, shared, buffer, channels, &mut plans, &mut actions);
        });
    }
}

/// The render and error callbacks the backend runs on its audio thread.
fn render_callbacks(
    player: &Player,
    access: AudioAccess<Audio>,
    mut clock: device_clock::CallbackClock,
    channels: usize,
    rate: u32,
) -> (ab::RenderCallback, ab::ErrorCallback) {
    let mut output = DeviceOutput::new(access, player, channels);
    let fault = Arc::clone(&player.fault);
    let error = error_callback(Arc::clone(&player.fault), Arc::clone(&player.shared), player.failure_signal.clone());
    let render = move |block: &mut ab::RenderBlock<'_>| {
        crate::mark_audio_thread();
        let started = telemetry::Timer::start();
        let creating_before = soundcraft_mix::creating_instance();
        let frames = block.frames;
        clock.observer.record(frames, block.stamp);
        fault.beats.fetch_add(1, Ordering::Relaxed);
        output.render(block);
        let creating = creating_before || soundcraft_mix::creating_instance();
        output.timing.record_overlap(&fault.telemetry, frames, rate, started.elapsed(), creating);
    };
    (Box::new(render), Box::new(error))
}

/// Virtual fallback pacing, not a physical device clock. Keep sub-frame time
/// and bounded-work backlog rather than rounding/dropping them on every wake.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct SilentPacer {
    scaled_nanoseconds: u128,
}
#[cfg(not(target_arch = "wasm32"))]
impl SilentPacer {
    fn advance(&mut self, elapsed: std::time::Duration, rate: u32, limit: usize) -> Option<usize> {
        let added = elapsed.as_nanos().checked_mul(u128::from(rate))?;
        let total = self.scaled_nanoseconds.checked_add(added)?;
        let frames = (total / 1_000_000_000).min(limit as u128) as usize;
        self.scaled_nanoseconds = total.checked_sub((frames as u128).checked_mul(1_000_000_000)?)?;
        Some(frames)
    }
}

/// Registered directly with the backend. Device loss must revoke admission even
/// when no further render callback or Main service occurs. No logging/formatting.
fn error_callback(
    fault: Arc<Fault>,
    shared: Arc<Shared>,
    signal: Option<plan_bridge::RuntimeIdentity>,
) -> impl FnMut(ab::StreamError) + Send + 'static {
    move |error| {
        crate::mark_audio_thread();
        let gone = matches!(error, ab::StreamError::DeviceNotAvailable);
        if gone {
            if let Some(signal) = &signal {
                signal.fault();
            }
            fault.gone.store(true, Ordering::Release);
            shared.playing.store(false, Ordering::Release);
        }
        fault.telemetry.error(gone);
    }
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
