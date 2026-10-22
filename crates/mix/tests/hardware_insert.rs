//! ADR 0015 slice (d): hardware inserts. A fake duplex device plays the sinks back on its input a
//! known number of frames later, so a hardware insert is a delay of that many frames. With the
//! round trip measured the insert reports it and delay compensation lines the strip up with a
//! parallel dry track, sample for sample. Unmeasured, the insert reports nothing and the strip is
//! late by the round trip (documented). With no capture stream the insert passes the signal
//! through dry.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, HardwareState, InstanceSpec, MixEngine, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, hardware_report,
    plan_sinks, unresolved_routes,
};
use soundcraft_model::{ChannelFormat, Clip, HardwareLatency, Insert, IoMap, Session, SourceAudio, SourceId, TrackId, TrackKind};
use std::sync::Arc;

const SR: u32 = 48_000;
const BLOCK: usize = 256;
const AT: usize = 2400;
const LEN: usize = 8_192;
/// The fake device's round trip: more than a block, not a multiple of one.
const D: usize = 700;
const SEND: &str = "Out 3-4";
const RET: &str = "In 3-4";

fn session() -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    s
}

/// A stereo audio track playing a 0.5 impulse at `AT` on both channels.
fn impulse_track(s: &mut Session, name: &str) -> TrackId {
    let mut sig = vec![0.0f32; LEN];
    sig[AT] = 0.5;
    let src = SourceId(s.next_id + 10_000);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "imp", src, 0, 0, LEN as i64));
    tr.mixer.pan = vec![-1.0, 1.0];
    t
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 4, in_channels: 4 }
}

fn io(measured: Option<usize>) -> IoMap {
    let mut io = IoMap::for_device("test", "fake duplex", 4, 4);
    if let Some(frames) = measured {
        io.set_hardware_latency(HardwareLatency { send: SEND.into(), ret: RET.into(), round_trip_frames: frames as u32, rate: SR, measured_at: 1 });
    }
    io
}

struct Rendered {
    /// Main's first channel from sample 0 of the render.
    main: Vec<f32>,
    latency: usize,
    /// What the device played on its channel 2 (the first channel of "Out 3-4").
    sent: Vec<f32>,
}

/// Renders `s` on the fake device. `duplex` feeds the input from the device output `delay` frames
/// earlier; otherwise the mix gets no input at all.
fn render(s: &Session, io: &IoMap, duplex: bool, delay: usize) -> Rendered {
    MixEngine::force_serial(true);
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    compiler.set_io(Arc::new(io.clone()));
    let mut maker = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| {
        soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch)
            .map(|n| soundcraft_mix::NewInstance { plugin: n.plugin, refused_state: false })
    };
    let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut maker).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let sinks = plan_sinks(s, io, 4);
    let dest = sinks.destinations();
    let main_w = soundcraft_mix::main_channels(s);
    let mut out = vec![vec![0.0f32; BLOCK]; main_w + sinks.channels()];
    // The device's output stream, per device channel, from sample 0.
    let mut device: Vec<Vec<f32>> = vec![Vec::new(); 4];
    let (mut main, mut pos, mut latency) = (Vec::new(), 0usize, 0usize);
    while main.len() < LEN + latency + 2 * D {
        mix.input = if duplex {
            (0..4)
                .map(|c| (0..BLOCK).map(|i| (pos + i).checked_sub(delay).and_then(|k| device[c].get(k)).copied().unwrap_or(0.0)).collect())
                .collect()
        } else {
            Vec::new()
        };
        mix.render_active(pos as i64, BLOCK, &mut out);
        latency = mix.latency;
        main.extend_from_slice(&out[0]);
        for c in &mut device {
            c.extend(std::iter::repeat_n(0.0, BLOCK));
        }
        for (k, &d) in dest.iter().enumerate() {
            for i in 0..BLOCK {
                device[usize::from(d)][pos + i] += out[main_w + k][i];
            }
        }
        pos += BLOCK;
    }
    assert_eq!(mix.status().capacity_exceeded, 0, "the plan's delay lines hold the round trip");
    Rendered { main, latency, sent: device.swap_remove(2) }
}

fn hits(x: &[f32]) -> Vec<usize> {
    x.iter().enumerate().filter(|(_, v)| v.abs() > 1e-3).map(|(i, _)| i).collect()
}

fn dry_baseline() -> (f32, usize) {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let r = render(&s, &io(None), false, 0);
    let h = hits(&r.main);
    (r.main[h[0]], h[0])
}

#[test]
fn the_insert_delays_the_strip_by_the_round_trip_and_sends_the_signal_out() {
    let (level, at) = dry_baseline();
    assert_eq!(at, AT);
    let mut s = session();
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!(r.latency, D, "the insert reports the measured round trip");
    assert_eq!(hits(&r.main), [AT + D], "the strip output is its input delayed by the round trip");
    assert_eq!(r.main[AT + D], level, "and nothing else changed: unity through the send and the return");
    assert_eq!(hits(&r.sent), [AT], "the signal left on the send path at its own time");
    assert_eq!(r.sent[AT], 0.5);
}

#[test]
fn compensation_lines_the_insert_up_with_a_parallel_dry_track() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!(hits(&r.main), [AT + D], "both tracks land on the same sample");
    // The two sum: a dry track alone would be half as loud.
    let (level, _) = dry_baseline();
    assert!((r.main[AT + D] - 2.0 * level).abs() < 1e-6);
    // Compensation off: no alignment and no latency reported.
    s.edit.delay_compensation = false;
    let off = render(&s, &io(Some(D)), true, D);
    assert_eq!(hits(&off.main), [AT, AT + D]);
}

#[test]
fn an_unmeasured_insert_reports_nothing_and_the_strip_is_late() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    let r = render(&s, &io(None), true, D);
    assert_eq!(r.latency, 0);
    assert_eq!(hits(&r.main), [AT, AT + D], "uncompensated: the outboard track is late by the round trip");
    let report = hardware_report(&s, &io(None), 4, 4, true, SR);
    assert_eq!(report.len(), 1);
    assert_eq!((report[0].state.clone(), report[0].latency), (HardwareState::Unmeasured, None));
    let report = hardware_report(&s, &io(Some(D)), 4, 4, true, SR);
    assert_eq!((report[0].state.clone(), report[0].latency), (HardwareState::Ready, Some(D as u32)));
}

#[test]
fn without_a_capture_stream_the_insert_passes_the_signal_through_dry() {
    let mut s = session();
    impulse_track(&mut s, "Dry");
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    let r = render(&s, &io(Some(D)), false, 0);
    assert_eq!(r.latency, 0, "an inactive insert adds no latency");
    assert_eq!(hits(&r.main), [AT]);
    assert!(hits(&r.sent).is_empty(), "nothing is sent while the insert is inactive");
    let report = hardware_report(&s, &io(Some(D)), 4, 0, false, SR);
    assert_eq!(report[0].state, HardwareState::NoInput);
}

#[test]
fn bypass_and_missing_paths_pass_the_signal_through_dry() {
    let mut s = session();
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().bypass = true;
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!((hits(&r.main), r.latency), (vec![AT], 0));
    assert!(hits(&r.sent).is_empty());
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware("Nowhere", RET));
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!(hits(&r.main), [AT]);
    assert_eq!(unresolved_routes(&s, &io(None), 4), ["Nowhere"]);
    assert_eq!(hardware_report(&s, &io(None), 4, 4, true, SR)[0].state, HardwareState::MissingSend);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, "In 9"));
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!(hits(&r.main), [AT]);
    assert_eq!(hardware_report(&s, &io(None), 4, 4, true, SR)[0].state, HardwareState::MissingReturn);
}

#[test]
fn the_send_path_is_a_sink_of_the_plan() {
    let mut s = session();
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    let p = plan_sinks(&s, &io(None), 4);
    assert_eq!(p.sinks.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), [SEND]);
    assert_eq!(p.destinations(), [2, 3]);
}

#[test]
fn a_disabled_insert_passes_the_signal_through_dry_claims_no_sink_and_reports_why() {
    let mut s = session();
    let t = impulse_track(&mut s, "Outboard");
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::hardware(SEND, RET));
    s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().hardware.as_mut().unwrap().disabled = Some("send path in use by Other".into());
    let r = render(&s, &io(Some(D)), true, D);
    assert_eq!((hits(&r.main), r.latency), (vec![AT], 0), "dry, no latency");
    assert!(hits(&r.sent).is_empty(), "nothing is sent");
    assert!(plan_sinks(&s, &io(None), 4).sinks.is_empty(), "a disabled insert's send path is no sink");
    let report = hardware_report(&s, &io(Some(D)), 4, 4, true, SR);
    assert_eq!((report[0].state.clone(), report[0].latency), (HardwareState::Disabled("send path in use by Other".into()), None));
}
