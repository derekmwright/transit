use egui::accesskit::Role;
use egui::{Event, Modifiers, PointerButton, Pos2, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, menus};

fn press(h: &mut Harness<'_, SoundApp>, pos: Pos2, pressed: bool) {
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
    h.run_steps(2);
}

/// #399: in Track > Edit Group..., dragging down the Tracks list adds every track it passes, and OK
/// groups them all.
#[test]
fn dragging_down_the_group_dialogs_track_list_adds_each_track() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let tracks: Vec<(u64, String)> = app.engine.session().tracks.iter().map(|t| (t.id.0, t.name.clone())).collect();
    assert!(tracks.len() >= 4);
    app.run("edit.select", json!({"tracks": [tracks[0].0], "exact": true})).unwrap();
    // The classic menu leaf maps to the command by its new label.
    assert_eq!(soundcraft_engine::catalog::implemented_by("Track > Edit Group...", &[]).as_deref(), Some("track.group"));
    menus::invoke_menu(&mut app, "track.group", "Track > Edit Group...");
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    let at: Vec<Pos2> = tracks[1..4].iter().map(|(_, n)| h.get_by_role_and_label(Role::CheckBox, n).rect().center()).collect();
    h.event(Event::PointerMoved(at[0]));
    h.run_steps(1);
    press(&mut h, at[0], true);
    for p in &at[1..] {
        h.event(Event::PointerMoved(*p));
        h.run_steps(2);
    }
    press(&mut h, at[2], false);
    h.get_by_role_and_label(Role::Button, "OK").click();
    h.run_steps(2);
    let session = h.state().engine.session();
    let members: Vec<u64> = session.groups.last().map(|g| g.members.iter().map(|m| m.0).collect()).unwrap_or_default();
    assert_eq!(members, tracks[..4].iter().map(|t| t.0).collect::<Vec<_>>());
}
