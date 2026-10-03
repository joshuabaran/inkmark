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
pub use links::{
    Heading, Link, definition_label, footnote_offset, heading_offset, headings, is_link, link_at,
    slug,
};
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
    /// [`normalize_label`]: the first definition of a label wins, as in
    /// CommonMark. A local reparse replaces the definitions inside its
    /// region and drops a label nothing defines any more. When a label
    /// starts or stops resolving, references to it are restyled in that
    /// same pass, including in blocks the edit did not touch. A new
    /// destination does not restyle: reference spans do not carry the URL.
    pub link_defs: std::collections::HashMap<String, String>,
    /// Labels of footnote definitions (`[^label]:`), as written.
    pub footnotes: std::collections::BTreeSet<String>,
    /// Where each link and footnote definition is, in source order, so a
    /// local reparse can drop the ones an edit deleted. Every occurrence is
    /// listed, so when the first of two `[ref]:` goes, the second takes over.
    pub definitions: Vec<Definition>,
}

/// A link reference or footnote definition's place in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub range: std::ops::Range<usize>,
    pub label: DefinitionLabel,
    /// A link definition's destination (empty for a footnote).
    pub dest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DefinitionLabel {
    /// A link reference (`[label]: dest`), normalized as `link_defs` keys.
    Link(String),
    /// A footnote (`[^label]:`), as written.
    Footnote(String),
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
