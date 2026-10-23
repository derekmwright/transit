//! ADR 0018 section 3: an adopted instance's forwarder adds no allocation to the audio side. The
//! render graph calls these on the audio thread; each must allocate and free nothing (the counting
//! allocator of `transit_rt::testing`).
#![allow(clippy::unwrap_used)]

mod common;

use common::{Fake, Log};
use soundcraft_dsp::Transport;
use soundcraft_dsp::event::{Message, NoteId, PluginEvent};
use std::sync::Arc;
use std::time::Duration;
use transit_plugin_host::{Config, Host};

#[global_allocator]
static ALLOC: transit_rt::testing::CountingAlloc = transit_rt::testing::CountingAlloc;

#[test]
fn the_forwarder_allocates_nothing_on_the_audio_side() {
    let host = Host::new(Config { inline: false, ..Config::default() });
    let log = Arc::new(Log::default());
    let l = Arc::clone(&log);
    let mut plugin = host.submit(move |ctx| ctx.adopt(Fake::make(&l, Duration::ZERO, Duration::ZERO))).wait(Duration::from_secs(5)).unwrap();
    plugin.prepare(48_000.0, 64, 2);
    let mut io = vec![vec![0.5f32; 64]; 2];
    let key = vec![vec![0.0f32; 64]; 2];
    let ev = PluginEvent { channel: 0, note_id: PluginEvent::NO_ID, message: Message::NoteOn { key: 60, velocity: 100, id: NoteId::NONE } };
    let transport = Transport::default();
    // Warm once (first-use effects belong to no block).
    plugin.process(&mut io, 64);
    let ((), stats) = transit_rt::testing::measure(|| {
        for _ in 0..100 {
            plugin.set_transport(&transport);
            plugin.set_input_silent(false);
            let _ = plugin.set_param("gain", 0.5);
            let _ = plugin.set_param_at(3, "gain", 0.25);
            let _ = plugin.event(0, &ev);
            plugin.process(&mut io, 64);
            plugin.process_with_key(&mut io, &key, 64);
            let _ = (plugin.latency(), plugin.tail_samples(), plugin.process_status(), plugin.gain_reduction_db(), plugin.info());
        }
    });
    assert!(stats.is_zero(), "the forwarder allocated on the audio side: {stats:?}");
    assert!(io[0][0] < 0.25, "the instance did the processing");
    drop(plugin);
    assert!(host.shutdown(Duration::from_secs(5)));
}
