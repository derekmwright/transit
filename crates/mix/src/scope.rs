//! Per-insert scope taps: a preallocated ring of the peak of each frame an insert (or an
//! instrument) produced, and the gain reduction it reported, for the panel of a built-in plug-in.
//!
//! The plan compiler makes one [`ScopeTap`] with each fresh insert or instrument instance (it moves
//! with the instance across plans) and registers it in the [`ScopeHub`] under the track and the
//! insert's uid. The render writes to a tap only while its `enabled` flag is set, which the UI flips
//! without a recompile; nothing here allocates or locks on the audio thread. The UI reads through the
//! hub, which holds weak references, so a retired instance's tap goes away with it.
//!
//! The ring is written with relaxed stores and published with a release store of the write index.
//! A reader may see a block half written; it is a display, so that is accepted.

use soundcraft_model::TrackId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// The hub key of an instrument (an instrument's instance has no insert uid).
pub const INSTRUMENT_UID: u64 = u64::MAX;

/// Frames the ring holds (a power of two).
pub const SCOPE_FRAMES: usize = 2048;
const MASK: usize = SCOPE_FRAMES - 1;

/// The ring of one insert, its enable flag and its gain reduction.
pub struct ScopeTap {
    enabled: AtomicBool,
    /// Peak of the channels per frame, as `f32` bits.
    buf: Box<[AtomicU32]>,
    /// Frames written in all (wrapping).
    write: AtomicUsize,
    /// The plug-in's gain reduction in dB as of the last block, as `f32` bits.
    gr: AtomicU32,
}

impl Default for ScopeTap {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeTap {
    /// A tap, off, with an empty ring.
    #[must_use]
    pub fn new() -> Self {
        ScopeTap {
            enabled: AtomicBool::new(false),
            buf: (0..SCOPE_FRAMES).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            gr: AtomicU32::new(0),
        }
    }

    /// Bytes one tap holds (counted in the plan's prepared bytes).
    pub(crate) const BYTES: usize = SCOPE_FRAMES * size_of::<AtomicU32>() + size_of::<ScopeTap>();

    /// Whether the render writes to this tap.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// The gain reduction in dB the render last stored (positive: reducing).
    #[must_use]
    pub fn gain_reduction_db(&self) -> f32 {
        f32::from_bits(self.gr.load(Ordering::Relaxed))
    }

    /// Audio thread: stores the peak of each of the first `frames` frames of `buf` and `gr_db`.
    /// Does nothing while disabled.
    pub(crate) fn push(&self, buf: &[Vec<f32>], frames: usize, gr_db: f32) {
        if !self.enabled() {
            return;
        }
        // One writer: the render thread.
        let w = self.write.load(Ordering::Relaxed);
        // A block longer than the ring leaves only its last frames.
        let first = frames.saturating_sub(SCOPE_FRAMES);
        for i in first..frames {
            let mut peak = 0.0f32;
            for c in buf {
                if let Some(x) = c.get(i) {
                    peak = peak.max(x.abs());
                }
            }
            if let Some(cell) = self.buf.get(w.wrapping_add(i) & MASK) {
                cell.store(peak.to_bits(), Ordering::Relaxed);
            }
        }
        self.gr.store(gr_db.to_bits(), Ordering::Relaxed);
        self.write.store(w.wrapping_add(frames), Ordering::Release);
    }

    /// Copies the last `out.len()` frames (at most the ring holds; older ones read 0.0) in time
    /// order, the newest last. Returns how many frames the ring has ever received, saturated at
    /// `out.len()`.
    pub fn read(&self, out: &mut [f32]) -> usize {
        let end = self.write.load(Ordering::Acquire);
        let n = out.len().min(SCOPE_FRAMES);
        let skip = out.len() - n;
        let start = end.wrapping_sub(n);
        for (i, o) in out.iter_mut().enumerate() {
            *o = match i.checked_sub(skip) {
                Some(k) => self.buf.get(start.wrapping_add(k) & MASK).map_or(0.0, |c| f32::from_bits(c.load(Ordering::Relaxed))),
                None => 0.0,
            };
        }
        end.min(out.len())
    }
}

/// The taps of a compiler's instances, by track and insert uid. Control and UI threads only.
#[derive(Default)]
pub struct ScopeHub {
    taps: Mutex<HashMap<(TrackId, u64), Weak<ScopeTap>>>,
}

impl std::fmt::Debug for ScopeHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeHub").finish_non_exhaustive()
    }
}

impl ScopeHub {
    /// Records `tap` for the insert `uid` of `track`; taps of retired instances are dropped.
    pub fn register(&self, track: TrackId, uid: u64, tap: &Arc<ScopeTap>) {
        if uid == 0 {
            return;
        }
        if let Ok(mut m) = self.taps.lock() {
            m.retain(|_, w| w.strong_count() > 0);
            m.insert((track, uid), Arc::downgrade(tap));
        }
    }

    /// The tap of insert `uid` of `track`, if an instance of it is alive.
    #[must_use]
    pub fn get(&self, track: TrackId, uid: u64) -> Option<Arc<ScopeTap>> {
        self.taps.lock().ok()?.get(&(track, uid))?.upgrade()
    }

    /// Switches the tap of an insert on or off; false when it has none.
    pub fn set_enabled(&self, track: TrackId, uid: u64, on: bool) -> bool {
        self.get(track, uid).map(|t| t.set_enabled(on)).is_some()
    }

    /// Copies the last `out.len()` frames of an insert's tap (see [`ScopeTap::read`]); 0 with no tap.
    pub fn frames(&self, track: TrackId, uid: u64, out: &mut [f32]) -> usize {
        self.get(track, uid).map_or(0, |t| t.read(out))
    }

    /// The insert's last reported gain reduction in dB.
    #[must_use]
    pub fn gain_reduction_db(&self, track: TrackId, uid: u64) -> f32 {
        self.get(track, uid).map_or(0.0, |t| t.gain_reduction_db())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(v: f32, n: usize) -> Vec<Vec<f32>> {
        vec![vec![v; n], vec![-2.0 * v; n]]
    }

    #[test]
    fn disabled_tap_writes_nothing_and_enabled_tap_keeps_the_peak_of_the_channels() {
        let t = ScopeTap::new();
        t.push(&block(0.25, 64), 64, 3.0);
        let mut out = [1.0f32; 8];
        assert_eq!(t.read(&mut out), 0);
        assert!(out.iter().all(|x| *x == 0.0));
        t.set_enabled(true);
        t.push(&block(0.25, 64), 64, 3.0);
        assert_eq!(t.read(&mut out), 8);
        assert!(out.iter().all(|x| (*x - 0.5).abs() < 1e-6));
        assert!((t.gain_reduction_db() - 3.0).abs() < 1e-6);
    }

    #[test]
    fn ring_wraps_and_a_long_block_leaves_its_last_frames() {
        let t = ScopeTap::new();
        t.set_enabled(true);
        let ramp: Vec<f32> = (0..5000).map(|i| i as f32).collect();
        t.push(&[ramp], 5000, 0.0);
        let mut out = [0.0f32; 4];
        t.read(&mut out);
        assert_eq!(out, [4996.0, 4997.0, 4998.0, 4999.0]);
        let mut big = vec![0.0f32; SCOPE_FRAMES + 10];
        t.read(&mut big);
        assert_eq!(big[10 + SCOPE_FRAMES - 1], 4999.0);
        assert_eq!(big[0], 0.0);
    }

    #[test]
    fn hub_forgets_a_dropped_tap() {
        let hub = ScopeHub::default();
        let t = Arc::new(ScopeTap::new());
        hub.register(TrackId(1), 7, &t);
        assert!(hub.set_enabled(TrackId(1), 7, true));
        assert!(t.enabled());
        drop(t);
        assert!(hub.get(TrackId(1), 7).is_none());
        assert!(!hub.set_enabled(TrackId(1), 7, true));
    }
}
