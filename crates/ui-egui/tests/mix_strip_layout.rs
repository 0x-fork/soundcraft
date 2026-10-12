use egui::{Color32, FullOutput, Shape, epaint::TextShape, vec2};
use egui_kittest::Harness;
use serde_json::json;
use soundcraft_ui_egui::{
    Services, SoundApp,
    theme::{ThemeMode, Tokens},
};

const SCALE: [&str; 10] = ["12", "6", "0", "5", "10", "15", "20", "30", "40", "60"];

// The palette is process-wide, so tests in this file take turns.
static PALETTE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn mix(mode: ThemeMode) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.engine.session_mut().edit.selected_tracks.clear();
    // Fader caps at the bottom, clear of the scale.
    for track in &mut app.engine.session_mut().tracks {
        track.mixer.volume_db = -144.0;
    }
    app.run("window.mix", json!({})).unwrap();
    app.run("ui.theme", json!({"mode": mode.id()})).unwrap();
    let mut harness = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    for _ in 0..4 {
        harness.step();
    }
    harness
}

fn texts<'a>(output: &'a FullOutput, keep: impl Fn(&TextShape) -> bool + 'a) -> impl Iterator<Item = &'a TextShape> + 'a {
    output.shapes.iter().filter_map(move |shape| match &shape.shape {
        Shape::Text(text) if keep(text) => Some(text),
        _ => None,
    })
}

fn contrast(a: Color32, b: Color32) -> f64 {
    let luminance = |color: Color32| {
        let linear = |channel: u8| {
            let s = f64::from(channel) / 255.0;
            if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[test]
fn fader_scale_numbers_are_readable_and_clear_of_their_ticks() {
    let _palette = PALETTE.lock().unwrap_or_else(|e| e.into_inner());
    for (mode, palette) in [(ThemeMode::Dark, Tokens::DARK), (ThemeMode::Light, Tokens::LIGHT)] {
        let harness = mix(mode);
        let output = harness.output();
        let numbers: Vec<_> =
            texts(output, |t| t.galley.job.sections[0].format.font_id.size == 8.0 && SCALE.contains(&t.galley.job.text.as_str())).collect();
        assert!(numbers.len() >= SCALE.len() * 2, "{mode:?}: found {} scale numbers", numbers.len());
        for number in numbers {
            let color = number.override_text_color.unwrap_or(number.galley.job.sections[0].format.color);
            let ratio = contrast(color, palette.strip_bg);
            assert!(ratio >= 4.5, "{mode:?}: scale number {:?} contrast {ratio:.2}:1", number.galley.job.text);
            let glyphs = number.visual_bounding_rect();
            for shape in &output.shapes {
                if let Shape::LineSegment { points: [a, b], .. } = &shape.shape {
                    let crosses = a.y == b.y && glyphs.y_range().contains(a.y) && a.x.min(b.x) < glyphs.max.x && a.x.max(b.x) > glyphs.min.x;
                    assert!(!crosses, "{mode:?}: a tick runs through {:?}", number.galley.job.text);
                }
            }
        }
    }
}
