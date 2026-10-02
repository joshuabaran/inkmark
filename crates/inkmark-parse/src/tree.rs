use std::ops::Range;

use inkmark_buffer::{Bias, Change};

use crate::chunked::{Chunked, Ranged};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    BlockQuote,
    /// `start` is the first number of an ordered list.
    List {
        ordered: bool,
        start: u64,
    },
    Item,
    CodeBlock {
        fenced: bool,
    },
    HtmlBlock,
    ThematicBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub range: Range<usize>,
    /// Nesting depth; 0 is top level. Parents are implied by pre-order.
    pub depth: u16,
}

/// Blocks in document (pre-)order. Storing depth instead of parent indices
/// keeps splicing in a re-parsed region cheap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockTree {
    blocks: Chunked<Block>,
}

impl Ranged for Block {
    fn range(&self) -> &Range<usize> {
        &self.range
    }

    fn range_mut(&mut self) -> &mut Range<usize> {
        &mut self.range
    }
}

impl BlockTree {
    pub(crate) fn from_blocks(blocks: Vec<Block>) -> Self {
        Self {
            blocks: Chunked::from_vec(blocks),
        }
    }

    /// All blocks, in document order.
    pub fn iter(&self) -> impl Iterator<Item = Block> + '_ {
        self.blocks.iter()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn top_level(&self) -> impl Iterator<Item = Block> + '_ {
        self.iter().filter(|b| b.depth == 0)
    }

    /// Byte span of the top-level blocks overlapping `range`, widened by
    /// `neighbours` top-level blocks on each side. A local reparse covers the
    /// edit plus one neighbour, since edits can merge or split blocks.
    pub(crate) fn top_level_span(
        &self,
        range: Range<usize>,
        neighbours: usize,
    ) -> Option<Range<usize>> {
        let mut blocks = Vec::new();
        let mut back = self
            .blocks
            .iter_back_from(range.start)
            .filter(|b| b.depth == 0 && b.range.start <= range.start);
        if let Some(b) = back.next() {
            let overlaps = b.range.end > range.start;
            blocks.push(b);
            let more = if overlaps {
                neighbours
            } else {
                neighbours.saturating_sub(1)
            };
            blocks.extend(back.take(more));
            if !overlaps && neighbours == 0 {
                blocks.clear();
            }
        }
        let forward = self
            .blocks
            .iter_from(range.start)
            .filter(|b| b.depth == 0 && b.range.start > range.start);
        let mut extra = 0;
        for b in forward {
            if b.range.start >= range.end {
                if extra == neighbours {
                    break;
                }
                extra += 1;
            }
            blocks.push(b);
        }
        let start = blocks.iter().map(|b| b.range.start).min()?;
        let end = blocks.iter().map(|b| b.range.end).max()?;
        Some(start..end)
    }

    pub(crate) fn rebase(&mut self, change: &Change) {
        let delta = change.new_end as isize - change.old_end as isize;
        self.blocks.rebase(
            change.start,
            change.old_end,
            delta,
            |r| change.map(r.start, Bias::Left)..change.map(r.end, Bias::Right),
            |b| !b.range.is_empty(),
        );
    }

    /// Replaces the blocks inside `region` (whole top-level blocks and their
    /// children) with `blocks`, which are already in document offsets.
    pub(crate) fn splice(&mut self, region: Range<usize>, blocks: Vec<Block>) {
        self.blocks.edit_region(region.clone(), |v| {
            v.retain(|b| !(b.range.start >= region.start && b.range.end <= region.end));
            let at = v.partition_point(|b| b.range.start < region.start);
            v.splice(at..at, blocks);
        });
    }
}
