//! Device facade for the prepared runtime. All factories, editors and reclamation
//! belong to this creating-thread owner; the device borrows its anchored audio state.
use super::audio::{ActionResult, Audio, Command, CountIn};
use super::{Notice, PlayerError, RuntimeControl, ServiceReport, SessionRequestId, sources};
use crate::{AudioConfig, EditorEdit, Fault, HeldInput, MeterSnapshot, OutputCallback, OutputTrouble, PluginStates, Shared};
use crate::{action_lane, device_clock, devices, plan_bridge, record, scheduling, telemetry};
use soundcraft_mix::{Arrival, DeviceGeneration, Due, EngineEpoch, InstrumentEvent, Message, NoteId, PlanLimits, RenderTarget, Source};
use soundcraft_model::{IoMap, Session, TrackId};
use soundcraft_rt::handoff::{Anchor, AudioAccess};
use soundcraft_time::{Range, Samples};
use std::cell::{Cell, RefCell, RefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type Owner = RuntimeControl<Command, ActionResult>;
pub type PlayerNotice = Notice<Command, ActionResult>;

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
    state: Option<Anchor<Audio>>,
    #[cfg(feature = "test-hooks")]
    test_output: RefCell<Option<DeviceOutput>>,
    stream: Option<cpal::Stream>,
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
            let output = DeviceOutput::new(access, &player, 2, rate);
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
            output.fill(&mut [0.0f32; 1024]);
        }
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
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
        let (anchor, access) = Anchor::new(audio);
        player.state = Some(anchor);
        player.failure_signal = Some(owner.failure_signal());
        *player.owner.get_mut() = Some(owner);
        player.audition = Some(audition);
        Ok((player, access, callback_clock))
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig) -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, StreamTrait};
        let want_ch = u16::try_from(soundcraft_mix::main_channels(&session)).unwrap_or(2);
        let (device, config, format, name, system) = crate::open_device(session.sample_rate.hz(), want_ch, cfg)?;
        let duplex = crate::input_with_output(system, || device.default_input_config().is_ok_and(|c| record::input_usable(&c)));
        // Every output the stream opened, and the duplex device's inputs, get default paths; the
        // app replaces them with this machine's saved table (`set_io`).
        let inputs = if duplex { record::input_channels_on(&device, config.sample_rate.0) } else { 0 };
        let wide = config.channels;
        let attempts = crate::output_attempts(&config, want_ch);
        let mut error = "no usable output stream".to_owned();
        for config in attempts {
            let io = IoMap::for_device(system, &name, inputs, config.channels);
            let (mut player, access, clock) = Self::prepare(Arc::clone(&session), config.sample_rate.0, io)?;
            *player.input.get_mut() = if duplex {
                match record::Recorder::build_on(&device, config.sample_rate.0, config.buffer_size) {
                    Ok(recorder) => HeldInput::Held(Some(recorder)),
                    Err(error) => HeldInput::Failed(format!("the audio input did not open with the output ({error})")),
                }
            } else if system == devices::ASIO_SYSTEM {
                HeldInput::Failed("the ASIO output has no usable input".into())
            } else {
                HeldInput::No
            };
            let built = match format {
                cpal::SampleFormat::I8 => build_output::<i8>(&device, &config, &player, access, clock),
                cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, &player, access, clock),
                cpal::SampleFormat::I24 => build_output::<cpal::I24>(&device, &config, &player, access, clock),
                cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, &player, access, clock),
                cpal::SampleFormat::I64 => build_output::<i64>(&device, &config, &player, access, clock),
                cpal::SampleFormat::U8 => build_output::<u8>(&device, &config, &player, access, clock),
                cpal::SampleFormat::U16 => build_output::<u16>(&device, &config, &player, access, clock),
                cpal::SampleFormat::U32 => build_output::<u32>(&device, &config, &player, access, clock),
                cpal::SampleFormat::U64 => build_output::<u64>(&device, &config, &player, access, clock),
                cpal::SampleFormat::F32 => build_output::<f32>(&device, &config, &player, access, clock),
                cpal::SampleFormat::F64 => build_output::<f64>(&device, &config, &player, access, clock),
                _ => return Err(format!("unsupported device sample format: {format}")),
            };
            let stream = match built {
                Ok(stream) => stream,
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
            let started = player.stream.as_ref().ok_or("output stream disappeared")?.play();
            if let Err(failure) = started {
                error = failure.to_string();
                continue;
            }
            log::info!("audio output opened with {} channels (asked for {})", config.channels, wide);
            player.device_name = name.clone();
            player.device_system = system.to_string();
            player.device_channels = config.channels;
            player.device_buffer = match config.buffer_size {
                cpal::BufferSize::Fixed(n) => Some(n),
                cpal::BufferSize::Default => None,
            };
            player.silent = false;
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(midi) = player.midi.get_mut() {
                midi.enable(soundcraft_midi_io::MidiIo::new("transitDAW"));
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
        let mut callback = DeviceOutput::new(access, self, 2, rate);
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
                    callback.fill(data);
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
        let result = self.owner()?.send_action(Command::Record { ticket, from, end, looped, count_in }, true);
        if result.is_err() {
            self.cancel_record_start()?;
        }
        result
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
    pub fn meters(&self) -> Result<MeterSnapshot, PlayerError> {
        let mut out = MeterSnapshot::default();
        self.copy_meters(&mut out)?;
        Ok(out)
    }
    pub fn thread_priority(&self) -> scheduling::WorkerPriority {
        scheduling::observe(self.stream.as_ref())
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

impl Drop for Player {
    fn drop(&mut self) {
        use cpal::traits::StreamTrait;
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
            let _ = stream.pause();
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
    output: OutputCallback,
    shared: Arc<Shared>,
    fault: Arc<Fault>,
}

impl DeviceOutput {
    fn new(access: AudioAccess<Audio>, player: &Player, channels: usize, rate: u32) -> Self {
        Self { access, output: OutputCallback::new(channels, rate), shared: Arc::clone(&player.shared), fault: Arc::clone(&player.fault) }
    }

    fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(&mut self, data: &mut [T]) {
        let channels = self.output.channels;
        self.output.fill_with(data, |buffer, plans, actions| {
            render_held(&mut self.access, &self.fault, &self.shared, buffer, channels, plans, actions);
        });
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    player: &Player,
    access: AudioAccess<Audio>,
    mut clock: device_clock::CallbackClock,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let channels = usize::from(config.channels).max(1);
    let rate = config.sample_rate.0;
    let mut output = DeviceOutput::new(access, player, channels, rate);
    let fault = Arc::clone(&player.fault);
    let error = error_callback(Arc::clone(&player.fault), Arc::clone(&player.shared), player.failure_signal.clone());
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            crate::mark_audio_thread();
            let started = telemetry::Timer::start();
            let creating_before = soundcraft_mix::creating_instance();
            let frames = data.len() / channels;
            clock.cpal.output(frames, info, clock.origin.now_ns());
            fault.beats.fetch_add(1, Ordering::Relaxed);
            output.fill(data);
            let creating = creating_before || soundcraft_mix::creating_instance();
            output.output.timing.record_overlap(&fault.telemetry, frames, rate, started.elapsed(), creating);
        },
        error,
        None,
    )
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
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |error| {
        crate::mark_audio_thread();
        let gone = matches!(error, cpal::StreamError::DeviceNotAvailable);
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
mod tests;
