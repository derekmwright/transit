//! Tests for the transport's tempo and meter fields: drag, type, wheel, the meter popup, one undo
//! step per gesture, no document change without a changed value, the grid following the tempo,
//! and what a tempo change does to MIDI clips, audio clips and automation. Driven like a user:
//! synthetic clicks, drags and keys through the control channel's input queue.

use crate::{Services, StudioApp};
use serde_json::{Value, json};
use soundcraft_model::{ClipContent, TrackId};

struct T {
    app: StudioApp,
    ctx: egui::Context,
    time: f64,
    /// X of every vertical bar line the last frame painted in the timeline.
    bar_xs: Vec<f32>,
}

fn mid(r: [f32; 4]) -> (f32, f32) {
    ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0)
}

impl T {
    fn new() -> T {
        let app = StudioApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let mut t = T { app, ctx: egui::Context::default(), time: 0.0, bar_xs: Vec::new() };
        t.frames(3);
        t
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.time += 1.0 / 60.0;
            let mut raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 800.0))),
                time: Some(self.time),
                ..Default::default()
            };
            self.app.raw_input_hook(&mut raw);
            let app = &mut self.app;
            let mut out = self.ctx.run_ui(raw, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            });
            out.textures_delta.clear();
            self.bar_xs.clear();
            let color = crate::theme::TOKENS.grid_bar;
            for s in &out.shapes {
                collect_bars(&s.shape, color, &mut self.bar_xs);
            }
        }
    }

    fn input(&mut self, method: &str, p: Value) {
        let (req, _rx) = crate::ControlRequest::new(method, p);
        let ctx = egui::Context::default();
        let _ = crate::control::handle_for_test(&mut self.app, &ctx, &req);
        self.frames(30);
    }

    fn click(&mut self, at: (f32, f32)) {
        self.input("ui.click", json!({"x": at.0, "y": at.1}));
    }

    fn double(&mut self, at: (f32, f32)) {
        self.input("ui.click", json!({"x": at.0, "y": at.1, "count": 2}));
    }

    fn key(&mut self, key: &str) {
        self.input("ui.key", json!({"key": key}));
    }

    fn typed(&mut self, text: &str) {
        self.input("ui.text", json!({"text": text}));
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        self.input("ui.drag", json!({"x": from.0, "y": from.1, "to_x": to.0, "to_y": to.1}));
    }

    fn rect(&self, name: &str) -> (f32, f32) {
        mid(*self.app.arrange.layout.transport.get(name).unwrap_or_else(|| panic!("no transport rect {name}")))
    }

    fn undo_len(&self) -> usize {
        self.app.engine.undo_history().len()
    }

    fn bpm(&self) -> f64 {
        self.app.engine.session().tempo.tempos()[0].bpm
    }

    fn meter(&self) -> (u32, u32) {
        let m = self.app.engine.session().tempo.meters()[0];
        (m.numerator, m.denominator)
    }

    fn doc(&self) -> (bool, u64) {
        (self.app.engine.is_dirty(), self.app.engine.revision)
    }

    fn track(&mut self, p: Value) -> TrackId {
        let t = TrackId(self.app.run("track.new", p).unwrap()["tracks"][0].as_u64().unwrap());
        self.frames(3);
        t
    }
}

fn collect_bars(s: &egui::Shape, color: egui::Color32, out: &mut Vec<f32>) {
    match s {
        egui::Shape::Vec(v) => v.iter().for_each(|s| collect_bars(s, color, out)),
        egui::Shape::LineSegment { points, stroke } if stroke.color == color && (points[0].x - points[1].x).abs() < 0.01 => out.push(points[0].x),
        _ => {}
    }
}

#[test]
fn dragging_the_tempo_changes_it_in_one_undo_step() {
    let mut t = T::new();
    let before = t.undo_len();
    let at = t.rect("tempo");
    // Up 30 points is 10 BPM (3 points per BPM).
    t.drag(at, (at.0, at.1 - 30.0));
    assert!(t.bpm() > 120.0, "dragging up raises the tempo: {}", t.bpm());
    assert_eq!(t.undo_len(), before + 1, "the whole drag is one undo step");
    t.app.run("edit.undo", json!({})).unwrap();
    assert_eq!(t.bpm(), 120.0);
}

#[test]
fn double_click_types_a_tempo_enter_commits_escape_cancels() {
    let mut t = T::new();
    let before = t.undo_len();
    let at = t.rect("tempo");
    t.double(at);
    t.typed("90");
    t.key("Enter");
    assert_eq!(t.bpm(), 90.0);
    assert_eq!(t.undo_len(), before + 1);
    let doc = t.doc();
    t.double(at);
    t.typed("150");
    t.key("Escape");
    assert_eq!(t.bpm(), 90.0, "Escape cancels");
    assert_eq!(t.undo_len(), before + 1);
    assert_eq!(t.doc(), doc);
    t.double(at);
    t.typed("93.456");
    t.key("Enter");
    assert_eq!(t.bpm(), 93.46, "two decimals");
}

#[test]
fn a_typed_tempo_outside_20_to_999_clamps() {
    let mut t = T::new();
    let at = t.rect("tempo");
    t.double(at);
    t.typed("5000");
    t.key("Enter");
    assert_eq!(t.bpm(), 999.0);
    t.double(at);
    t.typed("3");
    t.key("Enter");
    assert_eq!(t.bpm(), 20.0);
    t.double(at);
    t.typed("fast");
    t.key("Enter");
    assert_eq!(t.bpm(), 20.0, "text that is not a number changes nothing");
}

#[test]
fn the_wheel_nudges_a_bpm_and_one_gesture_is_one_undo_step() {
    let mut t = T::new();
    let before = t.undo_len();
    let r = egui::Rect::from_center_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
    t.app.wheel_tempo(1.0, false, r, 1.0);
    t.app.wheel_tempo(1.0, false, r, 1.1);
    assert_eq!(t.bpm(), 122.0);
    t.app.wheel_tempo(-1.0, true, r, 1.2);
    assert_eq!(t.bpm(), 121.99, "Shift is 0.01");
    assert_eq!(t.undo_len(), before + 1);
}

#[test]
fn opening_the_field_and_committing_the_same_value_leaves_the_document_alone() {
    let mut t = T::new();
    t.frames(3);
    let doc = t.doc();
    let before = t.undo_len();
    let at = t.rect("tempo");
    t.double(at);
    t.key("Enter");
    assert_eq!(t.doc(), doc, "committing the value it already has changes nothing");
    t.double(at);
    t.typed("120");
    t.key("Enter");
    assert_eq!(t.doc(), doc);
    t.drag(at, (at.0, at.1));
    assert_eq!(t.doc(), doc);
    t.click(at);
    assert_eq!(t.doc(), doc, "a click is not an edit");
    let m = t.rect("meter");
    t.click(m);
    t.key("Enter");
    assert_eq!(t.doc(), doc, "committing the meter it already has changes nothing");
    assert_eq!(t.undo_len(), before);
}

#[test]
fn the_meter_popup_sets_three_four_in_one_undo_step_by_typing_or_buttons() {
    let mut t = T::new();
    let before = t.undo_len();
    let m = t.rect("meter");
    t.click(m);
    assert!(t.app.tempo_ui.meter.is_some(), "a click opens the popup");
    t.typed("3/4");
    t.key("Enter");
    assert_eq!(t.meter(), (3, 4));
    assert_eq!(t.undo_len(), before + 1);
    assert!(t.app.tempo_ui.meter.is_none(), "Enter closes it");
    // Buttons: 6/8 is the numerator up three and /8, set once.
    t.click(m);
    let (dec, inc) = (t.rect("meter_dec"), t.rect("meter_inc"));
    t.click(inc);
    t.click(inc);
    t.click(inc);
    t.click(dec);
    t.click(inc);
    t.click(inc);
    let den8 = t.rect("meter_den_8");
    t.click(den8);
    assert_eq!(t.meter(), (3, 4), "the buttons draft; nothing is set yet");
    let set = t.rect("meter_set");
    t.click(set);
    assert_eq!(t.meter(), (7, 8));
    assert_eq!(t.undo_len(), before + 2);
    // Escape cancels; garbage is refused and the popup stays.
    t.click(m);
    t.typed("9/9");
    t.key("Enter");
    assert_eq!(t.meter(), (7, 8));
    assert!(t.app.tempo_ui.meter.is_some() && t.app.status.contains("not a meter"), "{}", t.app.status);
    t.key("Escape");
    assert!(t.app.tempo_ui.meter.is_none());
    assert_eq!(t.undo_len(), before + 2);
}

#[test]
fn the_grid_redraws_at_the_new_tempo() {
    let mut t = T::new();
    t.app.run("view.zoom_to", json!({"samples_per_px": 400.0})).ok();
    t.frames(3);
    let x0 = t.bar_xs.iter().copied().fold(f32::MAX, f32::min);
    let second = |xs: &[f32]| {
        let mut v = xs.to_vec();
        v.sort_by(f32::total_cmp);
        v.dedup_by(|a, b| (*a - *b).abs() < 0.5);
        v[1] - v[0]
    };
    assert!(x0.is_finite() && t.bar_xs.len() > 2, "bar lines: {:?}", t.bar_xs);
    let bar_px = second(&t.bar_xs);
    let at = t.rect("tempo");
    t.double(at);
    t.typed("60");
    t.key("Enter");
    t.frames(3);
    let slow = second(&t.bar_xs);
    assert!((slow / bar_px - 2.0).abs() < 0.05, "half the tempo, bars twice as wide: {bar_px} -> {slow}");
}

#[test]
fn past_a_tempo_change_the_field_edits_that_change_and_says_so() {
    let mut t = T::new();
    t.app.run("event.tempo", json!({"bpm": 100.0, "at": 192_000})).unwrap();
    t.frames(3);
    let g0 = crate::tempo_edit::governing_tempo(&t.app);
    assert_eq!((g0.tick, g0.bpm), (0, 120.0));
    // Put the cursor in bar 4.
    let s = t.app.engine.session();
    let in_bar4 = s.tempo.samples_at_bar_beat(soundcraft_time::BarBeat { bar: 4, beat: 1, tick: 0 }, s.sample_rate);
    t.app.ui.cursor = in_bar4;
    t.frames(3);
    let g = crate::tempo_edit::governing_tempo(&t.app);
    assert_eq!((g.bar, g.bpm), (3, 100.0), "the event that governs bar 4 is the one at bar 3");
    let before = t.undo_len();
    let at = t.rect("tempo");
    t.double(at);
    t.typed("80");
    t.key("Enter");
    let tempos = t.app.engine.session().tempo.tempos().to_vec();
    assert_eq!(tempos.len(), 2);
    assert_eq!(tempos[0].bpm, 120.0, "bar 1 is untouched");
    assert_eq!(tempos[1].bpm, 80.0);
    assert_eq!(t.undo_len(), before + 1);
}

/// What a tempo change does to existing material (recorded, not assumed): clip placements and
/// automation points are sample positions and stay put; MIDI notes are ticks inside their clip,
/// so they follow the new tempo; audio plays at its recorded rate, with no stretch.
#[test]
fn a_tempo_change_leaves_clips_and_automation_in_place_and_midi_notes_follow() {
    let mut t = T::new();
    let inst = t.track(json!({"kind": "instrument"}));
    let clip = t.app.run("midi.new_clip", json!({"tracks": [inst.0], "start": 96_000, "end": 384_000})).unwrap()["clip"].as_u64().unwrap();
    t.app.run("midi.note_add", json!({"clip": clip, "pitch": 60, "start_ticks": 960, "length_ticks": 480})).unwrap();
    let audio = t.track(json!({"kind": "audio"}));
    let dir = std::env::temp_dir().join(format!("transit-tempo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("a.wav");
    std::fs::write(&wav, silent_wav(96_000)).unwrap();
    let plan = soundcraft_engine::io::Recording { start: 96_000, pass: None, punch: None, rate: 48_000, takes: Vec::new() };
    t.app.place_takes(&plan, vec![(audio, wav)], 192_000, 1);
    t.app.run("automation.write_range", json!({"track": inst.0, "param": "volume", "start": 96_000, "end": 192_000, "value": -6.0})).unwrap();
    t.frames(3);
    let snap = |t: &T| {
        let s = t.app.engine.session();
        let spans = |id: TrackId| s.track(id).unwrap().clips().iter().map(|c| (c.start, c.length)).collect::<Vec<_>>();
        let notes = s
            .track(inst)
            .unwrap()
            .clips()
            .iter()
            .find_map(|c| match &c.content {
                ClipContent::Midi { sequence: m } => Some(m.notes.iter().map(|n| (n.start, n.length)).collect::<Vec<_>>()),
                _ => None,
            })
            .unwrap();
        let pts = s.track(inst).unwrap().automation.iter().flat_map(|l| l.points.iter().map(|p| p.at)).collect::<Vec<_>>();
        let note_s = s.tempo.tick_to_samples(960, s.sample_rate);
        (spans(inst), spans(audio), notes, pts, note_s)
    };
    let (m1, a1, n1, p1, ns1) = snap(&t);
    let at = t.rect("tempo");
    t.double(at);
    t.typed("60");
    t.key("Enter");
    let (m2, a2, n2, p2, ns2) = snap(&t);
    assert_eq!(m1, m2, "a MIDI clip keeps its sample placement");
    assert_eq!(a1, a2, "an audio clip keeps its sample placement and length (no time-stretch)");
    assert_eq!(n1, n2, "notes are ticks inside the clip");
    assert_eq!(p1, p2, "automation points are sample positions");
    assert_eq!(ns2, ns1 * 2, "so the same note sounds later in the song at half the tempo");
    let _ = std::fs::remove_dir_all(dir);
}

fn silent_wav(frames: u32) -> Vec<u8> {
    let data = frames * 2;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(48_000u32.to_le_bytes());
    b.extend(96_000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data.to_le_bytes());
    b.extend((0..frames).flat_map(|i| (((i % 200) as i16 - 100) * 100).to_le_bytes()));
    b
}

/// Press on `at`, move through `path` a frame at a time, release.
fn drag_path(t: &mut T, at: (f32, f32), path: &[(f32, f32)]) {
    use egui::{Event, PointerButton, pos2};
    let m = egui::Modifiers::NONE;
    let a = pos2(at.0, at.1);
    t.app.synthetic.push(Event::PointerMoved(a));
    t.app.synthetic.push(Event::PointerButton { pos: a, button: PointerButton::Primary, pressed: true, modifiers: m });
    t.frames(3);
    for p in path {
        t.app.synthetic.push(Event::PointerMoved(pos2(p.0, p.1)));
        t.frames(3);
    }
    let end = path.last().map_or(a, |p| pos2(p.0, p.1));
    t.app.synthetic.push(Event::PointerButton { pos: end, button: PointerButton::Primary, pressed: false, modifiers: m });
    t.frames(5);
}

#[test]
fn a_drag_that_ends_where_it_began_leaves_no_undo_step() {
    let mut t = T::new();
    let doc = t.doc();
    let at = t.rect("tempo");
    drag_path(&mut t, at, &[(at.0, at.1 - 15.0), (at.0, at.1 - 30.0), (at.0, at.1 - 15.0), (at.0, at.1)]);
    assert_eq!(t.bpm(), 120.0);
    assert_eq!(t.undo_len(), 0, "120 -> 130 -> 120 is no edit");
    assert!(!t.doc().0, "and the document is clean: {:?} from {doc:?}", t.doc());
    // A drag that does end elsewhere is still one step.
    drag_path(&mut t, at, &[(at.0, at.1 - 15.0), (at.0, at.1 - 30.0)]);
    assert!(t.bpm() > 120.0);
    assert_eq!(t.undo_len(), 1);
}

#[test]
fn a_field_opened_before_a_tempo_change_edits_the_event_it_opened_on() {
    let mut t = T::new();
    t.app.run("event.tempo", json!({"bpm": 100.0, "at": 192_000})).unwrap();
    t.frames(3);
    let at = t.rect("tempo");
    t.double(at);
    assert!(t.app.tempo_ui.edit.is_some());
    // Playback crosses into bar 4 while the field is open.
    let s = t.app.engine.session();
    t.app.ui.cursor = s.tempo.samples_at_bar_beat(soundcraft_time::BarBeat { bar: 4, beat: 1, tick: 0 }, s.sample_rate);
    t.frames(3);
    assert_eq!(crate::tempo_edit::governing_tempo(&t.app).bpm, 100.0);
    t.typed("90");
    t.key("Enter");
    let tempos = t.app.engine.session().tempo.tempos().to_vec();
    assert_eq!((tempos[0].bpm, tempos[1].bpm), (90.0, 100.0), "the event under the playhead when it opened is the one edited");
}

#[test]
fn a_popup_opened_before_a_meter_change_edits_the_event_it_opened_on() {
    let mut t = T::new();
    t.app.run("event.meter", json!({"numerator": 6, "denominator": 8, "at": 192_000})).unwrap();
    t.frames(3);
    let m = t.rect("meter");
    t.click(m);
    let s = t.app.engine.session();
    t.app.ui.cursor = s.tempo.samples_at_bar_beat(soundcraft_time::BarBeat { bar: 4, beat: 1, tick: 0 }, s.sample_rate);
    t.frames(3);
    t.typed("3/4");
    t.key("Enter");
    let meters = t.app.engine.session().tempo.meters().to_vec();
    assert_eq!(((meters[0].numerator, meters[0].denominator), (meters[1].numerator, meters[1].denominator)), ((3, 4), (6, 8)));
}

#[test]
fn a_mid_song_meter_edit_changes_that_meter_and_later_ones_keep_their_bars() {
    let mut t = T::new();
    // 4/4 from bar 1, 6/8 from bar 3, 5/4 from bar 5.
    t.app.run("event.meter", json!({"numerator": 6, "denominator": 8, "at": 192_000})).unwrap();
    t.app.run("event.meter", json!({"numerator": 5, "denominator": 4, "at": 384_000})).unwrap();
    let bars = |t: &T| {
        let s = t.app.engine.session();
        s.tempo.meters().iter().map(|m| (s.tempo.bar_beat_at_tick(m.tick).bar, m.numerator, m.denominator)).collect::<Vec<_>>()
    };
    assert_eq!(bars(&t), [(1, 4, 4), (3, 6, 8), (5, 5, 4)]);
    // The playhead in bar 3: the popup edits the 6/8 event.
    let s = t.app.engine.session();
    t.app.ui.cursor = s.tempo.samples_at_bar_beat(soundcraft_time::BarBeat { bar: 3, beat: 2, tick: 0 }, s.sample_rate);
    t.frames(3);
    let before = t.undo_len();
    let m = t.rect("meter");
    t.click(m);
    t.typed("3/4");
    t.key("Enter");
    assert_eq!(bars(&t), [(1, 4, 4), (3, 3, 4), (5, 5, 4)], "the edited event stays at bar 3 and the later one at bar 5");
    assert_eq!(t.undo_len(), before + 1);
}
