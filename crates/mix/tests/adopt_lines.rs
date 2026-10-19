//! `MixEngine::adopt_lines` takes in only prepared delay lines that serve this engine: the same
//! channel count, longer than the current line, and a ring that holds a full engine block at the
//! longest delay. Anything else stays with the caller, and the clamp report keeps saying what the
//! strip is missing, so playback prepares again.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::Plugin;
use soundcraft_mix::{MixEngine, PreparedInstance, PreparedLines, mark_audio_thread};
use soundcraft_model::{ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackId, TrackKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SR: f32 = 48_000.0;
const BLOCK: usize = 256;
const PERIOD: usize = 4096;
/// A latency past the smallest lines, whose lines round up to 32,768 samples.
const LATENCY: usize = 32_700;

/// A latent insert that delays its input by the latency a test sets.
struct VarDelay {
    knob: Arc<AtomicUsize>,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl Plugin for VarDelay {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("gain").unwrap()
    }
    fn prepare(&mut self, _sr: f32, _mb: usize, _ch: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _id: &str, _v: f32) -> bool {
        true
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let d = self.latency();
        let n = self.ring[0].len();
        for (c, ch) in io.iter_mut().enumerate().take(2) {
            for (i, x) in ch.iter_mut().take(frames).enumerate() {
                let w = (self.w + i) % n;
                self.ring[c][w] = *x;
                *x = self.ring[c][(w + n - d) % n];
            }
        }
        self.w = (self.w + frames) % n;
    }
    fn latency(&self) -> usize {
        self.knob.load(Ordering::Relaxed)
    }
}

fn pulse_track(s: &mut Session, name: &str, level: f32, src: u64) -> TrackId {
    let len = 48_000usize * 30;
    let sig: Vec<f32> = (0..len).map(|i| if i % PERIOD == 0 { level } else { 0.0 }).collect();
    s.pool.insert(SourceId(src), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "p", SourceId(src), 0, 0, len as i64));
    t
}

/// A legacy engine on the audio thread (it never grows lines itself) playing a dry pulse (0.25)
/// and a latent one (0.5) that sum to 0.75 when compensated.
struct Rig {
    mix: MixEngine,
    s: Session,
    lat: TrackId,
    knob: Arc<AtomicUsize>,
    pos: i64,
    out: Vec<Vec<f32>>,
}

impl Rig {
    fn new() -> Self {
        MixEngine::force_serial(true);
        mark_audio_thread();
        let mut s = Session::default();
        s.edit.delay_compensation = true;
        s.edit.click = false;
        pulse_track(&mut s, "Dry", 0.25, 71);
        let lat = pulse_track(&mut s, "Lat", 0.5, 72);
        s.track_mut(lat).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
        let knob = Arc::new(AtomicUsize::new(100));
        let mut mix = MixEngine::new(SR, BLOCK);
        mix.set_external_instances(true);
        let ring = vec![vec![0.0; 400_000]; 2];
        let plugin = Box::new(VarDelay { knob: knob.clone(), ring, w: 0 });
        mix.adopt(vec![PreparedInstance { track: lat, slot: 0, id: "gain".into(), ch: 2, plugin }]);
        mix.ensure_synced(&s);
        Rig { mix, s, lat, knob, pos: 0, out: vec![vec![0.0f32; BLOCK]; 2] }
    }

    fn render(&mut self, blocks: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..blocks {
            self.mix.render(&self.s, self.pos, BLOCK, &mut self.out);
            all.extend_from_slice(&self.out[0]);
            self.pos += BLOCK as i64;
        }
        all
    }

    /// Plays at latency 100, then raises it past the lines: the next blocks clamp and report.
    fn clamp(&mut self) {
        self.render(20);
        self.knob.store(LATENCY, Ordering::Relaxed);
        self.render(4);
        let st = self.mix.status();
        assert!(st.capacity_exceeded > 0 && st.required_latency as usize >= LATENCY, "the shortfall is reported: {st:?}");
    }

    fn need(&self) -> usize {
        self.mix.status().required_latency as usize
    }

    fn pulses(&mut self) -> Vec<f32> {
        let all = self.render(300);
        let late = &all[all.len() - 4 * PERIOD..];
        late.iter().copied().filter(|x| x.abs() > 1e-3).collect()
    }

    fn assert_summed(&mut self, what: &str) {
        let hits = self.pulses();
        assert!(!hits.is_empty() && hits.iter().all(|x| (x - 0.75).abs() < 1e-4), "{what}: dry and latent pulses coincide: {hits:?}");
        assert_eq!(self.mix.status().capacity_exceeded, 0, "{what}: nothing clamped");
    }
}

/// Lines prepared with a smaller block than the engine's cannot hold a full block at their
/// longest delay: they are refused (and handed back), the shortfall keeps being reported, and
/// lines prepared with the engine's block then realign the pulses.
#[test]
fn prepared_lines_too_short_for_the_engines_block_are_refused() {
    let mut r = Rig::new();
    r.clamp();
    let mut small = PreparedLines::new(&r.s, 16, r.need());
    let before = small.bytes();
    r.mix.adopt_lines(&mut small);
    assert_eq!(small.bytes(), before, "the undersized lines are handed back, not installed");
    let st = r.mix.status();
    assert!(st.capacity_exceeded > 0 && st.required_latency as usize >= LATENCY, "still reports the shortfall: {st:?}");
    let hits = r.pulses();
    assert!(hits.iter().any(|x| (x - 0.75).abs() > 1e-3), "the pulses stay apart without lines that fit: {hits:?}");

    let mut right = PreparedLines::new(&r.s, BLOCK, r.need());
    r.mix.adopt_lines(&mut right);
    r.assert_summed("after lines prepared with the engine's block");
}

/// X1: a prepared line whose channel count differs from the strip's is not installed.
#[test]
fn a_prepared_line_with_the_wrong_channel_count_is_not_installed() {
    let mut r = Rig::new();
    r.clamp();
    let mut other = r.s.clone();
    other.track_mut(r.lat).unwrap().format = ChannelFormat::Mono;
    let mut mono = PreparedLines::new(&other, BLOCK, r.need());
    r.mix.adopt_lines(&mut mono);
    // The dry strip's lines (same channels) are taken in; the latent strip's mono ones are not,
    // so its shortfall is still reported.
    let st = r.mix.status();
    assert!(st.capacity_exceeded > 0 && st.required_latency as usize >= LATENCY, "the strip still reports its shortfall: {st:?}");
}

/// X2: a prepared line no longer than the strip's current one is not installed (it would shorten
/// the delay the strip holds).
#[test]
fn a_prepared_line_not_longer_than_the_current_one_is_not_installed() {
    let mut r = Rig::new();
    r.clamp();
    let mut good = PreparedLines::new(&r.s, BLOCK, r.need());
    r.mix.adopt_lines(&mut good);
    r.assert_summed("with the long lines");
    let mut short = PreparedLines::new(&r.s, BLOCK, 100);
    let before = short.bytes();
    r.mix.adopt_lines(&mut short);
    assert_eq!(short.bytes(), before, "the shorter lines are handed back, not swapped in");
    r.assert_summed("after offering shorter lines");
}

/// X8: adopting lines that upgrade nothing does not clear the clamp report.
#[test]
fn adopting_lines_that_upgrade_nothing_keeps_the_clamp_report() {
    let mut r = Rig::new();
    r.clamp();
    let need = r.need();
    let mut short = PreparedLines::new(&r.s, BLOCK, 100);
    r.mix.adopt_lines(&mut short);
    let st = r.mix.status();
    assert!(st.capacity_exceeded > 0 && st.required_latency as usize == need, "report kept: {st:?}");
    // Lines that do hold the delay clear it.
    let mut good = PreparedLines::new(&r.s, BLOCK, need);
    r.mix.adopt_lines(&mut good);
    assert_eq!(r.mix.status().capacity_exceeded, 0, "upgraded strips clear the report");
}
