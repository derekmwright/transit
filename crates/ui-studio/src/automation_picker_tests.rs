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
