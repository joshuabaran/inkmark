use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

use crate::map::{SourceMap, Span, SpanKind, Style, Syntax};
use crate::tree::{Block, BlockKind, BlockTree};
use crate::{MarkdownParser, ParseOutput};

/// CommonMark via pulldown-cmark (no extensions).
#[derive(Clone, Copy, Debug, Default)]
pub struct PulldownParser;

impl MarkdownParser for PulldownParser {
    fn parse(&self, src: &str) -> ParseOutput {
        Builder::new(src).run()
    }
}

/// An element open while walking events.
struct Open {
    tag: OpenTag,
    /// Index into `blocks` for block elements.
    block: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenTag {
    Paragraph,
    Heading(u8),
    BlockQuote,
    FencedCode,
    IndentedCode,
    HtmlBlock,
    List,
    Item,
    Emphasis,
    Strong,
    Link,
    Image,
    /// Extension tags we don't enable; kept so the stack stays balanced.
    Other,
}

struct Builder<'a> {
    src: &'a str,
    spans: Vec<Span>,
    blocks: Vec<Block>,
    stack: Vec<Open>,
    /// End of the last emitted span.
    cursor: usize,
}

impl<'a> Builder<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            spans: Vec::new(),
            blocks: Vec::new(),
            stack: Vec::new(),
            cursor: 0,
        }
    }

    fn run(mut self) -> ParseOutput {
        for (event, range) in Parser::new(self.src).into_offset_iter() {
            match event {
                Event::Start(tag) => {
                    self.fill_to(range.start);
                    self.open(tag, range);
                }
                Event::End(end) => {
                    // Closing delimiters and trailing markup belong to the element.
                    self.fill_to(range.end);
                    self.close(end);
                }
                Event::Text(text) => self.text(range, &text),
                Event::Code(text) => self.code_span(range, &text),
                Event::Html(text) | Event::InlineHtml(text) => self.text(range, &text),
                Event::SoftBreak => {
                    self.fill_to(range.start);
                    self.push(range, SpanKind::SoftBreak);
                }
                Event::HardBreak => {
                    self.fill_to(range.start);
                    self.push(range, SpanKind::Syntax(Syntax::HardBreak));
                }
                Event::Rule => {
                    self.fill_to(range.start);
                    self.blocks.push(Block {
                        kind: BlockKind::ThematicBreak,
                        range: range.clone(),
                        depth: self.depth(),
                    });
                    self.push(range, SpanKind::Syntax(Syntax::ThematicBreak));
                }
                // Extensions are off; treat anything else as opaque text.
                _ => self.text(range.clone(), &self.src[range]),
            }
        }
        self.fill_to(self.src.len());
        ParseOutput {
            blocks: BlockTree::from_blocks(self.blocks),
            map: SourceMap::from_spans(self.spans),
        }
    }

    fn depth(&self) -> u16 {
        self.stack.iter().filter(|o| o.block.is_some()).count() as u16
    }

    fn open(&mut self, tag: Tag, range: Range<usize>) {
        let (open, block) = match tag {
            Tag::Paragraph => (OpenTag::Paragraph, Some(BlockKind::Paragraph)),
            Tag::Heading { level, .. } => {
                let level = heading_level(level);
                (OpenTag::Heading(level), Some(BlockKind::Heading(level)))
            }
            Tag::BlockQuote(_) => (OpenTag::BlockQuote, Some(BlockKind::BlockQuote)),
            Tag::CodeBlock(CodeBlockKind::Fenced(_)) => (
                OpenTag::FencedCode,
                Some(BlockKind::CodeBlock { fenced: true }),
            ),
            Tag::CodeBlock(CodeBlockKind::Indented) => (
                OpenTag::IndentedCode,
                Some(BlockKind::CodeBlock { fenced: false }),
            ),
            Tag::HtmlBlock => (OpenTag::HtmlBlock, Some(BlockKind::HtmlBlock)),
            Tag::List(start) => (
                OpenTag::List,
                Some(BlockKind::List {
                    ordered: start.is_some(),
                    start: start.unwrap_or(1),
                }),
            ),
            Tag::Item => (OpenTag::Item, Some(BlockKind::Item)),
            Tag::Emphasis => (OpenTag::Emphasis, None),
            Tag::Strong => (OpenTag::Strong, None),
            Tag::Link { .. } => (OpenTag::Link, None),
            Tag::Image { .. } => (OpenTag::Image, None),
            _ => (OpenTag::Other, None),
        };
        let block = block.map(|kind| {
            self.blocks.push(Block {
                kind,
                range,
                depth: self.depth(),
            });
            self.blocks.len() - 1
        });
        self.stack.push(Open { tag: open, block });
    }

    fn close(&mut self, _end: TagEnd) {
        self.stack.pop();
    }

    fn style(&self) -> (Style, u8) {
        let mut style = Style::default();
        let mut heading = 0;
        for open in &self.stack {
            match open.tag {
                OpenTag::Heading(level) => {
                    style.insert(Style::HEADING);
                    heading = level;
                }
                OpenTag::BlockQuote => style.insert(Style::QUOTE),
                OpenTag::FencedCode | OpenTag::IndentedCode => style.insert(Style::CODE_BLOCK),
                OpenTag::HtmlBlock => style.insert(Style::HTML),
                OpenTag::List | OpenTag::Item => style.insert(Style::LIST),
                OpenTag::Emphasis => style.insert(Style::EMPHASIS),
                OpenTag::Strong => style.insert(Style::STRONG),
                OpenTag::Link => style.insert(Style::LINK),
                OpenTag::Image => style.insert(Style::IMAGE),
                OpenTag::Paragraph | OpenTag::Other => {}
            }
        }
        (style, heading)
    }

    fn push(&mut self, range: Range<usize>, kind: SpanKind) {
        // Clip anything the parser reports twice or out of order.
        let range = range.start.max(self.cursor)..range.end;
        if range.is_empty() {
            return;
        }
        let (style, heading) = self.style();
        self.cursor = range.end;
        self.spans.push(Span {
            range,
            kind,
            style,
            heading,
        });
    }

    fn text(&mut self, range: Range<usize>, rendered: &str) {
        self.fill_to(range.start);
        if range.is_empty() {
            return self.carve_synthetic(range.start, rendered);
        }
        let source = &self.src[range.clone()];
        let kind = if source == rendered {
            SpanKind::Text
        } else {
            SpanKind::Replaced(rendered.into())
        };
        self.push(range, kind);
    }

    /// pulldown-cmark reports some indentation it keeps (the rest of a
    /// partly consumed tab, indented lines in HTML blocks) as a zero-width
    /// text event. Those bytes are the whitespace just before `at` on the
    /// same line, already emitted as syntax or whitespace: make them visible.
    fn carve_synthetic(&mut self, at: usize, rendered: &str) {
        let Some(last) = self.spans.last() else {
            return;
        };
        if rendered.is_empty() || last.range.end != at {
            return;
        }
        let line_start = self.src[..at]
            .rfind('\n')
            .map_or(0, |i| i + 1)
            .max(last.range.start);
        let before = &self.src[line_start..at];
        let ws = before.len() - before.trim_end_matches([' ', '\t']).len();
        if ws == 0 {
            return;
        }
        // The whole run if it is exactly the text, else its last char (a tab).
        let start = if &self.src[at - ws..at] == rendered {
            at - ws
        } else {
            at - 1
        };
        let kind = if &self.src[start..at] == rendered {
            SpanKind::Text
        } else {
            SpanKind::Replaced(rendered.into())
        };
        let (style, heading) = self.style();
        let last = self.spans.last_mut().expect("checked above");
        if last.range.start == start {
            self.spans.pop();
        } else {
            last.range.end = start;
        }
        self.spans.push(Span {
            range: start..at,
            kind,
            style,
            heading,
        });
    }

    /// `` `code` ``: backtick runs (and the single padding space CommonMark
    /// strips) are delimiters; the content is text.
    fn code_span(&mut self, range: Range<usize>, rendered: &str) {
        self.fill_to(range.start);
        let source = &self.src[range.clone()];
        let ticks = source.bytes().take_while(|&b| b == b'`').count();
        let mut inner = range.start + ticks..range.end - ticks;
        let inner_src = &self.src[inner.clone()];
        if inner_src.len() == rendered.len() + 2
            && inner_src.starts_with([' ', '\n'])
            && inner_src.ends_with([' ', '\n'])
        {
            inner = inner.start + 1..inner.end - 1;
        }
        let kind = if &self.src[inner.clone()] == rendered {
            SpanKind::Text
        } else {
            SpanKind::Replaced(rendered.into())
        };
        let push = |b: &mut Self, r: Range<usize>, k| {
            let (mut style, heading) = b.style();
            style.insert(Style::CODE);
            if r.start >= b.cursor && !r.is_empty() {
                b.cursor = r.end;
                b.spans.push(Span {
                    range: r,
                    kind: k,
                    style,
                    heading,
                });
            }
        };
        push(
            self,
            range.start..inner.start,
            SpanKind::Syntax(Syntax::Delimiter),
        );
        push(self, inner.clone(), kind);
        push(
            self,
            inner.end..range.end,
            SpanKind::Syntax(Syntax::Delimiter),
        );
    }

    /// Classifies the bytes between the last span and `to` by what encloses them.
    fn fill_to(&mut self, to: usize) {
        if to <= self.cursor {
            return;
        }
        let gap = self.cursor..to;
        let text = &self.src[gap.clone()];
        let kind = if text.trim().is_empty() {
            SpanKind::Whitespace
        } else {
            SpanKind::Syntax(self.classify_gap(gap.start, text))
        };
        self.push(gap, kind);
    }

    fn classify_gap(&self, at: usize, text: &str) -> Syntax {
        let innermost_block = self
            .stack
            .iter()
            .rev()
            .find(|o| o.block.is_some())
            .map(|o| o.tag);
        let innermost_inline = self
            .stack
            .iter()
            .rev()
            .take_while(|o| o.block.is_none())
            .map(|o| o.tag)
            .find(|t| *t != OpenTag::Other);
        let container = self
            .stack
            .iter()
            .rev()
            .map(|o| o.tag)
            .find(|t| matches!(t, OpenTag::BlockQuote | OpenTag::Item));
        let line_start = at == 0 || self.src.as_bytes()[at - 1] == b'\n';

        if text == "\\" {
            return Syntax::Escape;
        }
        match innermost_block {
            Some(OpenTag::Heading(_)) if innermost_inline.is_none() => {
                return Syntax::HeadingMarker;
            }
            Some(OpenTag::FencedCode) => return Syntax::Fence,
            _ => {}
        }
        if line_start && let Some(c) = container {
            return container_syntax(c, text);
        }
        match innermost_inline {
            Some(OpenTag::Emphasis | OpenTag::Strong) => Syntax::Delimiter,
            Some(OpenTag::Link | OpenTag::Image) => Syntax::LinkMarkup,
            _ => container.map_or(Syntax::Other, |c| container_syntax(c, text)),
        }
    }
}

fn container_syntax(container: OpenTag, text: &str) -> Syntax {
    if container == OpenTag::BlockQuote || text.trim_start().starts_with('>') {
        Syntax::QuotePrefix
    } else {
        Syntax::ListMarker
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
