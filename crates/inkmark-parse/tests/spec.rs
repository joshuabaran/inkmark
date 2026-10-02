//! Every CommonMark spec example: the SourceMap covers every byte exactly
//! once, and its visible spans reproduce exactly the text pulldown-cmark
//! renders, in order. This is what lets the live view map a caret in
//! rendered text back to a source byte.

use std::collections::BTreeMap;

use inkmark_parse::{BlockKind, MarkdownParser, PulldownParser, SpanKind, Syntax};
use pulldown_cmark::{Event, Parser};

fn examples() -> Vec<(u64, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/commonmark/spec.json"
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    json.as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["example"].as_u64().unwrap(),
                e["markdown"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The text pulldown-cmark renders, in event order.
fn rendered_text(src: &str) -> String {
    Parser::new(src)
        .filter_map(|e| match e {
            Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                Some(t.into_string())
            }
            _ => None,
        })
        .collect()
}

/// The same text, reconstructed from the map's visible spans.
fn mapped_text(src: &str, map: &inkmark_parse::SourceMap) -> String {
    map.iter()
        .filter_map(|s| match s.kind {
            SpanKind::Text => Some(src[s.range].to_owned()),
            SpanKind::Replaced(r) => Some(r.into_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn spec_examples_map_every_byte() {
    let mut failures = Vec::new();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut other = Vec::new();
    let examples = examples();
    for (n, src) in &examples {
        let out = PulldownParser.parse(src);
        if let Err(e) = out.map.validate(src.len()) {
            failures.push(format!("example {n}: coverage: {e}"));
            continue;
        }
        let (want, got) = (rendered_text(src), mapped_text(src, &out.map));
        if want != got {
            failures.push(format!(
                "example {n}: text\n  want {want:?}\n  got  {got:?}"
            ));
        }
        for s in out.map.iter() {
            let key = match &s.kind {
                SpanKind::Syntax(k) => format!("{k:?}"),
                SpanKind::Replaced(_) => "Replaced".into(),
                k => format!("{k:?}"),
            };
            *kinds.entry(key).or_default() += 1;
            if s.kind == SpanKind::Syntax(Syntax::Escape) {
                assert_eq!(&src[s.range.clone()], "\\", "example {n}");
            }
            if s.kind == SpanKind::Syntax(Syntax::Other) {
                other.push(*n);
            }
        }
        for b in out.blocks.iter() {
            assert!(
                b.range.end <= src.len(),
                "example {n}: block {b:?} past end"
            );
        }
        // The live view renders leaf blocks, so all visible text must be in one.
        let leaves: Vec<_> = out
            .blocks
            .iter()
            .filter(|b| {
                matches!(
                    b.kind,
                    BlockKind::Paragraph
                        | BlockKind::Heading(_)
                        | BlockKind::CodeBlock { .. }
                        | BlockKind::HtmlBlock
                )
            })
            .map(|b| b.range)
            .collect();
        for s in out.map.iter() {
            if matches!(s.kind, SpanKind::Text | SpanKind::Replaced(_))
                && !leaves
                    .iter()
                    .any(|l| l.start <= s.range.start && s.range.end <= l.end)
            {
                failures.push(format!(
                    "example {n}: visible span {:?} {:?} outside any leaf block",
                    s.range,
                    &src[s.range.clone()]
                ));
            }
        }
    }
    other.dedup();
    eprintln!("{} examples; span kinds: {kinds:?}", examples.len());
    eprintln!("examples with Syntax::Other: {other:?}");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
