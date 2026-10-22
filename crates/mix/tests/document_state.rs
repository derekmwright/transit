//! A document restore must not inherit a matching previous project's live plug-in state.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::PluginStateReader;
use soundcraft_mix::{
    Adoption, CompileMode, DeviceGeneration, InstanceSpec, MixEngine, PlanCompiler, PlanEnvelope, PlanGeneration, PlanLimits, RenderTarget, SlotGen,
    create_instance,
};
use soundcraft_model::{ChannelFormat, Insert, Session, TrackKind};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use transit_rt::testing::{AllocStats, CountingAlloc, measure};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const GAIN: &str = "clap:org.soundcraft.test.gain";
const BLOCK: usize = 256;

fn fixture() {
    static INSTALL: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALL.get_or_init(|| {
        soundcraft_clap_host::set_scan_extra_dirs_only(true);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("document-state-fixture-target");
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--quiet", "--manifest-path"])
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
        assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 2);
    });
}

fn state(gain: f64) -> Vec<u8> {
    [gain.to_le_bytes(), 0.0f64.to_le_bytes()].concat()
}

struct Rig {
    compiler: PlanCompiler,
    mix: MixEngine,
    readers: Vec<(SlotGen, Box<dyn PluginStateReader>)>,
    next: PlanGeneration,
    out: Vec<Vec<f32>>,
}

impl Rig {
    fn new() -> Self {
        fixture();
        soundcraft_mix::mark_audio_thread();
        let target = RenderTarget { device: DeviceGeneration(3), sample_rate: 48_000, max_block: BLOCK as u32, out_channels: 0, in_channels: 0 };
        Self {
            compiler: PlanCompiler::new(target, PlanLimits::default()),
            mix: MixEngine::realtime(target),
            readers: Vec::new(),
            next: PlanGeneration::EMPTY,
            out: vec![vec![0.0; BLOCK]; 2],
        }
    }

    fn compile(&mut self, s: &Session, mode: CompileMode) -> PlanEnvelope {
        self.next = self.next.checked_next().unwrap();
        let readers = &mut self.readers;
        self.compiler
            .compile_with_mode(Arc::new(s.clone()), self.compiler.active(), self.next, mode, &mut |spec: &InstanceSpec, generation| {
                let mut instance = create_instance(&spec.id, spec.state.as_deref(), 48_000.0, BLOCK, spec.ch).unwrap();
                assert!(!instance.refused_state);
                readers.push((generation, instance.plugin.state_reader().unwrap()));
                Some(instance)
            })
            .unwrap()
    }

    fn adopt(&mut self, mut env: PlanEnvelope) {
        let target = self.mix.active().target;
        let (adopted, stats) = measure(|| {
            let result = self.mix.adopt_plan(&mut env);
            // Continue at a nonzero render position on the same device.
            for block in 0..64 {
                self.mix.render_active(48_000 + block * BLOCK as i64, BLOCK, &mut self.out);
            }
            result
        });
        assert_eq!(adopted, Adoption::Adopted);
        assert_eq!(stats, AllocStats::default(), "document adoption/render must not allocate or free");
        assert_eq!(self.mix.active().target, target);
        self.compiler.reclaim(env);
    }

    fn saved(&mut self) -> Vec<u8> {
        let generation = self.compiler.active_bindings()[0].2;
        self.readers.iter_mut().find(|(g, _)| *g == generation).unwrap().1.save_state().unwrap()
    }
}

fn session() -> Session {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let mut insert = Insert::new(GAIN);
    insert.set_state_bytes(&state(1.0));
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(insert);
    s
}

#[test]
fn new_document_restores_clap_state_while_updates_and_save_keep_the_instance() {
    let mut s = session();
    let mut rig = Rig::new();
    let env = rig.compile(&s, CompileMode::NewDocument);
    rig.adopt(env);
    assert_eq!(rig.saved(), state(1.0));
    let original = rig.compiler.active_bindings();

    // A changed saved blob is not an implicit restore during an ordinary edit.
    s.tracks[0].mixer.inserts[0].as_mut().unwrap().set_state_bytes(&state(0.25));
    let env = rig.compile(&s, CompileMode::Update);
    rig.adopt(env);
    assert_eq!(rig.saved(), state(1.0));
    assert_eq!(rig.compiler.active_bindings(), original);
    assert_eq!(rig.readers.len(), 1);

    // Same IDs and topology, but a distinct document: recreate and restore on control.
    s.name = "Second project".into();
    let env = rig.compile(&s, CompileMode::NewDocument);
    assert_eq!(rig.saved(), state(1.0), "preparing does not change the active instance");
    rig.adopt(env);
    assert_eq!(rig.saved(), state(0.25), "Save must receive the second project's state");
    assert_ne!(rig.compiler.active_bindings(), original);
    assert_eq!(rig.readers.len(), 2);
    assert_eq!(rig.compiler.take_released(), original);

    // Save captures a state snapshot and republishes it; neither resets nor recreation.
    let saved = rig.saved();
    s.tracks[0].mixer.inserts[0].as_mut().unwrap().set_state_bytes(&saved);
    let binding = rig.compiler.active_bindings();
    let env = rig.compile(&s, CompileMode::Update);
    rig.adopt(env);
    assert_eq!(rig.saved(), saved);
    assert_eq!(rig.compiler.active_bindings(), binding);
    assert_eq!(rig.readers.len(), 2);

    // Structural edits within this document keep the same live slot too.
    s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let env = rig.compile(&s, CompileMode::Update);
    rig.adopt(env);
    assert_eq!(rig.saved(), saved);
    assert_eq!(rig.compiler.active_bindings(), binding);
    assert_eq!(rig.readers.len(), 2);
}

#[test]
fn explicit_restore_refreshes_slot_identity_even_with_identical_saved_bytes_and_a_refused_plan() {
    let s = session();
    let mut rig = Rig::new();
    let env = rig.compile(&s, CompileMode::NewDocument);
    rig.adopt(env);
    let original = rig.compiler.active_bindings();
    let env = rig.compile(&s, CompileMode::NewDocument);
    let refused = rig.compiler.pending_bindings().unwrap();
    assert_ne!(refused, original, "new document slots must reject old event bindings");
    rig.compiler.reclaim(env); // Unserviced/refused publication; active document is intact.
    assert_eq!(rig.compiler.active_bindings(), original);
    assert_eq!(rig.compiler.pooled(), 1);

    let env = rig.compile(&s, CompileMode::NewDocument);
    let replacement = rig.compiler.pending_bindings().unwrap();
    assert_ne!(replacement, original);
    assert_ne!(replacement, refused, "a later restore cannot inherit the refused document's identity");
    assert_eq!(rig.readers.len(), 3);
    rig.adopt(env);
    assert_eq!(rig.compiler.active_bindings(), replacement);
    assert_eq!(rig.saved(), state(1.0));
    let released = rig.compiler.take_released();
    assert!(released.contains(&original[0]) && released.contains(&refused[0]));
}
