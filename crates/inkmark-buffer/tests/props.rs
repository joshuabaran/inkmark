//! Property tests: the rope document agrees with a plain `String`, and undo/redo
//! always round-trip.

use std::time::{Duration, Instant};

use inkmark_buffer::{Document, Edit, EditKind, Selection};
use proptest::prelude::*;

const ALPHABET: &[&str] = &["a", "b", " ", "\n", "é", "日", "🦀", "*", "#"];

fn text(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(ALPHABET), 0..max).prop_map(|v| v.concat())
}

#[derive(Clone, Debug)]
enum Op {
    /// Replace chars `[a, a+len)` (as fractions of the char count) with text.
    Replace {
        at: f64,
        len: f64,
        insert: String,
        kind: u8,
        gap_ms: u64,
    },
    Undo,
    Redo,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0.0..=1.0, 0.0..0.3, text(6), 0u8..3, 0u64..600).prop_map(|(at, len, insert, kind, gap_ms)| {
            Op::Replace { at, len, insert, kind, gap_ms }
        }),
        2 => Just(Op::Undo),
        1 => Just(Op::Redo),
    ]
}

/// Byte offset of the char at `frac` of the way through `s`.
fn char_offset(s: &str, frac: f64) -> usize {
    let chars = s.chars().count();
    let index = ((chars as f64) * frac).floor() as usize;
    s.char_indices().nth(index).map_or(s.len(), |(i, _)| i)
}

fn whole(doc: &Document) -> String {
    doc.slice(0..doc.len()).into_owned()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn edits_match_a_string_model(initial in text(40), ops in prop::collection::vec(op(), 0..40)) {
        let mut doc = Document::from_text(&initial);
        let mut model = initial.clone();
        for op in ops {
            if let Op::Replace { at, len, insert, .. } = op {
                let start = char_offset(&model, at);
                let end = char_offset(&model, (at + len).min(1.0)).max(start);
                doc.apply(
                    vec![Edit::replace(start..end, insert.clone())],
                    Selection::caret(start),
                    Selection::caret(start + insert.len()),
                    EditKind::Other,
                ).unwrap();
                model.replace_range(start..end, &insert);
            }
            prop_assert_eq!(whole(&doc), model.as_str());
        }
        // Line indexing agrees with splitting on '\n'.
        let lines: Vec<&str> = model.split('\n').collect();
        prop_assert_eq!(doc.line_count(), lines.len());
        for (i, line) in lines.iter().enumerate() {
            prop_assert_eq!(&doc.slice(doc.line_range(i)), line);
        }
    }

    #[test]
    fn undo_all_restores_and_redo_all_replays(initial in text(40), ops in prop::collection::vec(op(), 0..60)) {
        let mut doc = Document::from_text(&initial);
        let mut now = Instant::now();
        for op in ops {
            match op {
                Op::Replace { at, len, insert, kind, gap_ms } => {
                    let current = whole(&doc);
                    let start = char_offset(&current, at);
                    let end = char_offset(&current, (at + len).min(1.0)).max(start);
                    let kind = [EditKind::Typing, EditKind::Deleting, EditKind::Other][kind as usize];
                    now += Duration::from_millis(gap_ms);
                    doc.apply_at(
                        vec![Edit::replace(start..end, insert.clone())],
                        Selection::caret(start),
                        Selection::caret(start + insert.len()),
                        kind,
                        now,
                    ).unwrap();
                }
                Op::Undo => { doc.undo(); }
                Op::Redo => { doc.redo(); }
            }
        }
        let latest = whole(&doc);
        let mut steps = 0;
        while doc.undo().is_some() {
            steps += 1;
        }
        prop_assert_eq!(whole(&doc), initial.as_str());
        prop_assert!(!doc.is_dirty());
        // Steps undone before `latest` sit below these on the redo stack, so
        // exactly `steps` redos land back on `latest`.
        for _ in 0..steps {
            prop_assert!(doc.redo().is_some());
        }
        prop_assert_eq!(whole(&doc), latest);
    }
}
