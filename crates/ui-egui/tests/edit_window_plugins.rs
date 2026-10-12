use egui::{Event, Modifiers, PointerButton, Pos2, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::{Track, TrackId};
use soundcraft_ui_egui::edit_window::HEADER_W;
use soundcraft_ui_egui::{Services, SoundApp};

const CMD: Modifiers = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn press(h: &mut Harness<'_, SoundApp>, pos: Pos2, button: PointerButton, modifiers: Modifiers) {
    h.state_mut().synthetic.extend([
        Event::ModifiersChanged(modifiers),
        Event::PointerMoved(pos),
        Event::PointerButton { pos, button, pressed: true, modifiers },
        Event::PointerButton { pos, button, pressed: false, modifiers },
        Event::ModifiersChanged(Modifiers::NONE),
    ]);
    for _ in 0..10 {
        step(h);
    }
}

fn click(h: &mut Harness<'_, SoundApp>, pos: Pos2) {
    press(h, pos, PointerButton::Primary, Modifiers::NONE);
}

fn kick(h: &Harness<'_, SoundApp>) -> Track {
    h.state().engine.session().track_by_name("Kick").cloned().unwrap()
}

fn insert(h: &Harness<'_, SoundApp>, slot: usize) -> Option<String> {
    kick(h).mixer.inserts.get(slot).cloned().flatten().map(|i| i.plugin)
}

fn bypassed(h: &Harness<'_, SoundApp>, slot: usize) -> bool {
    kick(h).mixer.inserts.get(slot).cloned().flatten().is_some_and(|i| i.bypass)
}

fn harness(view: &str) -> (Harness<'static, SoundApp>, TrackId, [f32; 4]) {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.run(view, json!({})).unwrap();
    app.run("track.height", json!({"tracks": ["Kick"], "height": "medium"})).unwrap();
    let kick = app.engine.session().track_by_name("Kick").unwrap().id;
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    for _ in 0..6 {
        step(&mut h);
    }
    let row = h.state().edit_layout.rows.iter().find(|(id, _)| *id == kick.0).map(|(_, r)| *r).unwrap();
    (h, kick, row)
}

#[test]
fn edit_window_insert_slots_open_change_and_add_plugins() {
    let (mut h, kick, row) = harness("view.edit_inserts_ae");
    let slot = |i: usize| pos2(row[0] + HEADER_W + 56.0, row[1] + 9.5 + i as f32 * 15.0);

    click(&mut h, slot(0));
    assert!(h.state().ui.plugin_windows.contains(&(kick, 0)));
    h.state_mut().ui.plugin_windows.clear();

    press(&mut h, slot(0), PointerButton::Primary, CMD);
    assert!(bypassed(&h, 0));
    assert!(h.state().ui.plugin_windows.is_empty());

    press(&mut h, slot(1), PointerButton::Secondary, Modifiers::NONE);
    let remove = h.get_by_label("no insert").rect().center();
    click(&mut h, remove);
    assert_eq!(insert(&h, 1), None);

    let plugin = soundcraft_dsp::plugins().iter().find(|p| !p.is_instrument).unwrap();
    click(&mut h, slot(2));
    let category = h.get_by_label(&format!("{:?} ⏵", plugin.category)).rect().center();
    h.state_mut().synthetic.push(Event::PointerMoved(category));
    for _ in 0..10 {
        step(&mut h);
    }
    let pick = h.get_by_label(plugin.name).rect().center();
    click(&mut h, pick);
    assert_eq!(insert(&h, 2).as_deref(), Some(plugin.id));
}

#[test]
fn the_views_menu_stays_open_until_a_click_elsewhere() {
    let (mut h, _, _) = harness("view.edit_inserts_ae");
    let views = h.get_by_label("Views").rect().center();
    click(&mut h, views);
    for column in ["Sends A-E", "I/O"] {
        let item = h.get_by_label(column).rect().center();
        click(&mut h, item);
    }
    let edit = &h.state().engine.session().edit;
    assert!(edit.flag("edit_view.sends_ae") && edit.flag("edit_view.io"));
    click(&mut h, pos2(800.0, 990.0));
    assert!(h.query_by_label("Sends A-E").is_none());
}
