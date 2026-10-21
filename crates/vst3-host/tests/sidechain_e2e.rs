//! ADR 0015 slice (c): the sidechain key reaches a hosted VST3 plug-in's auxiliary input bus.
//! The fixture's "Fixture Sidechain" multiplies its main input by the key's RMS over the block, and
//! writes 0.125 for a key channel whose silence flag is set; "Fixture Sidechain Picky" refuses
//! every bus arrangement.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;

mod common;
use common::{fixture_dir, sidechain_fixture_dir};

const FRAMES: usize = 64;

fn plugin(dir: &std::path::Path, name: &str) -> Box<dyn Plugin> {
    let found = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]);
    let id = found.into_iter().find(|d| d.name == name).unwrap().id;
    let mut p = soundcraft_vst3_host::create(&id).unwrap();
    p.prepare(48_000.0, FRAMES, 2);
    p
}

fn keyed() -> Box<dyn Plugin> {
    plugin(sidechain_fixture_dir(), "Fixture Sidechain")
}

fn signal(v: f32) -> Vec<Vec<f32>> {
    vec![vec![v; FRAMES]; 2]
}

#[test]
fn the_key_reaches_the_aux_bus() {
    let mut p = keyed();
    assert!(p.has_key_input(), "an aux input bus is a sidechain");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.4).abs() < 1e-6), "main 0.8 times key RMS 0.5");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.25), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.2).abs() < 1e-6), "the output follows the key");
}

#[test]
fn a_mono_key_feeds_every_channel_and_a_stereo_key_keeps_its_own() {
    let mut p = keyed();
    let mut io = signal(1.0);
    p.process_with_key(&mut io, &[vec![0.5; FRAMES]], FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.5).abs() < 1e-6), "mono key onto both channels");
    let mut io = signal(1.0);
    p.process_with_key(&mut io, &[vec![0.5; FRAMES], vec![0.25; FRAMES]], FRAMES);
    assert!(io[0].iter().all(|&x| (x - 0.5).abs() < 1e-6) && io[1].iter().all(|&x| (x - 0.25).abs() < 1e-6));
}

#[test]
fn unrouted_the_aux_bus_is_silent_with_the_flags_set_and_the_key_comes_back() {
    let mut p = keyed();
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    let mut io = signal(0.8);
    p.process(&mut io, FRAMES);
    assert!(io.iter().flatten().all(|&x| x == 0.125), "no key: the plug-in sees the silence flags");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.4).abs() < 1e-6), "routing the key again clears the flags");
}

#[test]
fn blocks_longer_than_the_prepared_size_walk_the_key() {
    let mut p = keyed();
    let n = FRAMES * 2;
    let mut io = vec![vec![1.0; n]; 2];
    let key: Vec<Vec<f32>> = (0..2).map(|_| (0..n).map(|i| if i < FRAMES { 0.25 } else { 1.0 }).collect()).collect();
    p.process_with_key(&mut io, &key, n);
    assert!(io[0][..FRAMES].iter().all(|&x| (x - 0.25).abs() < 1e-6));
    assert!(io[0][FRAMES..].iter().all(|&x| (x - 1.0).abs() < 1e-6));
}

#[test]
fn a_plugin_that_refuses_the_aux_arrangement_gets_no_key() {
    let mut p = plugin(sidechain_fixture_dir(), "Fixture Sidechain Picky");
    assert!(!p.has_key_input(), "the aux bus stays inactive");
    let mut a = signal(0.6);
    let mut b = signal(0.6);
    p.process(&mut a, FRAMES);
    p.process_with_key(&mut b, &signal(0.9), FRAMES);
    assert_eq!(a, b, "processed as before");
    assert!(a.iter().flatten().all(|&x| (x - 0.6).abs() < 1e-6));
}

#[test]
fn a_plugin_without_a_sidechain_ignores_the_key() {
    let mut a = plugin(fixture_dir(), "Fixture Gain");
    let mut b = plugin(fixture_dir(), "Fixture Gain");
    assert!(!a.has_key_input());
    let mut x = signal(0.6);
    let mut y = signal(0.6);
    a.process(&mut x, FRAMES);
    b.process_with_key(&mut y, &signal(0.9), FRAMES);
    assert_eq!(x, y);
}
