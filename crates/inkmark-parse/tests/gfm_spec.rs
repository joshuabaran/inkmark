//! Every GFM spec example through `GfmParser`: byte-exact coverage, visible
//! text matching pulldown-cmark's (with the same extensions), and the GFM
//! extensions classified: table markup, task markers, strikethrough, and
//! autolink literals matching the spec's expected links.

use std::collections::BTreeMap;

use inkmark_parse::{BlockKind, GfmParser, MarkdownParser, ParseOutput, SpanKind, Style, Syntax};
use pulldown_cmark::{Event, Options, Parser};

struct Example {
    number: u64,
    extension: Option<String>,
    markdown: String,
    html: String,
}

fn examples() -> Vec<Example> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/gfm/spec.json");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    json.as_array()
        .unwrap()
        .iter()
        .map(|e| Example {
            number: e["example"].as_u64().unwrap(),
            extension: e["extension"].as_str().map(str::to_owned),
            markdown: e["markdown"].as_str().unwrap().to_owned(),
            html: e["html"].as_str().unwrap().to_owned(),
        })
        .collect()
}

fn rendered_text(src: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    Parser::new_ext(src, options)
        .filter_map(|e| match e {
            Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                Some(t.into_string())
            }
            // The live view shows a reference as its label in brackets.
            Event::FootnoteReference(label) => Some(format!("[{label}]")),
            _ => None,
        })
        .collect()
}

fn mapped_text(src: &str, out: &ParseOutput) -> String {
    out.map
        .iter()
        .filter_map(|s| match s.kind {
            SpanKind::Text => Some(src[s.range].to_owned()),
            SpanKind::Replaced(r) => Some(r.into_string()),
            _ => None,
        })
        .collect()
}

/// Runs of link-styled text that aren't inside `[...](...)` markup: the
/// autolink literals.
fn autolinked(src: &str, out: &ParseOutput) -> Vec<String> {
    let spans: Vec<_> = out.map.iter().collect();
    let mut runs: Vec<String> = Vec::new();
    let mut prev_link = false;
    for s in &spans {
        let link = s.kind == SpanKind::Text && s.style.contains(Style::LINK);
        if link {
            if prev_link {
                runs.last_mut().unwrap().push_str(&src[s.range.clone()]);
            } else {
                runs.push(src[s.range.clone()].to_owned());
            }
        }
        prev_link = link;
    }
    runs
}

/// Link texts in the expected HTML.
fn expected_links(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<a href=") {
        rest = &rest[i..];
        let text_start = rest.find('>').unwrap() + 1;
        let text_end = rest.find("</a>").unwrap();
        out.push(
            rest[text_start..text_end]
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\""),
        );
        rest = &rest[text_end..];
    }
    out
}

#[test]
fn gfm_spec_examples() {
    let mut failures = Vec::new();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let examples = examples();
    for ex in &examples {
        let (n, src) = (ex.number, ex.markdown.as_str());
        let out = GfmParser.parse(src);
        if let Err(e) = out.map.validate(src.len()) {
            failures.push(format!("example {n}: coverage: {e}"));
            continue;
        }
        let (want, got) = (rendered_text(src), mapped_text(src, &out));
        if want != got {
            failures.push(format!(
                "example {n}: text\n  want {want:?}\n  got  {got:?}"
            ));
        }
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
                        | BlockKind::TableCell
                )
            })
            .map(|b| b.range)
            .collect();
        let tables: Vec<_> = out
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Table { .. }))
            .map(|b| b.range)
            .collect();
        for s in out.map.iter() {
            let key = match &s.kind {
                SpanKind::Syntax(k) => format!("{k:?}"),
                SpanKind::Replaced(_) => "Replaced".into(),
                k => format!("{k:?}"),
            };
            *kinds.entry(key).or_default() += 1;
            if matches!(s.kind, SpanKind::Text | SpanKind::Replaced(_))
                && !leaves
                    .iter()
                    .any(|l| l.start <= s.range.start && s.range.end <= l.end)
            {
                failures.push(format!(
                    "example {n}: visible {:?} outside any leaf",
                    &src[s.range.clone()]
                ));
            }
            if s.kind == SpanKind::Syntax(Syntax::Other)
                && tables
                    .iter()
                    .any(|t| t.start <= s.range.start && s.range.end <= t.end)
            {
                failures.push(format!(
                    "example {n}: unclassified {:?} in a table",
                    &src[s.range.clone()]
                ));
            }
        }
        match ex.extension.as_deref() {
            Some("autolink") => {
                let (want, got) = (expected_links(&ex.html), autolinked(src, &out));
                if want != got {
                    failures.push(format!(
                        "example {n}: autolinks\n  want {want:?}\n  got  {got:?}"
                    ));
                }
            }
            Some("table") if ex.html.contains("<table>") && tables.is_empty() => {
                failures.push(format!("example {n}: no table block"));
            }
            Some("strikethrough") => {
                let struck: String = out
                    .map
                    .iter()
                    .filter(|s| s.kind == SpanKind::Text && s.style.contains(Style::STRIKE))
                    .map(|s| src[s.range].to_owned())
                    .collect();
                let want: String = ex
                    .html
                    .split("<del>")
                    .skip(1)
                    .map(|p| p.split("</del>").next().unwrap())
                    .collect();
                if struck != want {
                    failures.push(format!("example {n}: struck {struck:?}, want {want:?}"));
                }
            }
            _ => {}
        }
        // Task list items: one marker span per checkbox in the HTML.
        let boxes = ex.html.matches("type=\"checkbox\"").count();
        if boxes > 0 {
            let markers = out
                .map
                .iter()
                .filter(|s| matches!(s.kind, SpanKind::Syntax(Syntax::TaskMarker(_))))
                .count();
            if markers != boxes {
                failures.push(format!("example {n}: {markers} task markers, want {boxes}"));
            }
        }
    }
    eprintln!("{} GFM examples; span kinds: {kinds:?}", examples.len());
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
