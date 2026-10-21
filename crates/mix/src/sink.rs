//! Output sinks (ADR 0015 slice b): every output path of the machine's [`IoMap`] that a track,
//! bus or send names with `Route::Hardware(name)` is a summing point of its own, like Main.
//!
//! The compiler lays the sinks out from the session and the table ([`plan_sinks`]); the mix
//! renders into them and appends their channels after Main's in the block it hands the device
//! callback, which writes each channel to its device channel and sums where paths overlap.

use super::{ChannelFormat, MAX_CHANNELS, Session, TrackKind};
use soundcraft_model::{IoDirection, IoMap, MAX_DEVICE_CHANNELS, Route};

/// Most sink channels in a plan; a sink beyond it is unresolved (silent and listed).
pub const MAX_SINK_CHANNELS: usize = 2 * MAX_DEVICE_CHANNELS;

/// One sink the plan renders: the path's name, the device channels each of its channels plays on,
/// and its format (one channel per entry of `dest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkSpec {
    pub name: String,
    pub dest: Vec<u16>,
    pub format: ChannelFormat,
}

/// The sinks a session needs on a machine, and the output route names that have none here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkPlan {
    pub sinks: Vec<SinkSpec>,
    /// Names of outputs and send targets that name no output path this device opened, in track
    /// order, once each. They render as silent sinks; the session keeps the names, so the machine
    /// they were made on restores them.
    pub unresolved: Vec<String>,
}

impl SinkPlan {
    /// Channels the sinks add after Main's in the device block.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.sinks.iter().map(|s| s.dest.len()).sum()
    }

    /// The device channel of each sink channel, in block order.
    #[must_use]
    pub fn destinations(&self) -> Vec<u16> {
        self.sinks.iter().flat_map(|s| s.dest.iter().copied()).collect()
    }
}

/// Lay out the sinks of `s` for the paths in `io`. `out_channels` is what the device opened (0: no
/// device, nothing to check a channel against). Master and VCA strips have no output of their own.
#[must_use]
pub fn plan_sinks(s: &Session, io: &IoMap, out_channels: u16) -> SinkPlan {
    let mut plan = SinkPlan::default();
    let mut seen: Vec<&str> = Vec::new();
    let mut room = MAX_SINK_CHANNELS;
    let tracks = s.tracks.iter().filter(|t| !matches!(t.kind, TrackKind::Master | TrackKind::Vca));
    for t in tracks {
        let names = std::iter::once(&t.mixer.output).chain(t.mixer.sends.iter().flatten().map(|x| &x.target));
        for r in names {
            let Route::Hardware(name) = r else { continue };
            if seen.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                continue;
            }
            seen.push(name);
            let path = io.path(name).filter(|p| p.direction == IoDirection::Output);
            let open = |c: &u16| out_channels == 0 || *c < out_channels;
            match path.filter(|p| p.channels.len() <= MAX_CHANNELS && p.channels.len() <= room && p.channels.iter().all(open)) {
                Some(p) => {
                    room -= p.channels.len();
                    let format = if p.format.channels() == p.channels.len() { p.format } else { ChannelFormat::for_channels(p.channels.len()) };
                    plan.sinks.push(SinkSpec { name: p.name.clone(), dest: p.channels.clone(), format });
                }
                None => plan.unresolved.push(name.clone()),
            }
        }
    }
    plan
}

/// The output route names of `s` that name no output path of `io` (see [`SinkPlan::unresolved`]).
#[must_use]
pub fn unresolved_routes(s: &Session, io: &IoMap, out_channels: u16) -> Vec<String> {
    plan_sinks(s, io, out_channels).unresolved
}

/// A sink's buffers in a running plan.
pub(crate) struct Sink {
    pub(crate) name: String,
    pub(crate) fmt: ChannelFormat,
    pub(crate) buf: Vec<Vec<f32>>,
    /// The device latency every signal summed here is aligned to this block (see `pdc_visit`).
    pub(crate) arrival: usize,
}

impl Sink {
    pub(crate) fn new(spec: &SinkSpec, block: usize) -> Self {
        Sink { name: spec.name.clone(), fmt: spec.format, buf: vec![vec![0.0; block]; spec.dest.len()], arrival: 0 }
    }

    pub(crate) fn clear(&mut self, frames: usize) {
        for c in self.buf.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        self.arrival = 0;
    }
}

/// The sink `route` names (`Route::Hardware`), if the plan has one. No allocation.
pub(crate) fn sink_index(sinks: &[Sink], name: &str) -> Option<usize> {
    sinks.iter().position(|s| s.name.eq_ignore_ascii_case(name))
}
