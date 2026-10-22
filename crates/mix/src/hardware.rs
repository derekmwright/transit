//! Hardware inserts in a plan (ADR 0015 section 5, slice d).
//!
//! A hardware insert slot sends the strip's signal, as it stands after the slots before it, to
//! an output path's sink, and continues the chain with the signal that comes back on an input
//! path. The return read is the block the device captured during this callback, which holds what
//! was sent a measured round trip ago: the insert is a delay of that many frames, and it reports
//! exactly that as its latency, so delay compensation lines the strip up with its parallel tracks
//! like it does for a latent plug-in. Nothing here allocates on the audio thread: the send buffers
//! are laid out by the compiler, the return is read from the block of live input the engine holds,
//! and the send is summed into the sink buffer the plan already has.
//!
//! The slot is inactive, and passes the strip's signal through dry, when the plan has no live input
//! (the device is not duplex, or this is an offline render) or a path is unresolved.

use super::{MAX_LATENCY, Session, Sink, Track, TrackId, TrackKind, sink::SinkSpec};
use soundcraft_model::{IoDirection, IoMap};

/// What a hardware insert slot will do on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareState {
    /// Both paths resolve and the round trip was measured: the slot runs and reports it.
    Ready,
    /// Both paths resolve but the pair was not measured at this rate: the slot runs and reports no
    /// latency, so its track arrives late by the round trip.
    Unmeasured,
    /// The send path is not an output path of this device: the slot passes the signal through dry.
    MissingSend,
    /// The return path is not an input path of this device: the slot passes the signal through dry.
    MissingReturn,
    /// Both paths resolve but no capture stream is open (not a duplex device): the slot passes the
    /// signal through dry.
    NoInput,
}

/// One hardware insert of a session as this machine sees it (the UI's card, the engine's report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareReport {
    pub track: TrackId,
    pub slot: usize,
    pub send: String,
    pub ret: String,
    pub state: HardwareState,
    /// The measured round trip in frames, when there is one.
    pub latency: Option<u32>,
}

/// A hardware insert in a plan: where it sends, what it reads, and its latency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HwSpec {
    /// The insert slot of the strip.
    pub(crate) slot: u16,
    /// The sink of the plan the send plays on (`None`: the send path is unresolved).
    pub(crate) sink: Option<u16>,
    /// The width of that sink.
    pub(crate) sink_width: u16,
    /// The device input channel each return channel reads (empty: the return is unresolved).
    pub(crate) src: Vec<u16>,
    /// Frames of round trip, 0 when unmeasured.
    pub(crate) latency: u32,
    pub(crate) measured: bool,
}

impl HwSpec {
    pub(crate) fn runs(&self) -> bool {
        self.sink.is_some() && !self.src.is_empty()
    }
}

/// The latency, in frames, the hardware inserts of `t` would add: an upper bound for sizing delay
/// lines (a plan without live input adds none).
pub(crate) fn latency_of(t: &Track, io: &IoMap, rate: u32) -> usize {
    t.mixer
        .inserts
        .iter()
        .flatten()
        .filter(|i| i.active && !i.bypass)
        .filter_map(|i| i.hardware.as_ref())
        .fold(0usize, |a, h| a.saturating_add(io.hardware_latency(&h.send, &h.ret, rate).unwrap_or(0) as usize))
        .min(MAX_LATENCY)
}

/// The hardware inserts of `t` for a plan with `sinks`, an `in_channels`-wide device input, at `rate`.
pub(crate) fn specs_of(t: &Track, io: &IoMap, sinks: &[SinkSpec], in_channels: u16, rate: u32) -> Vec<HwSpec> {
    let mut out = Vec::new();
    for (slot, ins) in t.mixer.inserts.iter().enumerate() {
        let Some(h) = ins.as_ref().and_then(|i| i.hardware.as_ref()) else { continue };
        let Ok(slot) = u16::try_from(slot) else { continue };
        let sink = sinks.iter().position(|k| k.name.eq_ignore_ascii_case(&h.send));
        let src = io
            .path(&h.ret)
            .filter(|p| p.direction == IoDirection::Input && p.channels.iter().all(|&c| in_channels == 0 || c < in_channels))
            .map(|p| p.channels.clone())
            .unwrap_or_default();
        let measured = io.hardware_latency(&h.send, &h.ret, rate);
        out.push(HwSpec {
            slot,
            sink: sink.and_then(|k| u16::try_from(k).ok()),
            sink_width: sink.and_then(|k| sinks.get(k)).map_or(0, |k| u16::try_from(k.dest.len()).unwrap_or(u16::MAX)),
            src,
            latency: measured.unwrap_or(0),
            measured: measured.is_some(),
        });
    }
    out
}

/// Every hardware insert of `s` as the machine `io` sees it. `out_channels` and `in_channels` are
/// what the device opened (0: no device), `duplex` whether a capture stream is open, `rate` the
/// session's sample rate.
#[must_use]
pub fn hardware_report(s: &Session, io: &IoMap, out_channels: u16, in_channels: u16, duplex: bool, rate: u32) -> Vec<HardwareReport> {
    let sinks = super::plan_sinks(s, io, out_channels).sinks;
    let mut out = Vec::new();
    for t in s.tracks.iter().filter(|t| !matches!(t.kind, TrackKind::Master | TrackKind::Vca)) {
        for spec in specs_of(t, io, &sinks, in_channels, rate) {
            let Some(h) = t.mixer.inserts.get(usize::from(spec.slot)).and_then(Option::as_ref).and_then(|i| i.hardware.as_ref()) else { continue };
            let state = match (spec.sink.is_some(), spec.src.is_empty(), duplex, spec.measured) {
                (false, ..) => HardwareState::MissingSend,
                (true, true, ..) => HardwareState::MissingReturn,
                (true, false, false, _) => HardwareState::NoInput,
                (true, false, true, true) => HardwareState::Ready,
                (true, false, true, false) => HardwareState::Unmeasured,
            };
            out.push(HardwareReport {
                track: t.id,
                slot: usize::from(spec.slot),
                send: h.send.clone(),
                ret: h.ret.clone(),
                state,
                latency: spec.measured.then_some(spec.latency),
            });
        }
    }
    out
}

/// A hardware insert's state in a running strip.
pub(crate) struct HwSlot {
    pub(crate) spec: HwSpec,
    /// This block's send, one buffer per sink channel.
    send: Vec<Vec<f32>>,
    /// The slot ran this block (the send is summed into its sink after the strip).
    live: bool,
}

impl HwSlot {
    pub(crate) fn new(spec: HwSpec, block: usize) -> Self {
        let send = vec![vec![0.0; block]; usize::from(spec.sink_width)];
        HwSlot { spec, send, live: false }
    }

    pub(crate) fn rest(&mut self) {
        self.live = false;
    }

    /// Run the slot on the strip's `buf` with this block's live `input` (planar, device channels):
    /// keep the signal for the send and replace it with the return. False (and `buf` untouched)
    /// when the slot is inactive.
    pub(crate) fn run(&mut self, buf: &mut [Vec<f32>], input: &[Vec<f32>], frames: usize) -> bool {
        self.live = false;
        if input.is_empty() || !self.spec.runs() || buf.is_empty() {
            return false;
        }
        let n = buf.len();
        let m = self.send.len();
        for (c, dst) in self.send.iter_mut().enumerate() {
            let dst = dst.get_mut(..frames).unwrap_or(&mut []);
            if m == 1 && n > 1 {
                // A mono send takes the strip's mix.
                let g = 1.0 / n as f32;
                for (i, d) in dst.iter_mut().enumerate() {
                    *d = buf.iter().map(|b| b.get(i).copied().unwrap_or(0.0)).sum::<f32>() * g;
                }
            } else if let Some(src) = buf.get(c.min(n - 1)) {
                for (d, x) in dst.iter_mut().zip(src.iter()) {
                    *d = *x;
                }
            }
        }
        let last = self.spec.src.len().saturating_sub(1);
        for (k, dst) in buf.iter_mut().enumerate() {
            // A mono return feeds every channel of a wider strip.
            let dev = usize::from(self.spec.src.get(k.min(last)).copied().unwrap_or(0));
            let src = input.get(dev % input.len());
            let dst = dst.get_mut(..frames).unwrap_or(&mut []);
            match src {
                Some(src) => {
                    for (d, x) in dst.iter_mut().zip(src.iter()) {
                        *d = *x;
                    }
                    if src.len() < frames {
                        dst.iter_mut().skip(src.len()).for_each(|d| *d = 0.0);
                    }
                }
                None => dst.iter_mut().for_each(|d| *d = 0.0),
            }
        }
        self.live = true;
        true
    }

    /// Sum this block's send into its sink.
    pub(crate) fn flush(&self, sinks: &mut [Sink], frames: usize) {
        if !self.live {
            return;
        }
        let Some(k) = self.spec.sink.and_then(|k| sinks.get_mut(usize::from(k))) else { return };
        for (d, s) in k.buf.iter_mut().zip(self.send.iter()) {
            for (d, s) in d.iter_mut().zip(s.iter()).take(frames) {
                *d += *s;
            }
        }
    }
}
