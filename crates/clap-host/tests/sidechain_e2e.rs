//! ADR 0015 slice (c): the sidechain key reaches a hosted CLAP plug-in's non-main input port.
//! The fixture's sidechain plug-in multiplies its main input by the key's RMS over the block, and
//! writes 0.125 when the key port's `constant_mask` says the key is silent.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;

mod common;
use common::{fixture_dir, sidechain_fixture_dir};

const KEYED: &str = "clap:org.soundcraft.test.sidechain";
const PLAIN: &str = "clap:org.soundcraft.test.gain";
const FRAMES: usize = 64;

fn plugin(id: &str) -> Box<dyn Plugin> {
    sidechain_fixture_dir();
    fixture_dir();
    let mut p = soundcraft_clap_host::create(id).unwrap();
    p.prepare(48_000.0, FRAMES, 2);
    p
}

fn signal(v: f32) -> Vec<Vec<f32>> {
    vec![vec![v; FRAMES]; 2]
}

#[test]
fn the_key_reaches_the_sidechain_port() {
    let mut p = plugin(KEYED);
    assert!(p.has_key_input(), "a non-main input port is a sidechain");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.4).abs() < 1e-6), "main 0.8 times key RMS 0.5");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.25), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.2).abs() < 1e-6), "the output follows the key");
}

#[test]
fn a_mono_key_feeds_every_channel_and_a_stereo_key_keeps_its_own() {
    let mut p = plugin(KEYED);
    let mut io = signal(1.0);
    p.process_with_key(&mut io, &[vec![0.5; FRAMES]], FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.5).abs() < 1e-6), "mono key onto both channels");
    let mut io = signal(1.0);
    p.process_with_key(&mut io, &[vec![0.5; FRAMES], vec![0.25; FRAMES]], FRAMES);
    assert!(io[0].iter().all(|&x| (x - 0.5).abs() < 1e-6) && io[1].iter().all(|&x| (x - 0.25).abs() < 1e-6));
}

#[test]
fn unrouted_the_port_is_silent_with_constant_mask_set_and_the_key_comes_back() {
    let mut p = plugin(KEYED);
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    let mut io = signal(0.8);
    p.process(&mut io, FRAMES);
    assert!(io.iter().flatten().all(|&x| x == 0.125), "no key: the plug-in sees the constant (silent) flag");
    let mut io = signal(0.8);
    p.process_with_key(&mut io, &signal(0.5), FRAMES);
    assert!(io.iter().flatten().all(|&x| (x - 0.4).abs() < 1e-6), "routing the key again clears the flag");
}

#[test]
fn blocks_longer_than_the_activated_size_walk_the_key() {
    let mut p = plugin(KEYED);
    let n = FRAMES * 2;
    let mut io = vec![vec![1.0; n]; 2];
    // Two chunks of FRAMES: the first quiet, the second loud.
    let key: Vec<Vec<f32>> = (0..2).map(|_| (0..n).map(|i| if i < FRAMES { 0.25 } else { 1.0 }).collect()).collect();
    p.process_with_key(&mut io, &key, n);
    assert!(io[0][..FRAMES].iter().all(|&x| (x - 0.25).abs() < 1e-6));
    assert!(io[0][FRAMES..].iter().all(|&x| (x - 1.0).abs() < 1e-6));
}

#[test]
fn a_plugin_without_a_sidechain_ignores_the_key() {
    let mut a = plugin(PLAIN);
    let mut b = plugin(PLAIN);
    assert!(!a.has_key_input());
    let mut x = signal(0.6);
    let mut y = signal(0.6);
    a.process(&mut x, FRAMES);
    b.process_with_key(&mut y, &signal(0.9), FRAMES);
    assert_eq!(x, y);
}
