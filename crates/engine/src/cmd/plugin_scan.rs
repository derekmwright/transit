//! The external discovery boundary; commands use the same dispatch path in every build.
use super::*;
use serde_json::json;

pub(crate) struct PluginDiscovery {
    pub clap: fn(bool) -> Value,
    pub vst3: fn(bool) -> Value,
}

impl Default for PluginDiscovery {
    fn default() -> Self {
        #[cfg(not(test))]
        {
            Self {
                clap: |rescan| json!(if rescan { soundcraft_clap_host::rescan() } else { soundcraft_clap_host::scan() }),
                vst3: |rescan| json!(if rescan { soundcraft_vst3_host::rescan() } else { soundcraft_vst3_host::scan() }),
            }
        }
        // Unit tests exercise command dispatch without loading installed third-party plug-ins.
        // The CLAP integration fixture exercises the real host adapter separately.
        #[cfg(test)]
        {
            Self { clap: |_| json!([]), vst3: |_| json!([]) }
        }
    }
}

pub(super) fn clap(e: &mut Engine, p: &Value) -> Result<Value> {
    Ok((e.plugin_discovery.clap)(bool_or(p, "rescan", false)))
}

pub(super) fn vst3(e: &mut Engine, p: &Value) -> Result<Value> {
    Ok((e.plugin_discovery.vst3)(bool_or(p, "rescan", false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_commands_dispatch_both_modes_to_the_discovery_boundary_issue_10() {
        let mut e = Engine {
            plugin_discovery: PluginDiscovery {
                clap: |rescan| json!([{ "format": "clap", "rescan": rescan }]),
                vst3: |rescan| json!([{ "format": "vst3", "rescan": rescan }]),
            },
            ..Engine::default()
        };
        for format in ["clap", "vst3"] {
            for (params, rescan) in [(json!({}), false), (json!({"rescan": false}), false), (json!({"rescan": true}), true)] {
                let result = e.execute(&format!("engine.{format}_plugins"), &params).unwrap();
                assert_eq!(result, json!([{ "format": format, "rescan": rescan }]));
            }
        }
    }
}
