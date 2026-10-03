//! End of a wrapped row on real shaped text: where `hit_row` past the end
//! lands, and whether the caret there stays on that row.

use inkmark_text::{LineGeometry, TextConfig, TextRenderer};

fn shaped(text: &str, wrap: f32) -> LineGeometry {
    let ctx = egui::Context::default();
    let mut r = TextRenderer::new(&ctx);
    r.begin_frame(
        TextConfig {
            monospace: true,
            font_size: 14.0,
            line_height: 1.4,
            wrap_width: Some(wrap),
        },
        1.0,
    );
    r.geometry(text)
}

/// The text of row `row`, and where End puts the caret on it.
fn end_of_row(text: &str, g: &LineGeometry, row: usize) -> (String, String, bool) {
    let r = &g.rows[row];
    let (at, upstream) = g.hit_row_affine(row, f32::INFINITY);
    (
        text[r.start..r.end].to_owned(),
        text[r.start..at].to_owned(),
        upstream,
    )
}

#[test]
fn a_mid_word_wrap_ends_after_the_rows_last_character() {
    // Review of #28: End used to stop before the last character of a row
    // that wrapped inside a word, a CJK run, or after a hyphen.
    for text in [
        "abcdefghijklmnopqrstuvwxyz",
        "你好世界这是一段没有空格的文字",
        "one-two-three-four-five-six",
    ] {
        let g = shaped(text, 50.0);
        assert!(g.rows.len() > 1, "{text:?} wraps");
        let (row, before_caret, upstream) = end_of_row(text, &g, 0);
        assert_eq!(before_caret, row, "{text:?}: End keeps the whole row");
        assert!(upstream, "{text:?}: the caret stays on the first row");
        let at = g.rows[0].end;
        assert_eq!(g.row_of_affine(at, true), 0);
        assert_eq!(g.row_of_affine(at, false), 1);
    }
}

#[test]
fn a_space_wrap_ends_before_the_space() {
    // The space kept before an overlong word (a cluster of the first row),
    // and an ordinary word wrap (the shaper leaves the space out of both).
    for text in [
        "hi supercalifragilisticexpialidocious",
        "word word word word",
    ] {
        let g = shaped(text, 60.0);
        assert!(g.rows.len() > 1, "{text:?} wraps");
        // The space is either the row's last cluster or in neither row;
        // either way End stops after the word, on this row.
        let (row, before_caret, upstream) = end_of_row(text, &g, 0);
        assert_eq!(before_caret, row.trim_end(), "{text:?}");
        assert!(!upstream);
    }
}
