//! Issue #58: the automation picker lists a hosted plug-in's parameters by name from the host's
//! cached table, not the document's stored subset, and lane ids keep the parameter id's case.

use crate::automation::{Lane, choices};
use crate::{Services, StudioApp};
use serde_json::json;
use soundcraft_model::{AutoParam, Insert, Session, TrackId};
use std::process::Command;

fn app_with_track(kind: &str) -> (StudioApp, TrackId) {
    let mut app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    let t = TrackId(app.run("track.new", json!({"kind": kind, "name": "T"})).unwrap()["tracks"][0].as_u64().unwrap());
    (app, t)
}

fn listed(app: &mut StudioApp, t: TrackId) -> Vec<(String, String, String)> {
    let s = app.engine.session_arc();
    choices(app, &s, s.track(t).unwrap())
}

#[test]
fn a_hosted_plug_in_lists_every_parameter_by_name_with_none_stored() {
    // The host registry is process-global; the fixture runs in a child process (see
    // plugin_requests_tests).
    if std::env::var_os("TRANSIT_PICKER_FIXTURE_CHILD").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "automation_picker_tests::a_hosted_plug_in_lists_every_parameter_by_name_with_none_stored", "--nocapture"])
            .env("TRANSIT_PICKER_FIXTURE_CHILD", "1")
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{out}{}", String::from_utf8_lossy(&output.stderr));
        assert!(out.contains("1 passed"), "the child ran no test:\n{out}");
        return;
    }
    crate::plugin_requests_tests::requests_fixture();
    let plugin = crate::plugin_requests_tests::REQUESTS;
    let (mut app, t) = app_with_track("instrument");
    {
        let tr = app.engine.session_mut().track_mut(t).unwrap();
        tr.mixer.inserts[0] = Some(Insert::new(plugin));
        tr.instrument = Some(Insert::new(plugin));
        // One stored parameter, as a document that touched only the gain would hold.
        tr.mixer.inserts[1] = Some(Insert::new(plugin));
        tr.mixer.inserts[1].as_mut().unwrap().params.insert("7".into(), 1.0);
    }
    // Never loaded: nothing cached, so no instance is made here; the stored subset is listed by id.
    assert!(soundcraft_clap_host::cached_info(plugin).is_none());
    let before = listed(&mut app, t);
    assert!(before.iter().any(|c| c.1 == "param 7" && c.2 == "plugin:b:7"), "stored subset falls back to ids: {before:?}");
    assert!(!before.iter().any(|c| c.1 == "Gain"), "{before:?}");
    // Once an instance has loaded (as any project's plug-ins do), the table is cached.
    soundcraft_clap_host::plugin_info(plugin).expect("fixture info");
    let after = listed(&mut app, t);
    let name = app.plugin_name(plugin, false);
    for lane in ["plugin:a:7", "plugin:a:3", "plugin:b:7", "plugin:instrument:7", "plugin:instrument:3"] {
        assert!(after.iter().any(|c| c.2 == lane), "{lane} listed: {after:?}");
    }
    assert!(after.iter().any(|c| c.0 == name && c.1 == "Gain" && c.2 == "plugin:a:7"), "named, not id-labeled: {after:?}");
    assert!(after.iter().any(|c| c.1 == "Mode" && c.2 == "plugin:instrument:3"));
    assert!(!after.iter().any(|c| c.2.ends_with(":9")), "the hidden parameter stays hidden");
    // Value formatting comes from the same table.
    let s = app.engine.session_arc();
    let lane = Lane::parse("plugin:a:7").unwrap();
    assert!(lane.text(s.track(t).unwrap(), 1.0).contains('1'));
}

#[test]
fn a_mixed_case_parameter_id_gets_a_lane_that_round_trips() {
    let (mut app, t) = app_with_track("audio");
    let id = "Cutoff_Hz";
    let (_, lane) = Lane::of_param(t, 0, id).expect("a mixed-case id is automatable");
    assert_eq!(lane.id(), "plugin:a:Cutoff_Hz");
    assert_eq!(Lane::parse(&lane.id()), Some(lane.clone()));
    let inst = Lane::of_param(t, soundcraft_mix::INSTRUMENT_SLOT, id).unwrap().1;
    assert_eq!(inst.id(), "plugin:instrument:Cutoff_Hz");
    assert_eq!(Lane::parse(&inst.id()), Some(inst));
    // The picker offers it (stored subset) and the lane survives session save and load.
    {
        let tr = app.engine.session_mut().track_mut(t).unwrap();
        let mut ins = Insert::new("clap:no.such.mixed");
        ins.params.insert(id.into(), 0.5);
        tr.mixer.inserts[0] = Some(ins);
        tr.lane_mut(&AutoParam::Plugin { slot: 0, param: id.into() }).set_point(0, 0.25);
    }
    assert!(listed(&mut app, t).iter().any(|c| c.2 == "plugin:a:Cutoff_Hz"));
    let text = app.engine.session().to_json().unwrap();
    let back = Session::from_json(&text).unwrap();
    let p = match Lane::parse("plugin:a:Cutoff_Hz").unwrap() {
        Lane::Plugin { slot, param } => AutoParam::Plugin { slot, param },
        other => panic!("{other:?}"),
    };
    assert_eq!(back.track(t).unwrap().lane(&p).unwrap().points[0].value, 0.25);
}

// ---- Issue #65: grouped, searchable picker model ---------------------------------------------

mod grouping {
    use crate::automation_picker::{DeviceModel, PickerState, RECENT_MAX};
    use soundcraft_dsp::{Category, ParamInfo, PluginInfo, Taper, Unit};

    /// A Battery-like table: `cells` cells (A1, A2, ...), `per` parameters each, `Cell/<name>`
    /// groups, CLAP-style names that carry their module.
    fn rows(cells: usize, per: usize) -> Vec<(String, String, String)> {
        let mut v = Vec::new();
        for c in 0..cells {
            let cell = format!("A{}", c + 1);
            for p in 0..per {
                let leaf = if p % 2 == 0 { "Poly Mode On".to_string() } else { format!("Param {p}") };
                // "Poly Mode On" repeats per cell, as in Battery.
                let leaf = if p == 0 { leaf } else { format!("{leaf} {p}") };
                v.push((format!("{}", c * per + p), format!("Cell/{cell}/{leaf}"), format!("Cell/{cell}")));
            }
        }
        v
    }

    fn model(r: &[(String, String, String)]) -> DeviceModel {
        DeviceModel::build(r.iter().map(|(i, n, g)| (i.as_str(), n.as_str(), g.as_str())))
    }

    #[test]
    fn four_thousand_parameters_group_into_nested_cells_and_search_narrows() {
        let r = rows(125, 32);
        assert_eq!(r.len(), 4000);
        let m = model(&r);
        assert!(m.grouped);
        // One "Cell" group holding 125 cell groups of 32 leaves each; nothing at the root.
        assert!(m.root.leaves.is_empty());
        assert_eq!(m.root.children.len(), 1);
        let cell = &m.root.children[0];
        assert_eq!(cell.name, "Cell");
        assert_eq!(cell.children.len(), 125);
        assert!(cell.children.iter().all(|c| c.leaves.len() == 32 && c.children.is_empty()));
        assert_eq!(cell.children[0].name, "A1");
        // The CLAP module prefix is cut from the leaf, and the path reads "Cell > A1 > Poly Mode On".
        assert_eq!(m.entries[0].leaf, "Poly Mode On");
        assert_eq!(m.entries[0].path, "Cell > A1 > Poly Mode On");
        // Search: case-insensitive over the full path, words in any order; it narrows.
        let hits = m.search("POLY a1");
        assert!(hits.iter().all(|&i| m.entries[i].path.to_lowercase().contains("poly") && m.entries[i].path.to_lowercase().contains("a1")));
        assert!(hits.iter().any(|&i| m.entries[i].path == "Cell > A1 > Poly Mode On"));
        assert!(hits.len() < 4000 / 5, "27 of 125 cells hold \"a1\", and only their Poly leaves match");
        let narrow = m.search("cell > a7 > poly mode on");
        assert!(narrow.contains(&m.by_id["192"]) && narrow.len() < hits.len());
        assert!(m.search("no such parameter").is_empty());
        assert!(m.search("   ").is_empty(), "an empty query is no search");
    }

    #[test]
    fn a_table_without_groups_is_a_flat_list() {
        let r: Vec<_> = (0..50).map(|i| (i.to_string(), format!("Knob {i}"), String::new())).collect();
        let m = model(&r);
        assert!(!m.grouped);
        assert!(m.root.children.is_empty());
        assert_eq!(m.root.leaves.len(), 50);
        assert_eq!(m.leaf_label(3), "Knob 3");
        assert_eq!(m.search("knob 4").len(), 14, "Knob 4, 14, 24, 34, 40..49");
    }

    #[test]
    fn a_leaf_name_repeated_inside_a_group_shows_its_path() {
        let m = DeviceModel::build([("1", "Level", "Osc/A"), ("2", "Level", "Osc/A"), ("3", "Level", "Osc/B")]);
        assert_eq!(m.leaf_label(0), "Osc > A > Level (1)");
        assert_eq!(m.leaf_label(1), "Osc > A > Level (2)");
        assert_eq!(m.leaf_label(2), "Level");
        assert_eq!(m.path_label(1), "Osc > A > Level (2)");
        assert_eq!(m.path_label(2), "Osc > B > Level");
    }

    #[test]
    fn recent_keeps_the_last_eight_newest_first_without_repeats() {
        let mut st = PickerState::default();
        for i in 0..12 {
            st.touched("clap:x", &i.to_string());
        }
        st.touched("clap:x", "5");
        let r = &st.recent["clap:x"];
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], "5");
        assert_eq!(r[1], "11");
        assert_eq!(r.iter().filter(|p| *p == "5").count(), 1);
        assert!(!st.recent.contains_key("clap:y"));
    }

    fn leak_info(n: usize) -> &'static PluginInfo {
        let params: Vec<ParamInfo> = (0..n)
            .map(|i| ParamInfo {
                id: Box::leak(i.to_string().into_boxed_str()),
                name: Box::leak(format!("Cell/A{}/Poly Mode On", i % 100).into_boxed_str()),
                min: 0.0,
                max: 1.0,
                default: 0.0,
                unit: Unit::None,
                taper: Taper::Linear,
                choices: &[],
                group: Box::leak(format!("Cell/A{}", i % 100).into_boxed_str()),
            })
            .collect();
        Box::leak(Box::new(PluginInfo {
            id: "clap:synthetic",
            name: "Synthetic",
            short_name: "Synth",
            category: Category::Eq,
            params: Box::leak(params.into_boxed_slice()),
            is_instrument: false,
            audiosuite: false,
        }))
    }

    #[test]
    fn the_model_is_built_once_per_cached_table() {
        let mut st = PickerState::default();
        let info = leak_info(4000);
        let a = st.model("clap:synthetic", info);
        for _ in 0..100 {
            // A frame with the popup open: same table, same model, no rebuild.
            let b = st.model("clap:synthetic", info);
            assert!(std::sync::Arc::ptr_eq(&a, &b));
        }
        assert_eq!(st.builds, 1);
        assert_eq!(a.entries.len(), 4000);
        assert_eq!(a.root.children[0].children.len(), 100);
        // The host replaced the table (a new generation): rebuilt.
        let b = st.model("clap:synthetic", leak_info(10));
        assert_eq!(st.builds, 2);
        assert_eq!(b.entries.len(), 10);
    }
}
