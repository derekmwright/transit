//! ADR 0015 slice (c): a sidechain key reaches a hosted plug-in. The bass track's insert is the
//! CLAP fixture's sidechain plug-in (its output is the bass times the key's RMS over the block,
//! and 0.125 when the key is unrouted), keyed from a kick track that is not in the mix.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{
    Adoption, DeviceGeneration, InstanceSpec, MixEngine, NewInstance, PlanCompiler, PlanGeneration, PlanLimits, RenderTarget, create_instance,
};
use soundcraft_model::{ChannelFormat, Clip, Insert, KeyFrom, KeySource, Route, Session, SourceAudio, SourceId, Tap, TrackId, TrackKind};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use transit_rt::testing::{AllocStats, CountingAlloc, measure};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const HOSTED: &str = "clap:org.soundcraft.test.sidechain";
const SR: u32 = 48_000;
const BLOCK: usize = 256;
const LEN: usize = 24_000;
const KICKS: [usize; 3] = [3_000, 10_000, 17_000];
const BURST: usize = 600;
const BASS: f32 = 0.5;

fn fixture() {
    static INSTALL: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALL.get_or_init(|| {
        soundcraft_clap_host::set_scan_extra_dirs_only(true);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sidechain-hosted-fixture-target");
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--quiet", "--features", "sidechain", "--manifest-path"])
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .unwrap();
        assert!(status.success());
        let lib = if cfg!(target_os = "macos") {
            "libclap_fixture.dylib"
        } else if cfg!(windows) {
            "clap_fixture.dll"
        } else {
            "libclap_fixture.so"
        };
        let dir = target.join(format!("plugins-{}", std::process::id()));
        let dest = if cfg!(target_os = "macos") { dir.join("Fixture.clap/Contents/MacOS/Fixture") } else { dir.join("Fixture.clap") };
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(target.join("debug").join(lib), dest).unwrap();
        assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 1);
    });
}

fn audio(s: &mut Session, name: &str, sig: Vec<f32>, output: Route) -> TrackId {
    let src = SourceId(s.next_id + 10_000);
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR, channels: vec![sig.clone(), sig] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
    let cid = s.new_clip_id();
    let tr = s.track_mut(t).unwrap();
    tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "c", src, 0, 0, LEN as i64));
    tr.mixer.pan = vec![-1.0, 1.0];
    tr.mixer.output = output;
    t
}

fn session(key: bool) -> Session {
    let mut s = Session::default();
    s.edit.delay_compensation = true;
    s.edit.click = false;
    let mut kick = vec![0.0; LEN];
    for k in KICKS {
        kick[k..k + BURST].fill(0.9);
    }
    let kick = audio(&mut s, "Kick", kick, Route::None);
    let bass = audio(&mut s, "Bass", vec![BASS; LEN], Route::Main);
    let mut insert = Insert::new(HOSTED);
    insert.key = key.then_some(KeySource { from: KeyFrom::Track(kick), tap: Tap::Post });
    s.track_mut(bass).unwrap().mixer.inserts[0] = Some(insert);
    s
}

fn target() -> RenderTarget {
    RenderTarget { device: DeviceGeneration(1), sample_rate: SR, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 }
}

/// Renders the main mix (left channel); the render itself is measured for allocations.
fn render(s: &Session) -> (Vec<f32>, AllocStats) {
    fixture();
    soundcraft_mix::mark_audio_thread();
    let mut mix = MixEngine::realtime(target());
    let mut compiler = PlanCompiler::new(target(), PlanLimits::default());
    let mut make = |spec: &InstanceSpec, _: soundcraft_mix::SlotGen| {
        create_instance(&spec.id, spec.state.as_deref(), SR as f32, BLOCK, spec.ch).map(|n| NewInstance { plugin: n.plugin, refused_state: false })
    };
    let mut env = compiler.compile(Arc::new(s.clone()), compiler.active(), PlanGeneration::from_raw(1), &mut make).unwrap();
    assert_eq!(mix.adopt_plan(&mut env), Adoption::Adopted);
    compiler.reclaim(env);
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut all = Vec::with_capacity(LEN + BLOCK);
    let ((), stats) = measure(|| {
        let mut pos = 0i64;
        while all.len() < LEN {
            mix.render_active(pos, BLOCK, &mut out);
            all.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
    });
    (all, stats)
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

#[test]
fn a_kick_keyed_into_a_hosted_plugin_drives_it_and_allocates_nothing() {
    let (out, stats) = render(&session(true));
    for k in KICKS {
        let before = peak(&out[k - 800..k - 300]);
        assert!(before < 1e-3, "no key signal before the kick at {k}: {before}");
        let inside = peak(&out[k + 300..k + BURST]);
        assert!(inside > BASS * 0.5, "the kick opens the hosted plug-in at {k}: {inside}");
    }
    assert_eq!(stats, AllocStats::default(), "the hosted keyed path must not allocate or free");
}

#[test]
fn an_unkeyed_hosted_plugin_sees_the_silent_key_flag() {
    let (out, stats) = render(&session(false));
    // The fixture writes its 0.125 marker whenever the key port is flagged constant; the bass
    // input never matters, so every block is the same level.
    let a = peak(&out[2_000..2_400]);
    let b = peak(&out[3_100..3_500]);
    assert!(a > 0.01 && (a - b).abs() < 1e-6, "level {a} vs {b}");
    assert_eq!(stats, AllocStats::default());
}
