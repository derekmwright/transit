//! Audio Units on a Mac: `mix.insert` names one of Apple's own, which ship with every Mac. This is an
//! integration test because the engine's unit tests never reach the real plug-in hosts (they treat
//! every third-party id as unknown), so the lookup only works in a build without `cfg(test)`.
#![cfg(target_os = "macos")]

use serde_json::json;
use soundcraft_engine::Engine;

#[test]
fn audio_units_insert_by_id() {
    let mut e = Engine::default();
    e.execute("track.new", &json!({})).unwrap();
    let t = e.session().tracks[0].id.0;
    let r = e.execute("mix.insert", &json!({"track": t, "plugin": "au:aufx:bpas:appl"}));
    assert_eq!(r.map(|v| v["plugin"].clone()), Ok(json!("au:aufx:bpas:appl")), "Apple AUBandpass");
}
