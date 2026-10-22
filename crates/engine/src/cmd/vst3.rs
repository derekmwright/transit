//! Third-party VST3 plugins: discovery (`engine.vst3_plugins`). Hosting itself lives in
//! `soundcraft-vst3-host`; id lookup for the mixer commands is shared with CLAP in
//! `cmd::clap::plugin_info`.

use super::*;
use crate::cmd;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.vst3_plugins",
        "List VST3 Plugins",
        [],
        None,
        "{rescan?: bool} → [{id: \"vst3:<class id hex>\", class_id, name, vendor, version, sdk_version, sub_categories, category, is_instrument, path}]",
        always,
        super::plugin_scan::vst3
    )]
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn vst3_plugins_query_returns_a_list() {
        let mut e = Engine::default();
        let v = e.execute("engine.vst3_plugins", &json!({})).unwrap();
        assert!(v.is_array());
    }

    #[test]
    fn unknown_vst3_insert_is_an_error() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let id = "vst3:00000000000000000000000000000000";
        assert!(e.execute("mix.insert", &json!({"track": t, "plugin": id})).is_err());
        assert!(e.execute("mix.insert", &json!({"track": t, "plugin": "vst3:not-hex"})).is_err());
        assert!(crate::cmd::clap::plugin_info(id).is_none());
    }
}
