//! The Markdown syntax cheatsheet: one embedded source, drawn as the code
//! pane would type it and as the live pane would render it.
//!
//! Rows live in `cheatsheet.md`. A directive names the element that row
//! must parse to. The panel asks [`LiveView`] to paint the preview, so a
//! row shows the same headings, lists, and tables as a note.

use std::sync::OnceLock;

use egui::{Id, Rect, Sense, pos2, vec2};
use inkmark_buffer::Document;
use inkmark_parse::ParseOutput;
use inkmark_text::{GlyphMeshes, SharedFonts, TextConfig, TextRenderer};

use crate::live_view::LiveView;
use crate::theme;

/// One cheatsheet row. `source` is the Markdown as typed, with a trailing
/// newline. Built from `cheatsheet.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub source: String,
    expect: Expect,
}

/// The element `source` must parse to. The names match the directives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Heading(u8),
    Emphasis,
    Strong,
    Strikethrough,
    InlineCode,
    FencedCode,
    Link,
    Reference,
    Image,
    BlockQuote,
    UnorderedList,
    OrderedList,
    TaskList,
    Table,
    Rule,
    Footnote,
    HardBreak,
}

/// Every row, in panel order. The file is parsed once.
pub fn samples() -> &'static [Sample] {
    static SAMPLES: OnceLock<Vec<Sample>> = OnceLock::new();
    SAMPLES.get_or_init(load_samples)
}

/// Paints the cheatsheet rows. The app owns one of these and only shows it
/// while the panel is open.
pub struct Cheatsheet {
    live: LiveView,
    code: TextRenderer,
    code_size: f32,
    code_line: f32,
    row_rects: Vec<Rect>,
}

impl Cheatsheet {
    pub fn new(fonts: SharedFonts) -> Self {
        Self {
            live: LiveView::with_fonts(fonts.clone(), Id::new("syntax_cheatsheet")),
            code: TextRenderer::with_fonts(fonts),
            // The code pane's defaults, until the app copies the open tab.
            code_size: 14.0,
            code_line: 21.0,
            row_rects: Vec::new(),
        }
    }

    /// Font sizes from the open tab, so the two columns match the panes.
    pub fn set_type_size(
        &mut self,
        code_size: f32,
        code_line: f32,
        text_size: f32,
        text_line: f32,
    ) {
        self.code_size = code_size;
        self.code_line = code_line;
        self.live.font_size = text_size;
        self.live.line_height = text_line;
    }

    /// The window's image cache, so a row does not start a second decoder.
    pub fn share_images(&mut self, cache: crate::ImageCache, user: u64) {
        self.live.share_images(cache, user);
    }

    /// Drops row rectangles from the previous frame.
    pub fn begin_rows(&mut self) {
        self.row_rects.clear();
    }

    /// Where each row was drawn last frame, in the same order as [`samples`].
    pub fn row_rects(&self) -> &[Rect] {
        &self.row_rects
    }

    /// Draws `source` in the code font and `parse` through the live renderer.
    /// `true` when the row was clicked.
    pub fn show_row(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        source: &str,
        doc: &Document,
        parse: &ParseOutput,
    ) -> bool {
        let response = ui.scope(|ui| {
            ui.columns_const(|[left, right]| {
                self.show_source(left, source);
                self.live.show_snippet(right, doc, parse);
            });
        });
        let rect = response.response.rect;
        self.row_rects.push(rect);
        let hit = ui.interact(rect, Id::new(("syntax_row", index)), Sense::click());
        if hit.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        hit.clicked()
    }

    /// The source, wrapped in the code pane's monospace font.
    fn show_source(&mut self, ui: &mut egui::Ui, source: &str) {
        let width = ui.available_width().max(40.0);
        self.code.begin_frame(
            TextConfig {
                monospace: true,
                font_size: self.code_size,
                line_height: self.code_line,
                wrap_width: Some(width),
            },
            ui.ctx().pixels_per_point(),
        );
        let lines = source_lines(source);
        let mut heights = Vec::with_capacity(lines.len());
        let mut total = 0.0;
        for line in &lines {
            let height = self.code.geometry(line).height().max(1.0);
            heights.push(height);
            total += height;
        }
        let (rect, _) = ui.allocate_exact_size(vec2(width, total), Sense::hover());
        let painter = ui.painter_at(rect);
        let mut meshes = GlyphMeshes::default();
        let color = theme::current(ui.ctx()).text;
        let mut y = rect.top();
        for (line, height) in lines.iter().zip(heights) {
            if !line.is_empty() {
                self.code
                    .draw_line(&mut meshes, line, pos2(rect.left(), y), color);
            }
            y += height;
        }
        self.code.end_frame(meshes, &painter);
    }
}

/// Lines of `source`, without the trailing empty line a final newline adds.
fn source_lines(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.split('\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn load_samples() -> Vec<Sample> {
    let mut samples = Vec::new();
    let mut expect = None;
    let mut body = String::new();
    for line in include_str!("cheatsheet.md").split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(next) = parse_directive(line) {
            push_sample(&mut samples, expect.take(), &body);
            body.clear();
            expect = Some(next);
            continue;
        }
        if expect.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    push_sample(&mut samples, expect.take(), &body);
    assert!(!samples.is_empty(), "cheatsheet.md has no rows");
    samples
}

fn push_sample(samples: &mut Vec<Sample>, expect: Option<Expect>, body: &str) {
    let Some(expect) = expect else {
        return;
    };
    let source = finish_sample(body);
    assert!(
        !source.trim().is_empty(),
        "cheatsheet row {expect:?} has no Markdown"
    );
    samples.push(Sample { source, expect });
}

/// Drops the blank lines around a row and keeps one trailing newline, the
/// way a block is stored in a note.
fn finish_sample(body: &str) -> String {
    let mut text = body
        .trim_start_matches('\n')
        .trim_end_matches('\n')
        .to_owned();
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn parse_directive(line: &str) -> Option<Expect> {
    let trimmed = line.trim();
    if !trimmed.starts_with("<!--") {
        return None;
    }
    let inner = trimmed
        .strip_prefix("<!--")
        .and_then(|rest| rest.strip_suffix("-->"))
        .unwrap_or_else(|| panic!("bad cheatsheet directive: {line}"))
        .trim();
    Some(expect_from(inner))
}

fn expect_from(inner: &str) -> Expect {
    let mut parts = inner.split_whitespace();
    let kind = parts
        .next()
        .unwrap_or_else(|| panic!("empty cheatsheet directive"));
    let expect = match kind {
        "heading" => {
            let level: u8 = parts
                .next()
                .unwrap_or_else(|| panic!("heading needs a level"))
                .parse()
                .unwrap_or_else(|_| panic!("bad heading level in `{inner}`"));
            assert!((1..=6).contains(&level), "heading level {level}");
            Expect::Heading(level)
        }
        "emphasis" => Expect::Emphasis,
        "strong" => Expect::Strong,
        "strikethrough" => Expect::Strikethrough,
        "inline-code" => Expect::InlineCode,
        "fenced-code" => Expect::FencedCode,
        "link" => Expect::Link,
        "reference" => Expect::Reference,
        "image" => Expect::Image,
        "quote" => Expect::BlockQuote,
        "unordered" => Expect::UnorderedList,
        "ordered" => Expect::OrderedList,
        "task" => Expect::TaskList,
        "table" => Expect::Table,
        "rule" => Expect::Rule,
        "footnote" => Expect::Footnote,
        "hard-break" => Expect::HardBreak,
        other => panic!("unknown cheatsheet directive `{other}`"),
    };
    assert!(
        parts.next().is_none(),
        "extra text in cheatsheet directive `{inner}`"
    );
    expect
}

#[cfg(test)]
mod tests {
    use egui::{Pos2, pos2};
    use inkmark_parse::{BlockKind, GfmParser, MarkdownParser, SpanKind, Style, Syntax};

    use super::*;

    /// Whether `parse` of `sample.source` contains the element the directive named.
    fn has_expect(parse: &inkmark_parse::ParseOutput, expect: Expect) -> bool {
        let block = |kind: BlockKind| parse.blocks.iter().any(|block| block.kind == kind);
        let styled = |style: Style| {
            parse.map.iter().any(|span| {
                span.style.contains(style)
                    && matches!(span.kind, SpanKind::Text | SpanKind::Replaced(_))
            })
        };
        match expect {
            Expect::Heading(level) => block(BlockKind::Heading(level)),
            Expect::Emphasis => styled(Style::EMPHASIS),
            Expect::Strong => styled(Style::STRONG),
            Expect::Strikethrough => styled(Style::STRIKE),
            Expect::InlineCode => styled(Style::CODE),
            Expect::FencedCode => block(BlockKind::CodeBlock { fenced: true }),
            Expect::Link => styled(Style::LINK) && !styled(Style::IMAGE),
            Expect::Reference => !parse.link_defs.is_empty() && styled(Style::LINK),
            Expect::Image => styled(Style::IMAGE),
            Expect::BlockQuote => block(BlockKind::BlockQuote),
            Expect::UnorderedList => parse
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::List { ordered: false, .. })),
            Expect::OrderedList => parse
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::List { ordered: true, .. })),
            Expect::TaskList => parse
                .map
                .iter()
                .any(|span| matches!(span.kind, SpanKind::Syntax(Syntax::TaskMarker(_)))),
            Expect::Table => parse
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Table { .. })),
            Expect::Rule => block(BlockKind::ThematicBreak),
            Expect::Footnote => block(BlockKind::FootnoteDefinition) && styled(Style::FOOTNOTE),
            Expect::HardBreak => parse
                .map
                .iter()
                .any(|span| span.kind == SpanKind::Syntax(Syntax::HardBreak)),
        }
    }

    #[test]
    fn each_sample_parses_to_its_element() {
        let mut seen = Vec::new();
        for sample in samples() {
            let parse = GfmParser.parse(&sample.source);
            parse
                .map
                .validate(sample.source.len())
                .unwrap_or_else(|error| panic!("{}: {error}", sample.source));
            assert!(
                has_expect(&parse, sample.expect),
                "{:?} did not parse from {:?}",
                sample.expect,
                sample.source
            );
            seen.push(sample.expect);
        }
        for level in 1..=6 {
            assert!(seen.contains(&Expect::Heading(level)), "heading {level}");
        }
        for expect in [
            Expect::Emphasis,
            Expect::Strong,
            Expect::Strikethrough,
            Expect::InlineCode,
            Expect::FencedCode,
            Expect::Link,
            Expect::Reference,
            Expect::Image,
            Expect::BlockQuote,
            Expect::UnorderedList,
            Expect::OrderedList,
            Expect::TaskList,
            Expect::Table,
            Expect::Rule,
            Expect::Footnote,
            Expect::HardBreak,
        ] {
            assert!(seen.contains(&expect), "missing {expect:?}");
        }
    }

    #[test]
    fn disabled_extensions_are_not_rows() {
        // Definition lists, alerts, and metadata stay ordinary blocks.
        // The sheet does not teach syntax the parser will not render.
        let src = "Term\n: definition\n\n> [!NOTE]\n> Look\n\n---\ntitle: x\n---\n";
        let parse = GfmParser.parse(src);
        parse.map.validate(src.len()).unwrap();
        assert!(
            parse
                .blocks
                .iter()
                .any(|block| block.kind == BlockKind::Paragraph)
        );
        assert!(
            parse
                .blocks
                .iter()
                .any(|block| block.kind == BlockKind::BlockQuote)
        );
        let blob: String = samples()
            .iter()
            .map(|sample| sample.source.as_str())
            .collect();
        for needle in ["[!NOTE]", "[[", "^2^", "+++\n"] {
            assert!(!blob.contains(needle), "{needle} is on the sheet");
        }
    }

    #[test]
    fn each_sample_paints_through_the_live_renderer() {
        let ctx = egui::Context::default();
        let mut sheet = Cheatsheet::new(inkmark_text::Fonts::shared(&ctx));
        let mut time = 0.0;
        for (index, sample) in samples().iter().enumerate() {
            let doc = Document::from_text(&sample.source);
            let parse = GfmParser.parse(&sample.source);
            time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0))),
                time: Some(time),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                sheet.show_row(ui, index, &sample.source, &doc, &parse);
            });
            out.textures_delta.clear();
            assert!(
                out.shapes
                    .iter()
                    .any(|clipped| shape_visible(&clipped.shape)),
                "nothing painted for {:?}",
                sample.source
            );
        }
    }

    fn shape_visible(shape: &egui::Shape) -> bool {
        match shape {
            egui::Shape::Noop => false,
            egui::Shape::Vec(shapes) => shapes.iter().any(shape_visible),
            egui::Shape::Mesh(mesh) => !mesh.vertices.is_empty(),
            egui::Shape::Text(_)
            | egui::Shape::LineSegment { .. }
            | egui::Shape::Rect(_)
            | egui::Shape::Path(_)
            | egui::Shape::Circle(_)
            | egui::Shape::Ellipse(_)
            | egui::Shape::QuadraticBezier(_)
            | egui::Shape::CubicBezier(_)
            | egui::Shape::Callback(_) => true,
        }
    }
}
