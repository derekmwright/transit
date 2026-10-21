//! ADR 0015 slice (b): output paths are sinks. A track, bus or send routed to `Route::Hardware`
//! naming an output path of the machine's `IoMap` renders into that path's channels, delayed to the
//! one device latency Main has, so a stem or cue mix leaves the device with Main, sample for sample.
//! A name the table lacks is a silent, listed sink. Input routes resolve through the table too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::Plugin;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, plan_sinks,
    render_range, unresolved_routes,
};
use soundcraft_model::{
    Bus, BusId, ChannelFormat, Clip, Insert, IoDirection, IoMap, IoPath, KeyFrom, KeySource, Route, SendSlot, Session, SourceAudio, SourceId, Tap,
    TrackId, TrackKind,
};
use soundcraft_time::Range;
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
const AT: usize = 2400;
const LEN: usize = 8_192;

/// A pure delay of `n` samples that reports `n` as its latency.
struct Delay {
    n: usize,
    ring: Vec<Vec<f32>>,
    w: usize,
}

impl Plugin for Delay {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        soundcraft_dsp::plugin_info("gain").unwrap()
    }
    fn prepare(&mut self, _sr: f32, _mb: usize, ch: usize) {
        self.ring = vec![vec![0.0; self.n.max(1)]; ch.max(2)];
        self.w = 0;
    }
    fn reset(&mut self) {
        self.ring.iter_mut().for_each(|r| r.fill(0.0));
    }
    fn set_param(&mut self, _id: &str, _v: f32) -> bool {
        true
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let w0 = self.w;
        for (c, ch) in io.iter_mut().enumerate() {
            let Some(r) = self.ring.get_mut(c) else { continue };
            let mut w = w0;
            for x in ch.iter_mut().take(frames) {
                std::mem::swap(&mut r[w], x);
                w = (w + 1) % self.n;
            }
        }
        self.w = (w0 + frames) % self.n;
    }
    fn latency(&self) -> usize {
        self.n
    }
}

fn delay(n: usize) -> Insert {
    let mut i = Insert::new("gain");
    i.preset = format!("delay:{n}");
    i
}

fn make(s: &Session, spec: &InstanceSpec) -> Option<NewInstance> {
    let t = s.track(spec.track)?;
    let ins = t.mixer.inserts.get(spec.slot)?.as_ref()?;
    let Some(n) = ins.preset.strip_prefix("delay:").and_then(|n| n.parse::<usize>().ok()) else {
        return soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
            .map(|n| NewInstance { plugin: n.plugin, refused_state: false });
    };
    let mut d = Delay { n, ring: Vec::new(), w: 0 };
    d.prepare(SR as f32, BLOCK, spec.ch);
    Some(NewInstance { plugin: Box::new(d), refused_state: false })
}

fn session() -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    s
}

/// A stereo audio track playing a 0.5 impulse at `AT`, panned hard left and right.
fn impulse_track(s: &mut Session, name: &str, output: Route) -> TrackId {
    let mut sig = vec![0.0f32; LEN];
    sig[AT] = 0.5;
    let src = SourceId(s.next_id + 10_000);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "imp", src, 0, 0, LEN as i64));
    tr.mixer.pan = vec![-1.0, 1.0];
    tr.mixer.output = output;
    t
}

fn send_to(route: Route) -> Option<SendSlot> {
    Some(SendSlot { target: route, level_db: 0.0, pan: 0.0, mute: false, pre_fader: false, follow_main_pan: true, surround: None })
}

fn bus(s: &mut Session, name: &str) -> BusId {
    let id = BusId(s.alloc());
    s.busses.push(Bus { id, name: name.into(), format: ChannelFormat::Stereo });
    id
}

fn target(out: u16) -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: out, in_channels: 0 }
}

struct Rendered {
    /// Main's channels, then each sink's, from sample 0 (the mix latency is not removed).
    channels: Vec<Vec<f32>>,
    latency: usize,
    main: usize,
}

/// Renders `s` through the plan renderer on a device with `io`'s outputs, `LEN` samples plus the latency.
fn render(s: &Session, io: &IoMap, input: Option<Vec<Vec<f32>>>) -> Rendered {
    MixEngine::force_serial(true);
    let out_channels = io.outputs;
    let mut mix = MixEngine::realtime(target(out_channels));
    let mut compiler = PlanCompiler::new(target(out_channels), PlanLimits::default());
    compiler.set_io(Arc::new(io.clone()));
    let mut maker = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| make(s, spec);
    let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut maker).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let main = soundcraft_mix::main_channels(s);
    let width = main + plan_sinks(s, io, out_channels).channels();
    let mut out = vec![vec![0.0f32; BLOCK]; width];
    let mut channels = vec![Vec::new(); width];
    let mut pos = 0i64;
    let mut latency = 0;
    while channels[0].len() < LEN + latency {
        if let Some(input) = &input {
            mix.input = input.clone();
        }
        mix.render_active(pos, BLOCK, &mut out);
        latency = mix.latency;
        for (c, o) in out.iter().enumerate() {
            channels[c].extend_from_slice(o);
        }
        pos += BLOCK as i64;
    }
    assert_eq!(mix.status().capacity_exceeded, 0, "the plan's delay lines hold every sink's delay");
    Rendered { channels, latency, main }
}

fn hits(x: &[f32]) -> Vec<usize> {
    x.iter().enumerate().filter(|(_, v)| v.abs() > 1e-3).map(|(i, _)| i).collect()
}

fn wide_io() -> IoMap {
    IoMap::for_device("test", "test", 2, 10)
}

#[test]
fn stems_and_cue_mixes_leave_the_device_with_main() {
    // A latent track to Main, a dry one to "Out 3-4", a master insert on top.
    let mut s = session();
    let slow = impulse_track(&mut s, "Slow", Route::Main);
    s.track_mut(slow).unwrap().mixer.inserts[0] = Some(delay(512));
    impulse_track(&mut s, "Stem", Route::Hardware("Out 3-4".into()));
    let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, Some("Master"));
    s.track_mut(m).unwrap().mixer.inserts[0] = Some(delay(128));
    let two = render(&s, &wide_io(), None);
    assert_eq!(two.latency, 640, "Main's sources plus the master insert");
    assert_eq!(two.channels.len(), 4, "Main, then Out 3-4");
    let (main, stem) = (hits(&two.channels[0]), hits(&two.channels[two.main]));
    assert_eq!(main, [AT + 640]);
    assert_eq!(stem, main, "the stem is padded to the same device latency, master inserts included");
    // The same two tracks both on Main: one sink, the same arrival.
    let mut one = s.clone();
    one.tracks.iter_mut().filter(|t| t.name == "Stem").for_each(|t| t.mixer.output = Route::Main);
    let one = render(&one, &wide_io(), None);
    assert_eq!(hits(&one.channels[0]), [AT + 640]);
    assert_eq!(one.latency, two.latency);
    // The stem is hard-panned like Main's: left on its first channel, right on its second.
    assert!(two.channels[two.main][AT + 640] > 0.4 && two.channels[two.main + 1][AT + 640] > 0.4);
}

#[test]
fn a_path_slower_than_main_delays_main_to_it() {
    let mut s = session();
    impulse_track(&mut s, "Dry", Route::Main);
    let slow = impulse_track(&mut s, "Slow", Route::Hardware("Out 5-6".into()));
    s.track_mut(slow).unwrap().mixer.inserts[0] = Some(delay(300));
    let r = render(&s, &wide_io(), None);
    assert_eq!(r.latency, 300);
    assert_eq!(hits(&r.channels[0]), [AT + 300], "Main waits for the slowest sink");
    assert_eq!(hits(&r.channels[r.main]), [AT + 300]);
}

#[test]
fn a_send_to_a_path_feeds_it_aligned_with_main() {
    let mut s = session();
    let t = impulse_track(&mut s, "Src", Route::Main);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(delay(100));
    s.track_mut(t).unwrap().mixer.sends[0] = send_to(Route::Hardware("Out 7-8".into()));
    impulse_track(&mut s, "Other", Route::Hardware("Out 7-8".into()));
    let r = render(&s, &wide_io(), None);
    assert_eq!(r.latency, 100);
    assert_eq!(hits(&r.channels[r.main]), [AT + 100], "the cue sees the send and the dry track on the same sample");
    assert!(r.channels[r.main][AT + 100] > 0.9, "send plus the other track sum on the sink: {}", r.channels[r.main][AT + 100]);
    // A bus routed to a path carries what a track sends into it.
    let mut s = session();
    let b = bus(&mut s, "Drums");
    let t = impulse_track(&mut s, "Kick", Route::Bus(b));
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(delay(64));
    let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Drum bus"));
    let tr = s.track_mut(a).unwrap();
    tr.mixer.input = Route::Bus(b);
    tr.mixer.output = Route::Hardware("Out 3-4".into());
    tr.mixer.pan = vec![-1.0, 1.0];
    let r = render(&s, &wide_io(), None);
    assert_eq!(hits(&r.channels[r.main]), [AT + 64]);
    assert!(hits(&r.channels[0]).is_empty(), "nothing on Main");
}

#[test]
fn a_route_to_a_missing_path_is_silent_and_listed() {
    let mut s = session();
    impulse_track(&mut s, "Dry", Route::Main);
    impulse_track(&mut s, "Cue", Route::Hardware("Headphones".into()));
    let t = impulse_track(&mut s, "Sender", Route::Main);
    s.track_mut(t).unwrap().mixer.sends[0] = send_to(Route::Hardware("headphones".into()));
    s.track_mut(t).unwrap().mixer.sends[1] = send_to(Route::Hardware("Out 9-10".into()));
    // A stereo device: "Out 9-10" and "Headphones" do not exist.
    let io = IoMap::for_device("test", "test", 2, 2);
    assert_eq!(unresolved_routes(&s, &io, 2), ["Headphones", "Out 9-10"], "once each, in track order, case-insensitively");
    let r = render(&s, &io, None);
    assert_eq!(r.channels.len(), 2, "no sink for a name the device lacks");
    // Main carries Dry and Sender (0.5 each, hard-panned); the cue never reaches it.
    assert!((r.channels[0][AT] - 1.0).abs() < 1e-4, "{}", r.channels[0][AT]);
    // The wide device resolves "Out 9-10" and still lists "Headphones".
    assert_eq!(unresolved_routes(&s, &wide_io(), 10), ["Headphones"]);
    // A path whose channels the device did not open is unresolved too, not played on another channel.
    let mut io = wide_io();
    io.add_path(IoPath { name: "Headphones".into(), direction: IoDirection::Output, channels: vec![8], format: ChannelFormat::Mono }).unwrap();
    assert!(unresolved_routes(&s, &io, 10).is_empty());
    assert_eq!(unresolved_routes(&s, &io, 4), ["Headphones", "Out 9-10"]);
    // An input path is not an output.
    io.add_path(IoPath { name: "Mic".into(), direction: IoDirection::Input, channels: vec![0], format: ChannelFormat::Mono }).unwrap();
    let mut s2 = s.clone();
    s2.tracks[0].mixer.output = Route::Hardware("Mic".into());
    assert!(unresolved_routes(&s2, &io, 10).contains(&"Mic".to_string()));
}

#[test]
fn only_main_falls_back_and_a_path_table_without_routes_adds_nothing() {
    // A mixed session with sends and a bus, no path routes, on a 10-channel device: the plan renders
    // exactly what the offline oracle does (the stereo-only session is unchanged).
    let mut s = session();
    let b = bus(&mut s, "Verb");
    let t = impulse_track(&mut s, "A", Route::Main);
    s.track_mut(t).unwrap().mixer.sends[0] = send_to(Route::Bus(b));
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(delay(40));
    let a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Verb return"));
    let tr = s.track_mut(a).unwrap();
    tr.mixer.input = Route::Bus(b);
    tr.mixer.pan = vec![-1.0, 1.0];
    impulse_track(&mut s, "B", Route::Main);
    let r = render(&s, &wide_io(), None);
    assert_eq!(r.channels.len(), 2);
    assert!(plan_sinks(&s, &wide_io(), 10).sinks.is_empty());
    let want = render_range(&s, Range::new(0, LEN as i64), 512);
    for (c, want) in want.iter().enumerate().take(2) {
        let got = &r.channels[c][r.latency..r.latency + LEN];
        assert!(got.iter().zip(want).all(|(x, y)| x.to_bits() == y.to_bits()), "channel {c} differs from the oracle");
    }
}

#[test]
fn an_offline_render_drops_a_path_route_without_panicking() {
    let mut s = session();
    impulse_track(&mut s, "Cue", Route::Hardware("Out 3-4".into()));
    let out = render_range(&s, Range::new(0, LEN as i64), 512);
    assert!(out.iter().all(|c| c.iter().all(|x| *x == 0.0)), "dropped, as before");
}

/// A mono track that monitors its input route (the audio host feeds `input`).
fn monitored(s: &mut Session, name: &str, input: Route) {
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some(name));
    let tr = s.track_mut(t).unwrap();
    tr.mixer.input = input;
    tr.mixer.input_monitor = true;
}

#[test]
fn an_input_path_by_name_monitors_its_channel() {
    // Eight inputs, each channel a distinct constant: channel c carries (c + 1) / 10.
    let input: Vec<Vec<f32>> = (0..8).map(|c| vec![(c + 1) as f32 / 10.0; BLOCK]).collect();
    let mut io = IoMap::for_device("test", "test", 8, 2);
    io.add_path(IoPath { name: "Guitar".into(), direction: IoDirection::Input, channels: vec![4], format: ChannelFormat::Mono }).unwrap();
    let heard = |name: &str| {
        let mut s = session();
        monitored(&mut s, "Mon", Route::Hardware(name.into()));
        // One block is enough (monitoring ignores the clips and the transport).
        let r = render(&s, &io, Some(input.clone()));
        r.channels[0][100]
    };
    let ch = |c: usize| (c + 1) as f32 / 10.0;
    let pan = heard("In 1") / ch(0);
    assert!(pan > 0.5, "the mono strip reaches Main");
    assert!((heard("Guitar") / ch(4) - pan).abs() < 1e-5, "Guitar is input channel 5, not channel 1");
    assert!((heard("guitar") / ch(4) - pan).abs() < 1e-5);
    // Names the table knows by label, and names it does not (an old session): the number in the label.
    assert!((heard("In 3") / ch(2) - pan).abs() < 1e-5);
    assert!((heard("In 7") / ch(6) - pan).abs() < 1e-5);
}

#[test]
fn a_key_edge_keeps_main_and_a_stem_aligned() {
    // A latent kick (outside the mix) keys the bass's compressor, so the bass waits for the key;
    // the device latency covers that wait, and Main and Out 3-4 still leave together.
    let keyed = |kick: usize, bass: usize| {
        let mut s = session();
        let k = impulse_track(&mut s, "Kick", Route::None);
        s.track_mut(k).unwrap().mixer.inserts[0] = Some(delay(kick));
        let b = impulse_track(&mut s, "Bass", Route::Main);
        let mut comp = Insert::new("compressor");
        comp.key = Some(KeySource { from: KeyFrom::Track(k), tap: Tap::Post });
        let bt = s.track_mut(b).unwrap();
        bt.mixer.inserts[0] = Some(delay(bass));
        bt.mixer.inserts[1] = Some(comp);
        impulse_track(&mut s, "Stem", Route::Hardware("Out 3-4".into()));
        s
    };
    // The key is late: the kick's 128 against the bass's 16 ahead of the compressor.
    let r = render(&keyed(128, 16), &wide_io(), None);
    assert_eq!(r.latency, 128);
    assert_eq!(hits(&r.channels[0]), [AT + 128], "the bass is on the device with the key's latency");
    assert_eq!(hits(&r.channels[r.main]), [AT + 128], "the stem leaves with Main");
    // The key is early: the bass's 200 against the kick's 32.
    let r = render(&keyed(32, 200), &wide_io(), None);
    assert_eq!(r.latency, 200);
    assert_eq!(hits(&r.channels[0]), [AT + 200]);
    assert_eq!(hits(&r.channels[r.main]), [AT + 200]);
}
