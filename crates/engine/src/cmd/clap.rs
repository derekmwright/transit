//! Third-party CLAP plugins: discovery (`engine.clap_plugins`) and id lookup shared by the mixer
//! commands. Hosting itself lives in `soundcraft-clap-host`.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_dsp::PluginInfo;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.clap_plugins",
        "List CLAP Plugins",
        [],
        None,
        "{rescan?: bool} → [{id: \"clap:…\", plugin_id, name, vendor, version, description, features, category, is_instrument, path}]",
        always,
        |_, p| {
            // Unit tests never scan the machine's plug-in folders: that loads every installed
            // plug-in in-process, and some never unload, which hangs the test binary at exit.
            #[cfg(test)]
            {
                let _ = p;
                Ok(json!([]))
            }
            #[cfg(not(test))]
            {
            let list = if p.get("rescan").and_then(Value::as_bool).unwrap_or(false) {
                soundcraft_clap_host::rescan()
            } else {
                soundcraft_clap_host::scan()
            };
            Ok(json!(list))
            }
        }
    )]
}

/// A built-in plugin's description, else a hosted CLAP (`clap:<id>`), VST3 (`vst3:<class id>`) or
/// Audio Units (`au:<type>:<subtype>:<manufacturer>`) plugin's.
pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    // Tests never reach the real hosts (see `soundcraft_mix::create_plugin`): a lookup scans the
    // machine's plug-in folders, and some plug-ins never unload.
    #[cfg(test)]
    if soundcraft_mix::is_third_party(id) {
        return None;
    }
    soundcraft_dsp::plugin_info(id)
        .or_else(|| soundcraft_clap_host::plugin_info(id))
        .or_else(|| soundcraft_vst3_host::plugin_info(id))
        .or_else(|| soundcraft_au_host::plugin_info(id))
}

/// What adding a plug-in needs to know of it: its id and whether it is an instrument, and its full
/// description when `params` is set (a command that names parameters). A hosted plug-in's
/// summary is its scan result; the full description ([`plugin_info`]) creates an instance, which
/// blocks the thread running the command for as long as the plug-in takes to load, and adding the
/// plug-in to a session has no use for it: the playback runtime creates the instance itself, one
/// frame later, with the UI saying so.
pub fn plugin_head(id: &str, params: bool) -> Option<(String, bool, Option<&'static PluginInfo>)> {
    if let Some(info) = soundcraft_dsp::plugin_info(id) {
        return Some((info.id.to_string(), info.is_instrument, Some(info)));
    }
    #[cfg(test)]
    if soundcraft_mix::is_third_party(id) {
        return None;
    }
    if !params {
        let scanned = if id.starts_with(soundcraft_clap_host::ID_PREFIX) {
            soundcraft_clap_host::cached().and_then(|all| all.into_iter().find(|d| d.id == id).map(|d| (d.id, d.is_instrument)))
        } else if id.starts_with(soundcraft_vst3_host::ID_PREFIX) {
            soundcraft_vst3_host::cached().and_then(|all| all.into_iter().find(|d| d.id == id).map(|d| (d.id, d.is_instrument)))
        } else {
            None
        };
        if let Some((id, is_instrument)) = scanned {
            return Some((id, is_instrument, None));
        }
    }
    plugin_info(id).map(|info| (info.id.to_string(), info.is_instrument, Some(info)))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn a_plugins_head_names_it_without_the_hosts() {
        let (id, instrument, info) = super::plugin_head("eq_7band", false).unwrap();
        assert_eq!((id.as_str(), instrument), ("eq_7band", false));
        assert!(info.is_some(), "a built-in always has its description");
        assert!(super::plugin_head("clap:no.such.plugin", false).is_none());
        assert!(super::plugin_head("vst3:00000000000000000000000000000000", true).is_none());
    }

    #[test]
    fn clap_plugins_query_returns_a_list() {
        let mut e = Engine::default();
        let v = e.execute("engine.clap_plugins", &json!({})).unwrap();
        assert!(v.is_array());
    }

    #[test]
    fn unknown_clap_insert_is_an_error() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let r = e.execute("mix.insert", &json!({"track": t, "plugin": "clap:no.such.plugin"}));
        assert!(r.is_err());
        assert!(super::plugin_info("eq_7band").is_some());
        assert!(super::plugin_info("clap:no.such.plugin").is_none());
    }

    /// Apple's own Audio Units ship with every Mac, so `mix.insert` can be checked against one.
    #[cfg(target_os = "macos")]
    #[test]
    fn audio_units_insert_by_id() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let r = e.execute("mix.insert", &json!({"track": t, "plugin": "au:aufx:bpas:appl"}));
        assert_eq!(r.map(|v| v["plugin"].clone()), Ok(json!("au:aufx:bpas:appl")), "Apple AUBandpass");
    }
}
