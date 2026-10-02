//! `MarkdownParser` trait, pulldown-cmark impl, BlockTree, SourceMap, rebase.

mod chunked;
mod map;
mod pulldown;
mod state;
mod tree;

pub use map::{SourceMap, Span, SpanKind, Style, Syntax};
pub use pulldown::PulldownParser;
pub use state::{DEBOUNCE, ParseState};
pub use tree::{Block, BlockKind, BlockTree, Leaf};

/// What a parser hands the views: block structure plus a byte-exact map of
/// the source. A GFM parser (e.g. comrak) implements this same trait.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseOutput {
    pub blocks: BlockTree,
    pub map: SourceMap,
}

pub trait MarkdownParser: Send + Sync {
    fn parse(&self, src: &str) -> ParseOutput;
}
