//! Monospace text on its cell grid: wide characters take two cells, so
//! columns of mixed CJK and Latin text line up in the code pane.

use inkmark_text::{LineGeometry, TextConfig, TextRenderer};

fn renderer(wrap: Option<f32>) -> TextRenderer {
    let ctx = egui::Context::default();
    let mut r = TextRenderer::new(&ctx);
    r.begin_frame(
        TextConfig {
            monospace: true,
            font_size: 14.0,
            line_height: 21.0,
            wrap_width: wrap,
        },
        1.0,
    );
    r
}

/// Each cluster's (text, x, w), in cells.
fn cells(g: &LineGeometry, text: &str, cell: f32) -> Vec<(String, f32, f32)> {
    g.rows
        .iter()
        .flat_map(|r| r.clusters.iter())
        .map(|c| (text[c.start..c.end].to_owned(), c.x / cell, c.w / cell))
        .collect()
}

#[test]
fn wide_characters_take_two_cells() {
    let mut r = renderer(None);
    let cell = r.geometry("a").rows[0].clusters[0].w;
    let text = "ab中文cd";
    let got = cells(&r.geometry(text), text, cell);
    let want: Vec<(String, f32, f32)> = [
        ("a", 0, 1),
        ("b", 1, 1),
        ("中", 2, 2),
        ("文", 4, 2),
        ("c", 6, 1),
        ("d", 7, 1),
    ]
    .into_iter()
    .map(|(s, x, w)| (s.to_owned(), x as f32, w as f32))
    .collect();
    assert_eq!(got, want);
    // Box drawing stays one cell each, tabs run to the next stop of 4.
    let text = "┌─┐\tx";
    let got = cells(&r.geometry(text), text, cell);
    assert_eq!(got.last().unwrap(), &("x".to_owned(), 4.0, 1.0), "{got:?}");
    assert_eq!(got[1], ("─".to_owned(), 1.0, 1.0));
}

#[test]
fn a_wrapped_line_of_wide_characters_stays_inside_its_width() {
    let wrap = 200.0;
    let mut r = renderer(Some(wrap));
    let text = "中文字符".repeat(20);
    let g = r.geometry(&text);
    assert!(g.rows.len() > 1, "it wraps");
    for row in &g.rows {
        let right = row.clusters.last().map_or(0.0, |c| c.x + c.w);
        assert!(right <= wrap + 0.5, "a row reaches {right} past {wrap}");
    }
}
