use std::ops::Range;

use inkmark_buffer::{Bias, Change};

use crate::chunked::{Chunked, Ranged};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// A GFM table. The live view lays it out as a whole; its rows and
    /// cells are blocks inside it.
    Table {
        columns: u16,
    },
    TableHead,
    TableRow,
    TableCell,
    /// `[^label]: ...`: a container, like a list item, for the note's blocks.
    FootnoteDefinition,
}

impl BlockKind {
    /// Leaf blocks hold inline content (or none); the rest contain blocks.
    /// A table counts as one leaf: its rows and cells are laid out with it.
    pub fn is_leaf(self) -> bool {
        !matches!(
            self,
            Self::BlockQuote | Self::List { .. } | Self::Item | Self::FootnoteDefinition
        )
    }

    /// Rows and cells live inside a table leaf.
    pub fn is_table_part(self) -> bool {
        matches!(self, Self::TableHead | Self::TableRow | Self::TableCell)
    }
}

/// A leaf block with the containers around it, outermost first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub block: Block,
    pub containers: Vec<Block>,
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

    /// Leaf blocks in document order, starting with the one containing
    /// `offset` (or the first after it).
    pub fn leaves_from(&self, offset: usize) -> impl Iterator<Item = Leaf> + '_ {
        // Containers around `offset`, found by walking back to the
        // top-level block that holds it.
        let mut path: Vec<Block> = Vec::new();
        let mut min_depth = u16::MAX;
        for b in self.blocks.iter_back_from(offset) {
            if b.range.start <= offset && b.range.end > offset && b.depth < min_depth {
                min_depth = b.depth;
                if !b.kind.is_leaf() {
                    path.push(b.clone());
                }
            }
            if b.depth == 0 && b.range.start <= offset {
                break;
            }
        }
        path.reverse();
        let mut stack = path;
        self.blocks
            .iter_from(offset)
            .filter(move |b| {
                if b.kind.is_table_part() {
                    // Yielded with their table.
                    false
                } else if b.kind.is_leaf() {
                    b.range.end > offset
                } else {
                    b.range.start > offset
                }
            })
            .filter_map(move |b| {
                while stack.last().is_some_and(|c| c.range.end <= b.range.start) {
                    stack.pop();
                }
                if b.kind.is_leaf() {
                    Some(Leaf {
                        block: b,
                        containers: stack.clone(),
                    })
                } else {
                    stack.push(b);
                    None
                }
            })
    }

    /// The rows of `table` (header first), each with its cells, in order.
    pub fn table_rows(&self, table: &Block) -> Vec<(Block, Vec<Block>)> {
        let mut rows: Vec<(Block, Vec<Block>)> = Vec::new();
        for b in self
            .blocks
            .iter_from(table.range.start)
            .skip_while(|b| b.range.start < table.range.start)
            .take_while(|b| b.range.start < table.range.end || b.range.is_empty())
        {
            if b.range.start > table.range.end {
                break;
            }
            match b.kind {
                BlockKind::TableHead | BlockKind::TableRow if b.depth == table.depth + 1 => {
                    rows.push((b, Vec::new()))
                }
                BlockKind::TableCell if b.depth == table.depth + 2 => {
                    if let Some((_, cells)) = rows.last_mut() {
                        cells.push(b);
                    }
                }
                _ => {}
            }
        }
        rows
    }

    /// Zero-based position of the item starting at `item_start` among the
    /// items of `list`.
    pub fn item_index(&self, list: &Block, item_start: usize) -> usize {
        self.blocks
            .iter_from(list.range.start)
            .skip_while(|b| b.range.start < list.range.start)
            .take_while(|b| b.range.start < item_start)
            .filter(|b| b.kind == BlockKind::Item && b.depth == list.depth + 1)
            .count()
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
