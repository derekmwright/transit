//! Stop gestures through the real UI and host queue. No player or audio device is opened.

use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::TrackId;

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
}

impl T {
    fn new() -> Self {
        let mut t =
            Self { app: StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default()), ctx: egui::Context::default(), time: 0.0 };
        t.app.set_cursor(48_000, None);
        t.frames(3);
        t
    }

    fn frames(&mut self, count: usize) {
        for _ in 0..count {
            self.frame(Vec::new());
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        self.time += 1.0 / 60.0;
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        self.app.raw_input_hook(&mut raw);
        let mut output = self.ctx.run_ui(raw, |ui| {
            self.app.logic(ui.ctx());
            self.app.ui(ui);
        });
        output.textures_delta.clear();
    }

    fn input(&mut self, method: &str, params: Value) {
        let (request, _rx) = crate::ControlRequest::new(method, params);
        let reply = crate::control::handle_for_test(&mut self.app, &self.ctx, &request).unwrap();
        assert_eq!(reply["ok"], true, "{reply}");
        self.frames(8);
    }

    fn stop(&mut self, count: u32) {
        let r = self.app.arrange.layout.transport["stop"];
        self.input("ui.click", json!({"x": (r[0] + r[2]) / 2.0, "y": (r[1] + r[3]) / 2.0, "count": count}));
    }

    fn play(&mut self) {
        self.app.run("transport.play", json!({})).unwrap();
        self.frames(3);
        assert!(self.app.is_playing());
        assert!(self.app.position() > 48_000);
    }

    fn assert_stopped_at(&self, at: i64) {
        assert!(!self.app.is_playing());
        assert!(!self.app.engine.transport.recording);
        assert_eq!(self.app.cursor(), at);
        assert_eq!(self.app.position(), at);
        assert_eq!(self.app.engine.transport.position, at);
    }
}

#[test]
fn double_click_stop_while_playing_returns_to_zero_without_undo() {
    let mut t = T::new();
    t.app.run("track.new", json!({})).unwrap();
    t.app.run("edit.undo", json!({})).unwrap();
    let history = t.app.engine.undo_history();
    assert!(t.app.engine.can_redo());
    t.play();
    t.stop(2);
    t.assert_stopped_at(0);
    assert_eq!(t.app.engine.session().edit.playhead, 0);
    assert_eq!(t.app.engine.undo_history(), history, "transport preserves undo history");
    assert!(t.app.engine.can_redo(), "transport preserves redo history");
    t.app.run("edit.redo", json!({})).unwrap();
    assert_eq!(t.app.engine.session().tracks.len(), 1);
}

#[test]
fn single_stop_while_stopped_returns_to_zero() {
    let mut t = T::new();
    t.stop(1);
    t.assert_stopped_at(0);
    assert!(t.app.engine.undo_history().is_empty());
}

#[test]
fn double_click_stop_in_one_frame_stops_before_returning_to_zero() {
    let mut t = T::new();
    t.play();
    let r = t.app.arrange.layout.transport["stop"];
    let pos = egui::pos2((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
    t.frame(vec![egui::Event::PointerMoved(pos)]);
    let clicks = [true, false, true, false]
        .into_iter()
        .map(|pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE })
        .collect();
    t.frame(clicks);
    t.frames(2);
    t.assert_stopped_at(0);
}

#[test]
fn first_stop_keeps_play_start_cursor_second_stop_returns_to_zero() {
    let mut t = T::new();
    t.play();
    t.stop(1);
    t.assert_stopped_at(48_000);
    // A second Stop is independent of the double-click timeout.
    t.frames(60);
    t.stop(1);
    t.assert_stopped_at(0);
}

#[test]
fn space_still_stops_at_the_cursor_and_restarts_there() {
    let mut t = T::new();
    t.play();
    t.input("ui.key", json!({"key": "Space"}));
    t.assert_stopped_at(48_000);
    t.input("ui.key", json!({"key": "Space"}));
    assert!(t.app.is_playing());
    assert_eq!(t.app.cursor(), 48_000);
    assert!(t.app.position() > 48_000);
}

#[test]
fn queued_stop_stop_play_locates_before_restarting_and_pause_does_not_rewind() {
    let mut t = T::new();
    t.play();
    for command in ["transport.stop", "transport.stop", "transport.play"] {
        t.app.run(command, json!({})).unwrap();
    }
    t.app.handle_transport(0.0);
    assert!(t.app.is_playing());
    assert_eq!(t.app.position(), 0);
    assert!(t.app.engine.transport_requests.is_empty());

    t.app.run("transport.locate", json!({"at": 96_000})).unwrap();
    for _ in 0..2 {
        t.app.run("transport.pause", json!({})).unwrap();
    }
    t.app.handle_transport(0.0);
    t.assert_stopped_at(96_000);
}

struct TakeFile(std::path::PathBuf);

impl TakeFile {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("transit-stop-{tag}-{}.wav", std::process::id()));
        // One second of synthetic mono PCM, just as the recording writer supplies to the host.
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(96_036u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48_000u32.to_le_bytes());
        wav.extend(96_000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(96_000u32.to_le_bytes());
        for _ in 0..48_000 {
            wav.extend(1000i16.to_le_bytes());
        }
        std::fs::write(&path, wav).unwrap();
        Self(path)
    }
}

impl Drop for TakeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Stop a one-second audio take, the pass having started approximately or on a validated clock.
fn stop_take(tag: &str, approximate: bool) -> T {
    let take = TakeFile::new(tag);
    let mut t = T::new();
    let track = TrackId(t.app.run("track.new", json!({"kind": "audio", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
    t.app.record_start = 48_000;
    t.app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 48_000,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    t.app.finished_test_takes = Some((vec![(track, take.0.clone())], None));
    t.app.test_approximate_start = approximate;
    t.app.engine.transport.recording = true;
    t.app.start_play(96_000, None, None);
    t.stop(1);
    assert_eq!(t.app.engine.session().track(track).unwrap().clips().len(), 1);
    t
}

#[test]
fn a_take_that_started_without_a_validated_clock_says_so_once_at_stop() {
    let mut t = stop_take("approximate", true);
    assert!(t.app.status.contains("Recorded with approximate alignment: the audio device clock did not validate"), "{}", t.app.status);
    assert!(t.app.status.starts_with("Recorded 1 clip"), "{}", t.app.status);
    assert_eq!(t.app.telemetry.approximate_takes, 1);
    t.frames(60);
    assert_eq!(t.app.telemetry.approximate_takes, 1, "counted at Stop only");
}

#[test]
fn a_take_on_a_validated_clock_is_not_flagged() {
    let t = stop_take("validated", false);
    assert!(!t.app.status.contains("approximate"), "{}", t.app.status);
    assert_eq!(t.app.telemetry.approximate_takes, 0);
}

fn recording_stop(count: u32) {
    let take = TakeFile::new(if count == 1 { "single" } else { "double" });
    let mut t = T::new();
    let track = TrackId(t.app.run("track.new", json!({"kind": "audio", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
    t.app.record_start = 48_000;
    t.app.take_plan = Some(soundcraft_engine::io::Recording {
        start: 48_000,
        pass: None,
        punch: None,
        rate: 48_000,
        takes: Vec::new(),
        midi_journal: None,
        midi_placed: false,
    });
    t.app.finished_test_takes = Some((vec![(track, take.0.clone())], None));
    t.app.engine.transport.recording = true;
    t.app.start_play(96_000, None, None);
    let before = t.app.engine.undo_history();
    t.stop(count);
    t.assert_stopped_at(if count == 1 { 48_000 } else { 0 });
    assert!(t.app.finished_test_takes.is_none(), "Stop must finish the take");
    assert!(t.app.take_plan.is_none());
    let clips = t.app.engine.session().track(track).unwrap().clips();
    assert_eq!(clips.len(), 1);
    assert_eq!((clips[0].start, clips[0].length), (48_000, 48_000), "the take stays at its recorded position and full length");
    let clip = clips[0].clone();
    let source = &t.app.engine.session().sources[0];
    assert_eq!(source.frames, 48_000);
    assert!(take.0.is_file());
    let after = t.app.engine.undo_history();
    assert_eq!(after.len(), before.len() + 1, "only the take adds an undo step");
    assert_eq!(after.last().map(String::as_str), Some("Record"));
    t.frames(60);
    t.stop(1);
    t.assert_stopped_at(0);
    assert_eq!(t.app.engine.undo_history(), after);
    assert_eq!(t.app.engine.session().track(track).unwrap().clips()[0].id, clip.id);
    t.app.run("edit.undo", json!({})).unwrap();
    assert!(t.app.engine.session().track(track).unwrap().clips().is_empty(), "Undo reaches the take, not the return to zero");
    t.app.run("edit.redo", json!({})).unwrap();
    assert_eq!(t.app.engine.session().track(track).unwrap().clips()[0].id, clip.id);
}

#[test]
fn double_click_stop_keeps_recorded_take_then_returns_to_zero() {
    recording_stop(2);
}

#[test]
fn first_stop_keeps_recorded_take_at_play_start_then_second_returns_to_zero() {
    recording_stop(1);
}

fn click_play(t: &mut T) {
    let r = t.app.arrange.layout.transport["play"];
    t.input("ui.click", json!({"x": (r[0] + r[2]) / 2.0, "y": (r[1] + r[3]) / 2.0, "count": 1}));
}

#[test]
fn play_is_muted_with_a_reason_while_play_is_pending() {
    let mut t = T::new();
    assert_eq!(t.app.transport_wait(), None);
    t.app.player_requests.play = Some(Default::default());
    t.frames(2);
    assert!(t.app.play_pending());
    assert_eq!(t.app.transport_wait(), Some("Play pending"));
    // The button is still laid out (the tooltip and click target), now in the muted state.
    assert!(t.app.arrange.layout.transport.contains_key("play"));
}

#[test]
fn clicking_play_while_pending_issues_no_second_play() {
    let mut t = T::new();
    t.app.player_requests.play = Some(Default::default());
    t.frames(2);
    click_play(&mut t);
    assert_eq!(t.app.player_requests.play, Some(Default::default()), "the pending request is untouched");
    assert!(!t.app.is_playing(), "no play started");
    assert!(t.app.engine.transport_requests.is_empty(), "no request queued");
}

#[test]
fn play_returns_to_normal_once_the_request_clears() {
    let mut t = T::new();
    t.app.player_requests.play = Some(Default::default());
    t.frames(2);
    assert!(t.app.transport_wait().is_some());
    t.app.player_requests.play = None;
    t.frames(2);
    assert_eq!(t.app.transport_wait(), None);
    click_play(&mut t);
    assert!(t.app.is_playing(), "Play works again");
}
