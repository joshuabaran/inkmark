//! Changing the font families re-shapes text already laid out.

use inkmark_text::{Fonts, TextConfig, TextRenderer};

const CONFIG: TextConfig = TextConfig {
    monospace: false,
    font_size: 14.0,
    line_height: 1.4,
    wrap_width: None,
};

fn width(r: &mut TextRenderer, text: &str) -> f32 {
    let g = r.geometry(text);
    g.caret_x(0, text.len())
}

#[test]
fn a_new_family_reshapes_cached_lines() {
    let ctx = egui::Context::default();
    let fonts = Fonts::shared(&ctx);
    // Proportional families (monospace ones can share a cell width), from
    // what's installed; CI installs DejaVu and Noto.
    let candidates = ["DejaVu Sans", "Noto Sans", "Liberation Sans"];
    let installed: Vec<&str> = candidates
        .into_iter()
        .filter(|f| fonts.borrow().has_family(f))
        .collect();
    if installed.len() < 2 {
        eprintln!("skipped: fewer than two of {candidates:?} installed");
        return;
    }
    let mut r = TextRenderer::with_fonts(fonts.clone());
    let text = "iiiiiiiiii mmmmmmmmmm";
    let mut widths = Vec::new();
    for family in &installed {
        assert!(
            fonts
                .borrow_mut()
                .set_families(None, Some(family))
                .is_empty()
        );
        assert!(
            r.begin_frame(CONFIG, 1.0),
            "a font change invalidates layout"
        );
        widths.push(width(&mut r, text));
        assert!(!r.begin_frame(CONFIG, 1.0), "then it's stable");
    }
    assert!(
        widths.windows(2).any(|w| (w[0] - w[1]).abs() > 0.5),
        "families {installed:?} gave widths {widths:?}"
    );
}

#[test]
fn a_missing_family_is_reported_and_changes_nothing() {
    let ctx = egui::Context::default();
    let fonts = Fonts::shared(&ctx);
    let mut r = TextRenderer::with_fonts(fonts.clone());
    r.begin_frame(CONFIG, 1.0);
    let missing = fonts
        .borrow_mut()
        .set_families(Some("No Such Font 123"), None);
    assert_eq!(missing, vec!["No Such Font 123".to_owned()]);
    assert!(!r.begin_frame(CONFIG, 1.0));
}

#[test]
fn a_name_in_another_case_is_stored_as_the_font_spells_it() {
    // Review of #30: font lookups compare names exactly.
    let ctx = egui::Context::default();
    let fonts = Fonts::shared(&ctx);
    let Some(family) = ["DejaVu Sans", "Noto Sans", "Liberation Sans"]
        .into_iter()
        .find(|f| fonts.borrow().has_family(f))
    else {
        return;
    };
    let typed = family.to_lowercase();
    assert!(
        fonts
            .borrow_mut()
            .set_families(None, Some(&typed))
            .is_empty()
    );
    assert_eq!(fonts.borrow().families().1, family);
}
