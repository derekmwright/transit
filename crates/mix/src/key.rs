//! Sidechain keys (ADR 0015 slice c): the buffers an insert detects on in place of its own input.
//!
//! The plan compiler lays out one [`KeyEntry`] per keyed insert, with a buffer as wide as the
//! keyed strip. The render fills an entry from its source (a strip's tap or a bus) just before
//! the keyed strip's layer runs, when everything that source depends on has rendered, and the
//! keyed strip reads it through a shared reference: layers are disjoint, so the borrow is safe.
//! Nothing here allocates once the bank is built.
//!
//! Delay compensation (ADR 0015 section 4): at a keyed insert the key and the strip's own signal
//! must come from the same source sample time. [`KeyBank::align`] finds the difference of their
//! arrival latencies each block; a key that arrives late delays the keyed strip's input before
//! the insert ([`KeyIn`], part of the strip's latency), a key that arrives early is delayed in its
//! [`KeyEntry`]. Both are [`KeyLine`]s sized when the plan is built.

use super::*;

/// Where a key is taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyEnd {
    /// A strip, by table index: its signal after the fader, or before it for a pre-fader key.
    Strip(u16),
    /// A bus.
    Bus(BusId),
}

/// One keyed insert of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeySpec {
    /// The keyed strip's table index and track.
    pub keyed: u16,
    pub id: TrackId,
    pub slot: u16,
    pub from: KeyEnd,
    pub pre: bool,
    /// Channels of the keyed strip: the key is mapped onto them.
    pub ch: u16,
}

/// A fixed delay applied in place to a block, over a delay line sized by the plan: the same
/// history and crossfade rules as a strip's edges, so a change of delay never reads silence the
/// line never held.
pub(crate) struct KeyLine {
    line: DelayLine,
    tap: EdgeTap,
}

impl KeyLine {
    /// A line for `ch` channels holding delays up to `cap` samples, in blocks up to `block`.
    pub(crate) fn new(ch: usize, cap: usize, block: usize) -> Self {
        KeyLine { line: DelayLine::with_capacity(ch, cap, block), tap: EdgeTap::default() }
    }

    /// The longest delay the line holds.
    pub(crate) fn cap(&self) -> usize {
        self.line.len
    }

    /// Delays the first `frames` of `buf` by `want` samples in place (`scratch` has a channel per
    /// channel of `buf`, with room for a block). With no delay and none still ringing, the block is
    /// left alone and the line goes idle.
    pub(crate) fn apply(&mut self, want: usize, buf: &mut [Vec<f32>], scratch: &mut [Vec<f32>], frames: usize) {
        if want == 0 && self.tap.holds() == 0 {
            self.line.idle();
            return;
        }
        self.line.write(buf, frames);
        let (from, to, done) = self.tap.step(want, false, &self.line, frames);
        if from == 0 && to == 0 {
            return;
        }
        self.line.read_x(from, to, done, scratch, frames);
        for (d, s) in buf.iter_mut().zip(scratch.iter()) {
            if let (Some(d), Some(s)) = (d.get_mut(..frames), s.get(..frames)) {
                d.copy_from_slice(s);
            }
        }
    }

    /// The line was not run this block: its history has a gap.
    pub(crate) fn idle(&mut self) {
        self.line.idle();
    }

    /// Takes over `old`'s history (a plan that keeps the strip).
    pub(crate) fn take_history(&mut self, old: &KeyLine) {
        self.line.take_history(&old.line);
        self.tap = old.tap;
    }
}

/// The delay of a keyed strip's own signal before one keyed insert (the key arrives late).
pub(crate) struct KeyIn {
    pub(crate) slot: u16,
    /// This block's delay, set by [`KeyBank::align`].
    pub(crate) want: usize,
    pub(crate) line: KeyLine,
    /// The insert ran this block.
    pub(crate) ran: bool,
}

impl KeyIn {
    pub(crate) fn new(slot: u16, ch: usize, cap: usize, block: usize) -> Self {
        KeyIn { slot, want: 0, line: KeyLine::new(ch, cap, block), ran: false }
    }
}

struct KeyEntry {
    spec: KeySpec,
    buf: Vec<Vec<f32>>,
    /// Delays the key when it arrives before the keyed strip's signal, by `want` this block.
    line: KeyLine,
    scratch: Vec<Vec<f32>>,
    want: usize,
}

/// The key buffers of a plan.
#[derive(Default)]
pub(crate) struct KeyBank {
    entries: Vec<KeyEntry>,
}

pub(crate) struct KeyAlignment {
    pub at: usize,
    pub arrive: usize,
    pub pos: Samples,
    pub pdc: bool,
    pub duplex: bool,
}

impl KeyBank {
    /// The entries for `specs`, each with a silent buffer of `block` frames and a delay line
    /// holding up to `cap_of(keyed strip)` samples.
    pub(crate) fn new(specs: &[KeySpec], block: usize, cap_of: impl Fn(u16) -> usize) -> Self {
        let entries = specs
            .iter()
            .map(|&spec| {
                let ch = usize::from(spec.ch).max(1);
                KeyEntry {
                    spec,
                    buf: vec![vec![0.0; block]; ch],
                    line: KeyLine::new(ch, cap_of(spec.keyed), block),
                    scratch: (0..ch).map(|_| Vec::with_capacity(block)).collect(),
                    want: 0,
                }
            })
            .collect();
        KeyBank { entries }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Delay compensation for the keys of the strip at table index `at` (`t`'s): sets the delay
    /// of each key and of the strip's input before each keyed insert, and the strip's latency
    /// with them. `arrive` is the latency the strip's input arrives with; `source` gives the
    /// latency a key arrives with (its source's output). With `pdc` off nothing is delayed.
    /// `duplex`: the device has live input, so the strip's hardware inserts run and add their round trip.
    /// Returns the longest delay a line could not hold (0: none).
    pub(crate) fn align(&mut self, t: &Track, st: &mut Strip, alignment: KeyAlignment, source: &dyn Fn(&KeySpec) -> usize) -> usize {
        let KeyAlignment { at, arrive, pos, pdc, duplex } = alignment;
        for k in st.key_in.iter_mut() {
            k.want = 0;
        }
        for e in self.entries.iter_mut().filter(|e| usize::from(e.spec.keyed) == at) {
            e.want = 0;
        }
        // A frozen track reports the latency of the chain its render replaces (`frozen_latency`): the
        // forward pass reads `st.latency`, so dropping it here made a frozen key source arrive early.
        let mut own = frozen_latency(t).saturating_add(st.instrument.as_ref().filter(|i| !i.failed).map_or(0, |i| i.plugin.latency()));
        let mut short = 0usize;
        for (i, ins) in t.mixer.inserts.iter().enumerate() {
            // A hardware insert has no plug-in: its round trip is the latency before the next slot.
            if let Some(l) = crate::hardware::slot_latency(ins, &st.hw, i, duplex) {
                own = own.saturating_add(l);
                continue;
            }
            let (Some(ins), Some(Some(slot))) = (ins, st.plugins.get_mut(i)) else { continue };
            if !ins.active || ins.bypass || slot.failed {
                continue;
            }
            apply_params(slot, ins, t, i, pos);
            if pdc
                && ins.key.is_some()
                && let Some(e) = self.entries.iter_mut().find(|e| e.spec.id == t.id && usize::from(e.spec.slot) == i)
                && let Some(k) = st.key_in.iter_mut().find(|k| usize::from(k.slot) == i)
            {
                let key_at = source(&e.spec).min(MAX_LATENCY);
                let here = arrive.saturating_add(own);
                if key_at > here {
                    // The key is late: the signal waits for it.
                    let d = key_at - here;
                    if d > k.line.cap() {
                        short = short.max(d);
                    }
                    k.want = d.min(k.line.cap());
                    own = own.saturating_add(k.want);
                } else {
                    // The key is early: it waits for the signal.
                    let d = here - key_at;
                    if d > e.line.cap() {
                        short = short.max(d);
                    }
                    e.want = d.min(e.line.cap());
                }
            }
            own = own.saturating_add(slot.plugin.latency());
        }
        st.latency = own.min(MAX_LATENCY);
        short
    }

    /// The key of track `id`'s insert in `slot`, as last filled.
    pub(crate) fn get(&self, id: TrackId, slot: usize) -> Option<&[Vec<f32>]> {
        self.entries.iter().find(|e| e.spec.id == id && usize::from(e.spec.slot) == slot).map(|e| e.buf.as_slice())
    }

    /// Fills the keys of the strips with table index in `keyed` from `strip` (the strips rendered
    /// so far, by table index) and `busses`, over `frames` samples. A source that has not rendered
    /// (`strip` gives none) fills silence.
    pub(crate) fn fill<'a>(&mut self, keyed: std::ops::Range<usize>, strip: impl Fn(usize) -> Option<&'a Strip>, busses: &BusTable, frames: usize) {
        for e in self.entries.iter_mut().filter(|e| keyed.contains(&usize::from(e.spec.keyed))) {
            let src: Option<&[Vec<f32>]> = match e.spec.from {
                KeyEnd::Strip(i) => strip(usize::from(i)).map(|st| if e.spec.pre { st.pre.as_slice() } else { st.buf.as_slice() }),
                KeyEnd::Bus(b) => busses.bus(b).map(|b| b.buf.as_slice()),
            };
            soundcraft_dsp::map_key_channels(src.unwrap_or(&[]), &mut e.buf, 0, frames);
            e.line.apply(e.want, &mut e.buf, &mut e.scratch, frames);
        }
    }
}
