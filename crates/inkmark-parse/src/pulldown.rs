use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::map::{SourceMap, Span, SpanKind, Style, Syntax};
use crate::tree::{Block, BlockKind, BlockTree};
use crate::{MarkdownParser, ParseOutput};

/// CommonMark via pulldown-cmark (no extensions).
#[derive(Clone, Copy, Debug, Default)]
pub struct PulldownParser;

impl MarkdownParser for PulldownParser {
    fn parse(&self, src: &str) -> ParseOutput {
        Builder::new(src, Options::empty()).run()
    }
}

/// GitHub Flavored Markdown: CommonMark plus tables, strikethrough, task
/// lists, footnotes (pulldown-cmark's extensions) and autolink literals
/// (ours).
#[derive(Clone, Copy, Debug, Default)]
pub struct GfmParser;

impl MarkdownParser for GfmParser {
    fn parse(&self, src: &str) -> ParseOutput {
        let mut out = Builder::new(src, GFM_OPTIONS).run();
        crate::autolinks::mark(src, &mut out.map);
        out
    }
}

/// pulldown-cmark's extensions that make up GFM (autolinks are ours).
pub(crate) const GFM_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_FOOTNOTES);

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
    InlineHtml,
    FootnoteReference,
    Strikethrough,
    Table,
    TableHead,
    TableRow,
    TableCell,
    FootnoteDefinition,
    /// Extension tags we don't enable; kept so the stack stays balanced.
    Other,
}

struct Builder<'a> {
    src: &'a str,
    options: Options,
    spans: Vec<Span>,
    blocks: Vec<Block>,
    stack: Vec<Open>,
    /// End of the last emitted span.
    cursor: usize,
    /// Implicit paragraph for the text of a tight list item (pulldown-cmark
    /// emits none), so every piece of inline content has a leaf block.
    tight: Option<usize>,
    footnotes: std::collections::BTreeSet<String>,
}

impl<'a> Builder<'a> {
    fn new(src: &'a str, options: Options) -> Self {
        Self {
            src,
            options,
            spans: Vec::new(),
            blocks: Vec::new(),
            stack: Vec::new(),
            cursor: 0,
            tight: None,
            footnotes: Default::default(),
        }
    }

    fn run(mut self) -> ParseOutput {
        let mut events = Parser::new_ext(self.src, self.options).into_offset_iter();
        for (event, range) in events.by_ref() {
            self.track_tight_item(&event, &range);
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
                Event::Html(text) => self.text(range, &text),
                Event::InlineHtml(text) => {
                    // Shown as raw source, styled like code.
                    self.stack.push(Open {
                        tag: OpenTag::InlineHtml,
                        block: None,
                    });
                    self.text(range, &text);
                    self.stack.pop();
                }
                Event::SoftBreak => {
                    self.fill_to(range.start);
                    self.push(range, SpanKind::SoftBreak);
                }
                Event::HardBreak => {
                    self.fill_to(range.start);
                    self.push(range, SpanKind::Syntax(Syntax::HardBreak));
                }
                Event::TaskListMarker(checked) => {
                    self.fill_to(range.start);
                    // The marker owns the space after it, so `[x] done`
                    // never shows as `[x]done`.
                    let mut range = range;
                    if self.src.as_bytes().get(range.end) == Some(&b' ') {
                        range.end += 1;
                    }
                    self.push(range, SpanKind::Syntax(Syntax::TaskMarker(checked)));
                }
                Event::FootnoteReference(label) => {
                    // Shown as `[label]` until the caret touches it, like an
                    // entity; then as typed.
                    self.fill_to(range.start);
                    self.stack.push(Open {
                        tag: OpenTag::FootnoteReference,
                        block: None,
                    });
                    self.push(range, SpanKind::Replaced(format!("[{label}]").into()));
                    self.stack.pop();
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
        let link_defs = events
            .reference_definitions()
            .iter()
            .map(|(label, def)| (crate::normalize_label(label), def.dest.to_string()))
            .collect();
        ParseOutput {
            blocks: BlockTree::from_blocks(self.blocks),
            map: SourceMap::from_spans(self.spans),
            link_defs,
            footnotes: self.footnotes,
        }
    }

    fn track_tight_item(&mut self, event: &Event, range: &Range<usize>) {
        let inline = match event {
            Event::Text(_)
            | Event::Code(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak => true,
            Event::Start(tag) => !is_block(tag),
            Event::End(end) => !is_block_end(*end),
            _ => false,
        };
        if !inline {
            self.tight = None;
            return;
        }
        if self.tight.is_none() {
            let in_item = self
                .stack
                .iter()
                .rev()
                .find(|o| o.block.is_some())
                .map(|o| o.tag)
                == Some(OpenTag::Item);
            if !in_item || range.is_empty() {
                return;
            }
            self.blocks.push(Block {
                kind: BlockKind::Paragraph,
                range: range.start..range.start,
                depth: self.depth(),
            });
            self.tight = Some(self.blocks.len() - 1);
        }
        if let Some(i) = self.tight {
            let block = &mut self.blocks[i].range;
            block.end = block.end.max(range.end);
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
            Tag::Strikethrough => (OpenTag::Strikethrough, None),
            Tag::Table(alignments) => (
                OpenTag::Table,
                Some(BlockKind::Table {
                    columns: alignments.len() as u16,
                }),
            ),
            Tag::TableHead => (OpenTag::TableHead, Some(BlockKind::TableHead)),
            Tag::TableRow => (OpenTag::TableRow, Some(BlockKind::TableRow)),
            Tag::TableCell => (OpenTag::TableCell, Some(BlockKind::TableCell)),
            Tag::FootnoteDefinition(label) => {
                self.footnotes.insert(label.into_string());
                (
                    OpenTag::FootnoteDefinition,
                    Some(BlockKind::FootnoteDefinition),
                )
            }
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
                OpenTag::HtmlBlock | OpenTag::InlineHtml => style.insert(Style::HTML),
                OpenTag::List | OpenTag::Item => style.insert(Style::LIST),
                OpenTag::Emphasis => style.insert(Style::EMPHASIS),
                OpenTag::Strong => style.insert(Style::STRONG),
                OpenTag::Link => style.insert(Style::LINK),
                OpenTag::Image => style.insert(Style::IMAGE),
                OpenTag::Strikethrough => style.insert(Style::STRIKE),
                OpenTag::TableHead => style.insert(Style::TABLE_HEAD),
                OpenTag::FootnoteReference => style.insert(Style::FOOTNOTE),
                OpenTag::Paragraph
                | OpenTag::FootnoteDefinition
                | OpenTag::Other
                | OpenTag::Table
                | OpenTag::TableRow
                | OpenTag::TableCell => {}
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
        // The leaf block starts after this indentation; it now renders it.
        if let Some(i) = self.stack.iter().rev().find_map(|o| o.block) {
            let block = &mut self.blocks[i].range;
            block.start = block.start.min(start);
        }
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
        let in_table = self.stack.iter().any(|o| {
            matches!(
                o.tag,
                OpenTag::Table | OpenTag::TableHead | OpenTag::TableRow | OpenTag::TableCell
            )
        });
        let kind = if in_table && !text.contains('\n') {
            // Cell padding is part of the row's markup, not droppable
            // whitespace: a revealed or edited row keeps its spacing.
            SpanKind::Syntax(Syntax::TableMarkup)
        } else if text.trim().is_empty() {
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
        let container = self.stack.iter().rev().map(|o| o.tag).find(|t| {
            matches!(
                t,
                OpenTag::BlockQuote | OpenTag::Item | OpenTag::FootnoteDefinition
            )
        });
        let line_start = at == 0 || self.src.as_bytes()[at - 1] == b'\n';

        if text == "\\" {
            return Syntax::Escape;
        }
        match innermost_block {
            Some(OpenTag::Heading(_)) if innermost_inline.is_none() => {
                return Syntax::HeadingMarker;
            }
            Some(OpenTag::FencedCode) => return Syntax::Fence,
            Some(OpenTag::Table | OpenTag::TableHead | OpenTag::TableRow | OpenTag::TableCell)
                if innermost_inline.is_none() =>
            {
                return Syntax::TableMarkup;
            }
            _ => {}
        }
        if line_start && let Some(c) = container {
            return container_syntax(c, text);
        }
        match innermost_inline {
            Some(OpenTag::Emphasis | OpenTag::Strong | OpenTag::Strikethrough) => Syntax::Delimiter,
            Some(OpenTag::Link | OpenTag::Image) => Syntax::LinkMarkup,
            _ => container.map_or(Syntax::Other, |c| container_syntax(c, text)),
        }
    }
}

fn container_syntax(container: OpenTag, text: &str) -> Syntax {
    if container == OpenTag::FootnoteDefinition {
        Syntax::FootnoteLabel
    } else if container == OpenTag::BlockQuote || text.trim_start().starts_with('>') {
        Syntax::QuotePrefix
    } else {
        Syntax::ListMarker
    }
}

fn is_block(tag: &Tag) -> bool {
    !matches!(
        tag,
        Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::Link { .. }
            | Tag::Image { .. }
    )
}

fn is_block_end(end: TagEnd) -> bool {
    !matches!(
        end,
        TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link
            | TagEnd::Image
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(src: &str) -> Vec<(BlockKind, &str, u16)> {
        PulldownParser
            .parse(src)
            .blocks
            .iter()
            .map(|b| (b.kind, &src[b.range], b.depth))
            .collect()
    }

    #[test]
    fn tight_list_items_get_implicit_paragraphs() {
        let src = "- one *em*\n  more\n- two\n  - nested\n";
        assert_eq!(
            blocks(src),
            vec![
                (
                    BlockKind::List {
                        ordered: false,
                        start: 1
                    },
                    src,
                    0
                ),
                (BlockKind::Item, "- one *em*\n  more\n", 1),
                (BlockKind::Paragraph, "one *em*\n  more", 2),
                (BlockKind::Item, "- two\n  - nested\n", 1),
                (BlockKind::Paragraph, "two", 2),
                (
                    BlockKind::List {
                        ordered: false,
                        start: 1
                    },
                    "- nested\n",
                    2
                ),
                (BlockKind::Item, "- nested\n", 3),
                (BlockKind::Paragraph, "nested", 4),
            ]
        );
    }

    #[test]
    fn leaves_carry_their_containers() {
        let src = "intro\n\n> - one\n>   two\n> - three\n\nend\n";
        let tree = PulldownParser.parse(src).blocks;
        let walk = |from: usize| -> Vec<(String, Vec<BlockKind>)> {
            tree.leaves_from(from)
                .map(|l| {
                    (
                        src[l.block.range].trim().to_owned(),
                        l.containers.iter().map(|c| c.kind).collect(),
                    )
                })
                .collect()
        };
        let quote_item = vec![
            BlockKind::BlockQuote,
            BlockKind::List {
                ordered: false,
                start: 1,
            },
            BlockKind::Item,
        ];
        let all = walk(0);
        assert_eq!(all[0], ("intro".into(), vec![]));
        assert_eq!(all[1], ("one\n>   two".into(), quote_item.clone()));
        assert_eq!(all[2], ("three".into(), quote_item.clone()));
        assert_eq!(all[3], ("end".into(), vec![]));
        // Starting mid-way still knows the containers.
        let mid = src.find("two").unwrap();
        assert_eq!(walk(mid)[0], ("one\n>   two".into(), quote_item));
        // Starting in a gap gives the next leaf.
        assert_eq!(walk(src.find("\n\nend").unwrap() + 1)[0].0, "end");
    }

    #[test]
    fn item_index_counts_siblings_only() {
        let src = "3. a\n3. b\n   - x\n   - y\n3. c\n";
        let tree = PulldownParser.parse(src).blocks;
        let blocks: Vec<_> = tree.iter().collect();
        let list = &blocks[0];
        let items: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Item && b.depth == 1)
            .map(|b| tree.item_index(list, b.range.start))
            .collect();
        assert_eq!(items, vec![0, 1, 2]);
    }

    #[test]
    fn table_padding_and_task_spaces_are_not_hidden() {
        // Regression for #21.
        use crate::GfmParser;
        let src = "|  a  |\n| --- |\n|  b  |\n";
        let pad = GfmParser
            .parse(src)
            .map
            .iter()
            .find(|s| s.range == (1..3))
            .unwrap();
        assert_eq!(pad.kind, SpanKind::Syntax(Syntax::TableMarkup));
        let src = "- [x] done\n";
        let marker = GfmParser
            .parse(src)
            .map
            .iter()
            .find(|s| matches!(s.kind, SpanKind::Syntax(Syntax::TaskMarker(_))))
            .unwrap();
        assert_eq!(&src[marker.range], "[x] ");
    }

    #[test]
    fn loose_lists_keep_their_own_paragraphs() {
        let src = "- one\n\n- two\n";
        let kinds: Vec<_> = blocks(src).into_iter().map(|(k, _, d)| (k, d)).collect();
        assert_eq!(
            kinds,
            vec![
                (
                    BlockKind::List {
                        ordered: false,
                        start: 1
                    },
                    0
                ),
                (BlockKind::Item, 1),
                (BlockKind::Paragraph, 2),
                (BlockKind::Item, 1),
                (BlockKind::Paragraph, 2),
            ]
        );
    }

    #[test]
    fn footnotes_map_every_byte() {
        let src = "Text[^1] and[^long].\n\n[^1]: One.\n\n[^long]: First\n    lazy line.\n\n    Second para.\n\n> Quoted[^1]\n\n- tight item[^1]\n- [^1] first\n\nNo[^missing] def.\n";
        let out = GfmParser.parse(src);
        out.map.validate(src.len()).unwrap();
        // Every reference is inside a leaf, so the live view draws it; a
        // tight list item's implicit paragraph has to take it in.
        let leaves: Vec<_> = out
            .blocks
            .iter()
            .filter(|b| b.kind.is_leaf())
            .map(|b| b.range)
            .collect();
        for s in out.map.iter().filter(|s| s.style.contains(Style::FOOTNOTE)) {
            assert!(
                leaves
                    .iter()
                    .any(|l| l.start <= s.range.start && s.range.end <= l.end),
                "{:?} at {:?} is outside every leaf",
                &src[s.range.clone()],
                s.range
            );
        }
        let shown: Vec<(&str, SpanKind)> = out
            .map
            .iter()
            .filter(|s| s.style.contains(Style::FOOTNOTE))
            .map(|s| (&src[s.range], s.kind))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("[^1]", SpanKind::Replaced("[1]".into())),
                ("[^long]", SpanKind::Replaced("[long]".into())),
                ("[^1]", SpanKind::Replaced("[1]".into())),
                ("[^1]", SpanKind::Replaced("[1]".into())),
                ("[^1]", SpanKind::Replaced("[1]".into())),
            ],
            "a reference without a definition stays text"
        );
        let labels: Vec<&str> = out
            .map
            .iter()
            .filter(|s| s.kind == SpanKind::Syntax(Syntax::FootnoteLabel))
            .map(|s| &src[s.range])
            .collect();
        assert_eq!(labels, vec!["[^1]: ", "[^long]: "]);
        let defs: Vec<(&str, u16)> = out
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::FootnoteDefinition)
            .map(|b| (&src[b.range], b.depth))
            .collect();
        assert_eq!(
            defs,
            vec![
                ("[^1]: One.\n\n", 0),
                ("[^long]: First\n    lazy line.\n\n    Second para.\n\n", 0),
            ]
        );
        // The note's paragraphs are leaves inside the definition.
        let paragraphs: Vec<(&str, u16)> = out
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Paragraph && b.depth == 1)
            .map(|b| (&src[b.range], b.depth))
            .collect();
        assert!(
            paragraphs.contains(&("Second para.\n", 1)),
            "{paragraphs:?}"
        );
        assert_eq!(
            out.footnotes.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["1", "long"]
        );
    }

    #[test]
    fn commonmark_has_no_footnotes() {
        let src = "Text[^1].\n\n[^1]: One.\n";
        let out = PulldownParser.parse(src);
        assert!(out.map.iter().all(|s| !s.style.contains(Style::FOOTNOTE)));
        assert!(out.footnotes.is_empty());
    }
}
