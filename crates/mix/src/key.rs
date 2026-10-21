//! Sidechain keys (ADR 0015 slice c): the buffers an insert detects on in place of its own input.
//!
//! The plan compiler lays out one [`KeyEntry`] per keyed insert, with a buffer as wide as the
//! keyed strip. The render fills an entry from its source (a strip's tap or a bus) just before
//! the keyed strip's layer runs, when everything that source depends on has rendered, and the
//! keyed strip reads it through a shared reference: layers are disjoint, so the borrow is safe.
//! Nothing here allocates once the bank is built.

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

struct KeyEntry {
    spec: KeySpec,
    buf: Vec<Vec<f32>>,
}

/// The key buffers of a plan.
#[derive(Default)]
pub(crate) struct KeyBank {
    entries: Vec<KeyEntry>,
}

/// Maps `src` onto `dst`'s channels: the same channels when the counts match, a mono key onto
/// every channel, a fold-down to the average, else channel k from `src[k % n]`.
fn map_channels(src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize) {
    let n = src.len();
    let nd = dst.len();
    for (c, d) in dst.iter_mut().enumerate() {
        let d = d.get_mut(..frames);
        let Some(d) = d else { continue };
        match (n, nd) {
            (0, _) => d.fill(0.0),
            (_, 1) if n > 1 => {
                d.fill(0.0);
                for s in src {
                    for (x, y) in d.iter_mut().zip(s) {
                        *x += *y;
                    }
                }
                let g = 1.0 / n as f32;
                d.iter_mut().for_each(|x| *x *= g);
            }
            _ => match src.get(c % n) {
                Some(s) => {
                    for (x, y) in d.iter_mut().zip(s) {
                        *x = *y;
                    }
                }
                None => d.fill(0.0),
            },
        }
    }
}

impl KeyBank {
    /// The entries for `specs`, each with a silent buffer of `block` frames.
    pub(crate) fn new(specs: &[KeySpec], block: usize) -> Self {
        let entries = specs.iter().map(|&spec| KeyEntry { spec, buf: vec![vec![0.0; block]; usize::from(spec.ch).max(1)] }).collect();
        KeyBank { entries }
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
            map_channels(src.unwrap_or(&[]), &mut e.buf, frames);
        }
    }
}
