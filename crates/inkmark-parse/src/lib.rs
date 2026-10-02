//! `MarkdownParser` trait, CommonMark (`PulldownParser`) and GFM
//! (`GfmParser`) impls, BlockTree, SourceMap, rebase.

mod autolinks;
mod chunked;
mod images;
mod links;
mod map;
mod pulldown;
mod state;
mod tree;

pub use images::{InlineImage, inline_images};
pub use links::{Link, definition_label, footnote_offset, heading_offset, is_link, link_at, slug};
pub use map::{SourceMap, Span, SpanKind, Style, Syntax};
pub use pulldown::{GfmParser, PulldownParser};
pub use state::{DEBOUNCE, ParseState};
pub use tree::{Block, BlockKind, BlockTree, Leaf};

/// What a parser hands the views: block structure plus a byte-exact map of
/// the source. A GFM parser (e.g. comrak) implements this same trait.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseOutput {
    pub blocks: BlockTree,
    pub map: SourceMap,
    /// Link reference definitions (`[label]: dest`), keyed by
    /// [`normalize_label`]. Local reparses only add to these; the next
    /// full parse drops ones that were deleted.
    pub link_defs: std::collections::HashMap<String, String>,
    /// Labels of footnote definitions (`[^label]:`), as written. Like
    /// `link_defs`, local reparses only add to these.
    pub footnotes: std::collections::BTreeSet<String>,
}

/// A link label as CommonMark matches it: case-folded, with runs of
/// whitespace collapsed.
pub fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub trait MarkdownParser: Send + Sync {
    fn parse(&self, src: &str) -> ParseOutput;
}
