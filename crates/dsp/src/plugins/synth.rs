//! Subtractive Synth: 16-voice polyphonic, two PolyBLEP oscillators, resonant state-variable
//! low-pass (TPT form) with its own ADSR, amp ADSR, optional glide.

use crate::event::{CC_ALL_NOTES_OFF, CC_ALL_SOUND_OFF, CC_RESET_CONTROLLERS, CC_SOSTENUTO, CC_SUSTAIN, Message, PluginEvent};
use crate::osc::{Osc, Waveform};
use crate::params::{Params, choice, lin, log, param_plumbing};
use crate::util::{Smoother, frames_in, ms_to_samples, note_hz, sane_channels, sane_sr, sort_events};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Unit, db_to_gain};

/// Capacity of a built-in instrument's event queue.
pub(crate) const MAX_EVENTS: usize = 1024;
/// Queue entries kept for releases: one per note and pedal entry the mix can hold open (224 +
/// 32), plus one reset (F10). Other events are refused once only these remain.
pub(crate) const RELEASE_RESERVE: usize = 257;

#[derive(Debug, Clone, Copy)]
pub(crate) enum EventKind {
    /// Channel, key, velocity, voice id (-1: none).
    On(u8, u8, u8, i32),
    /// Channel, key, voice id (-1: match by channel and key).
    Off(u8, u8, i32),
    AllOff,
    /// Channel, down.
    Sustain(u8, bool),
    /// Channel, down.
    Sostenuto(u8, bool),
    /// Channel, pitch bend as a frequency ratio.
    Bend(u8, f32),
    /// Channel.
    ResetControllers(u8),
}

impl EventKind {
    /// What must never be refused: releases (see [`Message::is_release`]).
    fn is_release(&self) -> bool {
        matches!(
            self,
            EventKind::Off(..) | EventKind::AllOff | EventKind::Sustain(_, false) | EventKind::Sostenuto(_, false) | EventKind::ResetControllers(_)
        )
    }

    /// The built-in instruments' reading of a plug-in event: notes, pedals, bend and the
    /// channel-mode messages. Anything else is `None` (ignored).
    pub(crate) fn from_event(ev: &PluginEvent) -> Option<EventKind> {
        let ch = ev.channel & 0x0F;
        Some(match ev.message {
            Message::NoteOn { key, velocity, .. } => EventKind::On(ch, key.min(127), velocity.min(127), ev.note_id),
            Message::NoteOff { key, .. } => EventKind::Off(ch, key.min(127), ev.note_id),
            Message::Control { cc, value } => EventKind::from_cc(ch, cc, value)?,
            Message::PitchBend { value } => EventKind::bend(ch, value),
            _ => return None,
        })
    }

    fn from_cc(ch: u8, cc: u8, value: u8) -> Option<EventKind> {
        Some(match cc {
            CC_SUSTAIN => EventKind::Sustain(ch, value >= 64),
            CC_SOSTENUTO => EventKind::Sostenuto(ch, value >= 64),
            CC_ALL_SOUND_OFF | CC_ALL_NOTES_OFF => EventKind::AllOff,
            CC_RESET_CONTROLLERS => EventKind::ResetControllers(ch),
            _ => return None,
        })
    }

    fn bend(ch: u8, value: u16) -> EventKind {
        let v = i32::from(value.min(16383)) - 8192;
        EventKind::Bend(ch, 2f32.powf(v as f32 / 8192.0 * BEND_RANGE / 12.0))
    }

    /// A MIDI 1.0 channel message (the legacy [`Plugin::midi`] path).
    pub(crate) fn from_midi(msg: [u8; 3]) -> Option<EventKind> {
        let [status, d1, d2] = msg;
        let ch = status & 0x0F;
        match status & 0xF0 {
            0xB0 => EventKind::from_cc(ch, d1 & 0x7F, d2 & 0x7F),
            0xE0 => Some(EventKind::bend(ch, u16::from(d2 & 0x7F) << 7 | u16::from(d1 & 0x7F))),
            _ => None,
        }
    }
}

/// Pitch bend range, semitones each way.
const BEND_RANGE: f32 = 2.0;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Event {
    pub(crate) offset: usize,
    pub(crate) kind: EventKind,
}

/// Fixed-capacity event queue (never reallocates on the audio thread). The last
/// [`RELEASE_RESERVE`] entries only take releases, so a note-off is never refused while notes
/// fill the queue.
#[derive(Debug)]
pub(crate) struct EventQueue {
    ev: Vec<Event>,
}

impl EventQueue {
    pub(crate) fn new() -> Self {
        Self { ev: Vec::with_capacity(MAX_EVENTS) }
    }

    /// Queues an event; `false` when it was refused (never logs: this runs on the audio thread).
    pub(crate) fn push(&mut self, offset: usize, kind: EventKind) -> bool {
        let room = if kind.is_release() { MAX_EVENTS } else { MAX_EVENTS - RELEASE_RESERVE };
        if self.ev.len() < room {
            self.ev.push(Event { offset, kind });
            true
        } else {
            false
        }
    }

    /// Takes the queued events, sorted by offset (stable), leaving the queue empty. Give the
    /// vector back with [`EventQueue::restore`] so its capacity is reused (no allocation).
    pub(crate) fn take_sorted(&mut self) -> Vec<Event> {
        let mut ev = std::mem::take(&mut self.ev);
        sort_events(&mut ev, |e| e.offset);
        ev
    }

    /// Hands back the vector [`EventQueue::take_sorted`] took, emptied (`apply` never queues, so
    /// nothing was queued meanwhile).
    pub(crate) fn restore(&mut self, mut ev: Vec<Event>) {
        ev.clear();
        self.ev = ev;
    }

    pub(crate) fn clear(&mut self) {
        self.ev.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ev.len()
    }
}

/// Runs a block's events against `frames` samples: `apply` for each event when its offset is
/// reached, `render` once per sample. Events past the block take effect at its last sample;
/// for an empty block, at once.
pub(crate) fn run_block<P>(
    p: &mut P,
    events: &[Event],
    frames: usize,
    mut apply: impl FnMut(&mut P, EventKind),
    mut render: impl FnMut(&mut P, usize),
) {
    let mut next = 0;
    for n in 0..frames {
        while let Some(e) = events.get(next) {
            if e.offset > n && n + 1 < frames {
                break;
            }
            apply(p, e.kind);
            next += 1;
        }
        render(p, n);
    }
    for e in events.iter().skip(next) {
        apply(p, e.kind);
    }
}

const WAVES: &[&str] = &["Saw", "Square", "Sine", "Triangle"];

fn wave(i: usize) -> Waveform {
    match i {
        1 => Waveform::Square,
        2 => Waveform::Sine,
        3 => Waveform::Triangle,
        _ => Waveform::Saw,
    }
}

static SYNTH_PARAMS: [ParamInfo; 19] = [
    choice("osc1_wave", "Osc 1", WAVES, 0),
    choice("osc2_wave", "Osc 2", WAVES, 0),
    lin("osc2_semitones", "Osc 2 Pitch", -24.0, 24.0, 0.0, Unit::Semitones),
    lin("osc2_detune", "Osc 2 Detune", -100.0, 100.0, 7.0, Unit::Cents),
    lin("osc_mix", "Osc Mix", 0.0, 100.0, 50.0, Unit::Percent),
    log("cutoff", "Cutoff", 20.0, 20000.0, 2000.0, Unit::Hz),
    lin("resonance", "Resonance", 0.0, 100.0, 20.0, Unit::Percent),
    lin("filter_env_amount", "Filter Env", -100.0, 100.0, 30.0, Unit::Percent),
    log("amp_attack", "Amp Attack", 0.5, 5000.0, 5.0, Unit::Ms),
    log("amp_decay", "Amp Decay", 1.0, 5000.0, 200.0, Unit::Ms),
    lin("amp_sustain", "Amp Sustain", 0.0, 100.0, 70.0, Unit::Percent),
    log("amp_release", "Amp Release", 1.0, 10000.0, 300.0, Unit::Ms),
    log("filter_attack", "Filter Attack", 0.5, 5000.0, 5.0, Unit::Ms),
    log("filter_decay", "Filter Decay", 1.0, 5000.0, 300.0, Unit::Ms),
    lin("filter_sustain", "Filter Sustain", 0.0, 100.0, 30.0, Unit::Percent),
    log("filter_release", "Filter Release", 1.0, 10000.0, 300.0, Unit::Ms),
    lin("glide", "Glide", 0.0, 1000.0, 0.0, Unit::Ms),
    lin("velocity", "Velocity Sens", 0.0, 100.0, 70.0, Unit::Percent),
    lin("level", "Level", -48.0, 6.0, -6.0, Unit::Db),
];

pub static SYNTH_INFO: PluginInfo = PluginInfo {
    id: "subtractive_synth",
    name: "Subtractive Synth",
    short_name: "SubSynth",
    category: Category::Instrument,
    params: &SYNTH_PARAMS,
    is_instrument: true,
    audiosuite: false,
};

const VOICES: usize = 16;
const SILENT: f32 = 1.0e-5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Linear-attack, exponential decay/release ADSR. Times are "to -60 dB" for decay/release.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Adsr {
    pub(crate) stage: Stage,
    pub(crate) level: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AdsrRates {
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
}

impl AdsrRates {
    pub(crate) fn new(a_ms: f32, d_ms: f32, s: f32, r_ms: f32, sr: f32) -> Self {
        let exp60 = |ms: f32| (0.001f32.ln() / ms_to_samples(ms, sr).max(1.0)).exp();
        Self { attack: 1.0 / ms_to_samples(a_ms, sr).max(1.0), decay: exp60(d_ms), sustain: s.clamp(0.0, 1.0), release: exp60(r_ms) }
    }
}

impl Adsr {
    pub(crate) const fn new() -> Self {
        Self { stage: Stage::Idle, level: 0.0 }
    }

    pub(crate) fn gate_on(&mut self) {
        self.stage = Stage::Attack;
    }

    pub(crate) fn gate_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    #[inline]
    pub(crate) fn next(&mut self, r: &AdsrRates) -> f32 {
        match self.stage {
            Stage::Idle => self.level = 0.0,
            Stage::Attack => {
                self.level += r.attack;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = r.sustain + (self.level - r.sustain) * r.decay;
                if (self.level - r.sustain).abs() < 1e-4 {
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = r.sustain,
            Stage::Release => {
                self.level *= r.release;
                if self.level < SILENT {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

/// Topology-preserving-transform state-variable low-pass.
#[derive(Debug, Clone, Copy, Default)]
struct Svf {
    ic1: f32,
    ic2: f32,
}

impl Svf {
    #[inline]
    fn lowpass(&mut self, x: f32, g: f32, k: f32) -> f32 {
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        let v3 = x - self.ic2;
        let v1 = a1 * self.ic1 + a2 * v3;
        let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
        self.ic1 = crate::util::flush(2.0 * v1 - self.ic1);
        self.ic2 = crate::util::flush(2.0 * v2 - self.ic2);
        if v2.is_finite() {
            v2
        } else {
            *self = Svf::default();
            0.0
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    note: u8,
    vel: f32,
    freq: f32,
    target: f32,
    osc1: Osc,
    osc2: Osc,
    amp: Adsr,
    filt: Adsr,
    svf: Svf,
    age: u64,
    held: bool,
    /// Released while a pedal held it (sustain, or sostenuto when `latched`).
    sustained: bool,
    /// MIDI channel and host voice id (-1: none).
    ch: u8,
    id: i32,
    /// Held when its channel's sostenuto pedal went down: it sounds on until the pedal comes up.
    latched: bool,
}

impl Voice {
    fn new(seed: u32) -> Self {
        Self {
            note: 0,
            vel: 0.0,
            freq: 440.0,
            target: 440.0,
            osc1: Osc::new(seed),
            osc2: Osc::new(seed.wrapping_mul(7919)),
            amp: Adsr::new(),
            filt: Adsr::new(),
            svf: Svf::default(),
            age: 0,
            held: false,
            sustained: false,
            ch: 0,
            id: -1,
            latched: false,
        }
    }

    fn active(&self) -> bool {
        self.amp.stage != Stage::Idle
    }
}

/// Polyphonic subtractive synthesizer.
pub struct SubtractiveSynth {
    p: Params,
    sr: f32,
    ch: usize,
    voices: [Voice; VOICES],
    events: EventQueue,
    amp_rates: AdsrRates,
    filt_rates: AdsrRates,
    glide_coef: f32,
    last_freq: Option<f32>,
    counter: u64,
    level: Smoother,
    cutoff: Smoother,
    /// Per MIDI channel: the sustain and sostenuto pedals and the pitch bend (a frequency ratio).
    sustain: [bool; 16],
    sostenuto: [bool; 16],
    bend: [f32; 16],
}

impl Default for SubtractiveSynth {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtractiveSynth {
    pub fn new() -> Self {
        let mut voices = [Voice::new(1); VOICES];
        for (i, v) in voices.iter_mut().enumerate() {
            *v = Voice::new(0x1000 + i as u32 * 31);
        }
        let mut s = Self {
            p: Params::new(&SYNTH_PARAMS),
            sr: 48_000.0,
            ch: 0,
            voices,
            events: EventQueue::new(),
            amp_rates: AdsrRates::default(),
            filt_rates: AdsrRates::default(),
            glide_coef: 0.0,
            last_freq: None,
            counter: 0,
            level: Smoother::new(0.5),
            cutoff: Smoother::new(2000.0),
            sustain: [false; 16],
            sostenuto: [false; 16],
            bend: [1.0; 16],
        };
        s.update();
        s
    }

    fn update(&mut self) {
        let sr = self.sr;
        let v = |i| self.p.v(i);
        self.amp_rates = AdsrRates::new(v(8), v(9), v(10) / 100.0, v(11), sr);
        self.filt_rates = AdsrRates::new(v(12), v(13), v(14) / 100.0, v(15), sr);
        let glide = v(16);
        self.glide_coef = if glide < 0.5 { 0.0 } else { crate::util::time_coef(glide / 3.0, sr) };
        self.level.set(db_to_gain(v(18)));
        self.cutoff.set(v(5));
    }

    fn start_note(&mut self, ch: u8, note: u8, vel: u8, id: i32) {
        if vel == 0 {
            self.stop_note(ch, note, id);
            return;
        }
        self.counter += 1;
        let freq = note_hz(f32::from(note.min(127)));
        // Reuse a voice already playing this note on this channel (with a voice id only a
        // releasing one: another id on the same key is another voice), else a free one, else
        // steal.
        let idx = self
            .voices
            .iter()
            .position(|v| v.active() && v.ch == ch && v.note == note && (id < 0 || !v.held))
            .or_else(|| self.voices.iter().position(|v| !v.active()))
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let ka = (a.held, a.amp.level);
                        let kb = (b.held, b.amp.level);
                        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        let sens = self.p.v(17) / 100.0;
        let glide = self.glide_coef > 0.0;
        let start = if glide { self.last_freq.unwrap_or(freq) } else { freq };
        let counter = self.counter;
        if let Some(v) = self.voices.get_mut(idx) {
            let was_active = v.active();
            v.note = note;
            v.vel = (1.0 - sens) + sens * f32::from(vel.min(127)) / 127.0;
            v.target = freq;
            v.freq = if was_active && glide { v.freq } else { start };
            if !was_active {
                v.osc1.reset();
                v.osc2.phase = 0.37;
                v.svf = Svf::default();
            }
            v.amp.gate_on();
            v.filt.gate_on();
            v.age = counter;
            v.held = true;
            v.sustained = false;
            v.latched = false;
            v.ch = ch;
            v.id = id;
        }
        self.last_freq = Some(freq);
    }

    /// Releases the voice with host id `id`, or without an id the held voices playing `note` on
    /// channel `ch`. A pedal keeps it sounding.
    fn stop_note(&mut self, ch: u8, note: u8, id: i32) {
        let sustain = self.sustain.get(usize::from(ch & 0x0F)).copied().unwrap_or(false);
        for v in self.voices.iter_mut().filter(|v| v.held && v.ch == ch && if id >= 0 { v.id == id } else { v.note == note }) {
            v.held = false;
            if sustain || v.latched {
                v.sustained = true;
            } else {
                v.amp.gate_off();
                v.filt.gate_off();
            }
        }
    }

    /// Releases channel `ch`'s pedal-held voices that no pedal holds any more.
    fn release_pedalled(&mut self, ch: u8) {
        let sustain = self.sustain.get(usize::from(ch & 0x0F)).copied().unwrap_or(false);
        for v in self.voices.iter_mut().filter(|v| v.ch == ch && v.sustained && !sustain && !v.latched) {
            v.sustained = false;
            v.amp.gate_off();
            v.filt.gate_off();
        }
    }

    fn set_sustain(&mut self, ch: u8, on: bool) {
        if let Some(s) = self.sustain.get_mut(usize::from(ch & 0x0F)) {
            *s = on;
        }
        if !on {
            self.release_pedalled(ch);
        }
    }

    /// Sostenuto down latches the voices held on the channel at that moment; up lets them go.
    fn set_sostenuto(&mut self, ch: u8, on: bool) {
        let Some(s) = self.sostenuto.get_mut(usize::from(ch & 0x0F)) else { return };
        if *s == on {
            return;
        }
        *s = on;
        for v in self.voices.iter_mut().filter(|v| v.ch == ch) {
            v.latched = on && v.held;
        }
        if !on {
            self.release_pedalled(ch);
        }
    }

    fn apply(&mut self, kind: EventKind) {
        match kind {
            EventKind::On(c, n, v, id) => self.start_note(c, n, v, id),
            EventKind::Off(c, n, id) => self.stop_note(c, n, id),
            EventKind::AllOff => {
                for v in &mut self.voices {
                    v.held = false;
                    v.sustained = false;
                    v.latched = false;
                    v.amp.gate_off();
                    v.filt.gate_off();
                }
            }
            EventKind::Sustain(c, on) => self.set_sustain(c, on),
            EventKind::Sostenuto(c, on) => self.set_sostenuto(c, on),
            EventKind::Bend(c, ratio) => {
                if let Some(b) = self.bend.get_mut(usize::from(c & 0x0F)) {
                    *b = ratio;
                }
            }
            EventKind::ResetControllers(c) => {
                self.set_sustain(c, false);
                self.set_sostenuto(c, false);
                if let Some(b) = self.bend.get_mut(usize::from(c & 0x0F)) {
                    *b = 1.0;
                }
            }
        }
    }

    #[inline]
    fn render_sample(&mut self) -> f32 {
        let sr = self.sr;
        let w1 = wave(self.p.choice(0));
        let w2 = wave(self.p.choice(1));
        let ratio2 = 2f32.powf((self.p.v(2) + self.p.v(3) / 100.0) / 12.0);
        let mix = self.p.v(4) / 100.0;
        let k = 2.0 - 1.95 * (self.p.v(6) / 100.0);
        let env_oct = self.p.v(7) / 100.0 * 6.0;
        let cutoff = self.cutoff.next();
        let nyq = sr * 0.45;
        let mut out = 0.0;
        for v in &mut self.voices {
            if !v.active() {
                continue;
            }
            if self.glide_coef > 0.0 {
                v.freq = v.target + (v.freq - v.target) * self.glide_coef;
            } else {
                v.freq = v.target;
            }
            let bend = self.bend.get(usize::from(v.ch & 0x0F)).copied().unwrap_or(1.0);
            let a = v.amp.next(&self.amp_rates);
            let fe = v.filt.next(&self.filt_rates);
            let s1 = v.osc1.next(w1, v.freq * bend / sr);
            let s2 = v.osc2.next(w2, v.freq * bend * ratio2 / sr);
            let raw = s1 * (1.0 - mix) + s2 * mix;
            let fc = (cutoff * 2f32.powf(env_oct * fe)).clamp(20.0, nyq);
            let g = (std::f32::consts::PI * fc / sr).tan();
            let y = v.svf.lowpass(raw, g, k);
            out += y * a * v.vel;
        }
        out * 0.3
    }
}

impl Plugin for SubtractiveSynth {
    param_plumbing!(SYNTH_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.level.set_time(20.0, self.sr);
        self.cutoff.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        for (i, v) in self.voices.iter_mut().enumerate() {
            *v = Voice::new(0x1000 + i as u32 * 31);
        }
        self.events.clear();
        self.last_freq = None;
        self.level.snap();
        self.cutoff.snap();
        self.sustain = [false; 16];
        self.sostenuto = [false; 16];
        self.bend = [1.0; 16];
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len());
        let frames = frames_in(io, frames, ch);
        let evs = self.events.take_sorted();
        run_block(self, &evs, frames, Self::apply, |p, n| {
            let y = p.render_sample() * p.level.next();
            for b in io.iter_mut().take(ch) {
                if let Some(x) = b.get_mut(n) {
                    *x = y;
                }
            }
        });
        self.events.restore(evs);
    }

    fn tail_samples(&self) -> usize {
        ms_to_samples(self.p.v(11), self.sr) as usize
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        self.events.push(offset, EventKind::On(0, note.min(127), velocity.min(127), -1));
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        self.events.push(offset, EventKind::Off(0, note.min(127), -1));
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        if let Some(kind) = EventKind::from_midi(msg) {
            self.events.push(offset, kind);
        }
    }

    fn event(&mut self, offset: u32, ev: &PluginEvent) -> bool {
        EventKind::from_event(ev).is_none_or(|kind| self.events.push(offset as usize, kind))
    }

    fn all_notes_off(&mut self) {
        self.events.push(0, EventKind::AllOff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NoteId;

    fn ev(channel: u8, note_id: i32, message: Message) -> PluginEvent {
        PluginEvent { channel, note_id, message }
    }

    fn on(ch: u8, key: u8, id: i32) -> PluginEvent {
        ev(ch, id, Message::NoteOn { key, velocity: 100, id: NoteId::NONE })
    }

    fn off(ch: u8, key: u8, id: i32) -> PluginEvent {
        ev(ch, id, Message::NoteOff { key, velocity: 0, id: NoteId::NONE })
    }

    fn cc(ch: u8, cc: u8, value: u8) -> PluginEvent {
        ev(ch, -1, Message::Control { cc, value })
    }

    fn synth() -> SubtractiveSynth {
        let mut s = SubtractiveSynth::new();
        s.prepare(48_000.0, 64, 2);
        s
    }

    fn run(s: &mut SubtractiveSynth) {
        let mut io = vec![vec![0.0; 64]; 2];
        s.process(&mut io, 64);
    }

    fn held(s: &SubtractiveSynth) -> Vec<(u8, u8, i32)> {
        s.voices.iter().filter(|v| v.held || v.sustained).map(|v| (v.ch, v.note, v.id)).collect()
    }

    #[test]
    fn every_admitted_note_on_gets_its_off_through_a_full_queue() {
        let mut s = synth();
        for _ in 0..700 {
            s.event(0, &ev(0, -1, Message::PitchBend { value: 9000 }));
        }
        let ons: Vec<bool> = (0..300).map(|i| s.event(1, &on((i % 16) as u8, (i % 128) as u8, i))).collect();
        let offs: Vec<bool> = (0..300).map(|i| s.event(2, &off((i % 16) as u8, (i % 128) as u8, i))).collect();
        let admitted = ons.iter().filter(|a| **a).count();
        assert_eq!(admitted, MAX_EVENTS - RELEASE_RESERVE - 700, "non-releases stop at the reserve");
        for (i, (a, b)) in ons.iter().zip(&offs).enumerate() {
            assert!(!*a || *b, "note {i} was admitted but its off was refused");
        }
        assert_eq!(s.events.len(), MAX_EVENTS);
        assert!(!s.event(3, &on(0, 1, 999)), "a full queue refuses a note-on");
        run(&mut s);
        assert!(held(&s).is_empty(), "every admitted note was released: {:?}", held(&s));
        assert_eq!(s.events.len(), 0);
    }

    #[test]
    fn voices_match_by_id_else_by_channel_and_key() {
        let mut s = synth();
        s.event(0, &on(0, 60, 1));
        s.event(0, &on(0, 60, 2));
        s.event(0, &on(1, 60, -1));
        run(&mut s);
        assert_eq!(held(&s).len(), 3, "two ids on one key are two voices; channel 1 is a third");
        s.event(0, &off(0, 60, 1));
        run(&mut s);
        let mut h = held(&s);
        h.sort_unstable();
        assert_eq!(h, vec![(0, 60, 2), (1, 60, -1)], "only id 1 released");
        s.event(0, &off(1, 60, -1));
        run(&mut s);
        assert_eq!(held(&s), vec![(0, 60, 2)], "a key off finds its channel only");
    }

    #[test]
    fn pedals_and_bend_are_per_channel() {
        let mut s = synth();
        s.event(0, &cc(1, 64, 127));
        s.event(0, &on(0, 60, -1));
        s.event(0, &on(1, 62, -1));
        s.event(10, &off(0, 60, -1));
        s.event(10, &off(1, 62, -1));
        s.event(20, &ev(1, -1, Message::PitchBend { value: 16383 }));
        run(&mut s);
        assert_eq!(held(&s), vec![(1, 62, -1)], "channel 1's sustain holds only its own note");
        assert_eq!(s.bend[0], 1.0);
        assert!(s.bend[1] > 1.1);
        s.event(0, &cc(1, 121, 0));
        run(&mut s);
        assert!(held(&s).is_empty(), "reset controllers lifts the pedal");
        assert_eq!(s.bend[1], 1.0);
    }

    #[test]
    fn sostenuto_holds_only_what_was_down_when_it_went_down() {
        let mut s = synth();
        s.event(0, &on(0, 60, -1));
        s.event(1, &cc(0, 66, 127));
        s.event(2, &on(0, 64, -1));
        s.event(3, &off(0, 60, -1));
        s.event(3, &off(0, 64, -1));
        run(&mut s);
        assert_eq!(held(&s), vec![(0, 60, -1)], "the latched note sounds on, the later one does not");
        s.event(0, &cc(0, 66, 0));
        run(&mut s);
        assert!(held(&s).is_empty());
    }

    #[test]
    fn the_legacy_midi_path_reads_the_channel_nibble() {
        let mut s = synth();
        s.midi(0, [0xB3, 64, 127]);
        s.note_on(0, 60, 100);
        s.note_off(1, 60);
        run(&mut s);
        assert!(held(&s).is_empty(), "sustain on channel 4 does not hold a channel 1 note");
        assert!(s.sustain[3]);
    }
}
