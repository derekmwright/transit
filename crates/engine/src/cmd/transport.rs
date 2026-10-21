//! Transport commands. They update the session's stopped playhead and queue requests for the
//! host's audio engine (which may be absent when headless).

use super::*;
use crate::{TransportRequest, cmd};
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(noundo "transport.play", "Play", [], Some("Space"), "{from?: position}", always, |e, p| {
            if let Some(at) = position_param(e, "transport.play", p, "from")? {
                e.transport_requests.push(TransportRequest::Locate(at.max(0)));
            }
            e.transport_requests.push(TransportRequest::Play);
            Ok(json!({}))
        }),
        cmd!(noundo "transport.stop", "Stop", [], Some("Space"), "{}", always, |e, _| { e.transport_requests.push(TransportRequest::Stop); Ok(json!({})) }),
        cmd!(noundo "transport.toggle", "Play/Stop", [], Some("Space"), "{}", always, |e, _| { e.transport_requests.push(TransportRequest::TogglePlay); Ok(json!({})) }),
        cmd!(noundo "transport.record", "Record", [], Some("Cmd+Space"), "{}", always, |e, _| { e.transport_requests.push(TransportRequest::Record); Ok(json!({})) }),
        cmd!(noundo "transport.pause", "Pause", [], None, "{}", always, |e, _| { e.transport_requests.push(TransportRequest::Pause); Ok(json!({})) }),
        cmd!(noundo "transport.half_speed", "Half-Speed Playback", [], Some("Shift+Space"), "{}", always, |e, _| { e.transport_requests.push(TransportRequest::HalfSpeed); Ok(json!({})) }),
        cmd!(noundo "transport.play_selection", "Play Edit", ["Edit", "Selection"], Some("Alt+["), "{}", always, |e, _| { e.transport_requests.push(TransportRequest::PlaySelection); Ok(json!({})) }),
        cmd!(noundo "transport.rtz", "Return to Zero", [], Some("Home"), "{}", always, |e, _| locate(e, 0)),
        cmd!(noundo "transport.go_to_end", "Go to End", [], Some("End"), "{}", always, |e, _| { let end = e.session().content_end(); locate(e, end) }),
        cmd!(noundo "transport.rewind", "Rewind", [], None, "{seconds?: 1}", always, |e, p| {
            let s = e.session();
            let d = s.sample_rate.samples(f64_or(p, "seconds", 1.0).clamp(0.0, 86_400.0));
            let at = (cur(e) - d).max(0);
            locate(e, at)
        }),
        cmd!(noundo "transport.fast_forward", "Fast Forward", [], None, "{seconds?: 1}", always, |e, p| {
            let s = e.session();
            let d = s.sample_rate.samples(f64_or(p, "seconds", 1.0).clamp(0.0, 86_400.0));
            let at = cur(e) + d;
            locate(e, at)
        }),
        cmd!(noundo "transport.locate", "Locate", [], None, "{at: position}", always, |e, p| {
            let at = position_param(e, "transport.locate", p, "at")?.ok_or_else(|| bad("transport.locate", "`at` required"))?;
            locate(e, at.max(0))
        }),
        cmd!(
            "transport.loop",
            "Set Loop Region",
            [],
            None,
            "{start?: position, end?: position (both: the region, an empty or reversed pair is ordered, an empty one clears it), enabled?: bool}",
            always,
            |e, p| {
                let st = position_param(e, "transport.loop", p, "start")?;
                let en = position_param(e, "transport.loop", p, "end")?;
                if st.is_some() != en.is_some() {
                    return Err(bad("transport.loop", "`start` and `end` go together"));
                }
                let enabled = p.get("enabled").and_then(Value::as_bool);
                let edit = &e.session().edit;
                let range = st
                    .zip(en)
                    .map(|(a, b)| soundcraft_time::Range::new(a.max(0), b.max(0)))
                    .map(|r| if r.is_empty() { soundcraft_time::Range::default() } else { r });
                // Touch the document only when something changes, so a no-op leaves no undo step.
                if range.is_some_and(|r| r != edit.loop_range) || enabled.is_some_and(|v| v != edit.loop_playback) {
                    let ed = &mut e.session_mut().edit;
                    if let Some(r) = range {
                        ed.loop_range = r;
                    }
                    if let Some(v) = enabled {
                        ed.loop_playback = v;
                    }
                }
                let ed = &e.session().edit;
                Ok(json!({"start": ed.loop_range.start, "end": ed.loop_range.end, "enabled": ed.loop_playback}))
            }
        ),
        cmd!(noundo "transport.status", "Transport Status", [], None, "{}", always, |e, _| Ok(json!(e.transport))),
    ]
}

fn cur(e: &Engine) -> i64 {
    if e.transport.playing { e.transport.position } else { e.session().edit.selection.start }
}

fn locate(e: &mut Engine, at: i64) -> Result<Value> {
    let s = e.session_mut();
    s.edit.playhead = at;
    if s.edit.selection.is_empty() || !s.edit.selection.contains(at) {
        s.edit.selection = soundcraft_time::Range::point(at);
    }
    e.transport_requests.push(TransportRequest::Locate(at));
    Ok(json!({"at": at}))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    fn undo_len(e: &mut Engine) -> usize {
        e.execute("engine.history", &json!({})).unwrap()["undo"].as_array().unwrap().len()
    }

    #[test]
    fn transport_loop_sets_the_region_in_one_undo_step() {
        let mut e = Engine::default();
        let steps = undo_len(&mut e);
        let v = e.execute("transport.loop", &json!({"start": 4800, "end": 1200, "enabled": true})).unwrap();
        assert_eq!((v["start"].as_i64(), v["end"].as_i64(), v["enabled"].as_bool()), (Some(1200), Some(4800), Some(true)), "ordered");
        assert_eq!(e.session().edit.loop_range, soundcraft_time::Range::new(1200, 4800));
        assert!(e.session().edit.loop_playback);
        assert_eq!(undo_len(&mut e), steps + 1);
        // Setting it again is no change, so no undo step.
        e.execute("transport.loop", &json!({"start": 1200, "end": 4800})).unwrap();
        assert_eq!(undo_len(&mut e), steps + 1);
        // Enabled alone leaves the region.
        e.execute("transport.loop", &json!({"enabled": false})).unwrap();
        assert_eq!(e.session().edit.loop_range, soundcraft_time::Range::new(1200, 4800));
        assert!(!e.session().edit.loop_playback);
        // An empty pair clears; a lone bound is refused; negatives clamp to 0.
        e.execute("transport.loop", &json!({"start": 7, "end": 7})).unwrap();
        assert!(e.session().edit.loop_range.is_empty());
        assert!(e.execute("transport.loop", &json!({"start": 7})).is_err());
        e.execute("transport.loop", &json!({"start": -50, "end": 100})).unwrap();
        assert_eq!(e.session().edit.loop_range, soundcraft_time::Range::new(0, 100));
        // Undo restores the previous region.
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.session().edit.loop_range.is_empty());
    }
}
