//! Range-keyed items stored in chunks with chunk-relative offsets, so an
//! edit shifts everything after it in O(chunks) instead of O(items).

use std::ops::Range;

/// Items per chunk when (re)building.
const CHUNK: usize = 512;

pub(crate) trait Ranged: Clone {
    fn range(&self) -> &Range<usize>;
    fn range_mut(&mut self) -> &mut Range<usize>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Chunk<T> {
    /// Absolute offset that item ranges are relative to.
    base: usize,
    /// Largest absolute range end in this chunk.
    max_end: usize,
    items: Vec<T>,
}

/// Items sorted by range start (ties in insertion order). Ranges may nest.
#[derive(Clone, Debug)]
pub(crate) struct Chunked<T> {
    chunks: Vec<Chunk<T>>,
}

/// Equal when the items are, however they happen to be chunked.
impl<T: Ranged + PartialEq> PartialEq for Chunked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<T: Ranged + Eq> Eq for Chunked<T> {}

impl<T> Default for Chunked<T> {
    fn default() -> Self {
        Self { chunks: Vec::new() }
    }
}

fn shift(r: &Range<usize>, by: isize) -> Range<usize> {
    r.start.wrapping_add_signed(by)..r.end.wrapping_add_signed(by)
}

impl<T: Ranged> Chunked<T> {
    /// From items with absolute ranges, sorted by start.
    pub(crate) fn from_vec(items: Vec<T>) -> Self {
        let mut chunks = Vec::with_capacity(items.len() / CHUNK + 1);
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let mut chunk: Vec<T> = items.by_ref().take(CHUNK).collect();
            let base = chunk[0].range().start;
            let max_end = chunk.iter().map(|t| t.range().end).max().unwrap_or(base);
            for t in &mut chunk {
                *t.range_mut() = shift(t.range(), -(base as isize));
            }
            chunks.push(Chunk {
                base,
                max_end,
                items: chunk,
            });
        }
        Self { chunks }
    }

    fn absolute(chunk: &Chunk<T>, t: &T) -> T {
        let mut t = t.clone();
        *t.range_mut() = shift(t.range(), chunk.base as isize);
        t
    }

    pub(crate) fn len(&self) -> usize {
        self.chunks.iter().map(|c| c.items.len()).sum()
    }

    /// All items with absolute ranges, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.chunks
            .iter()
            .flat_map(|c| c.items.iter().map(move |t| Self::absolute(c, t)))
    }

    /// Items from the first one starting at or before `at` (the last such
    /// chunk's start), in order. Callers stop when they've seen enough.
    pub(crate) fn iter_from(&self, at: usize) -> impl Iterator<Item = T> + '_ {
        let first = self
            .chunks
            .partition_point(|c| c.base <= at)
            .saturating_sub(1);
        self.chunks[first..]
            .iter()
            .flat_map(|c| c.items.iter().map(move |t| Self::absolute(c, t)))
    }

    /// Items in reverse order, starting from the chunk holding `at`.
    pub(crate) fn iter_back_from(&self, at: usize) -> impl Iterator<Item = T> + '_ {
        let last = self.chunks.partition_point(|c| c.base <= at);
        self.chunks[..last]
            .iter()
            .rev()
            .flat_map(|c| c.items.iter().rev().map(move |t| Self::absolute(c, t)))
    }

    pub(crate) fn last(&self) -> Option<T> {
        let c = self.chunks.last()?;
        c.items.last().map(|t| Self::absolute(c, t))
    }

    /// Maps every range through an edit of `start..old_end` that shifted
    /// later text by `delta`. Only chunks reaching the edit are touched item
    /// by item; later chunks just move their base. Items for which `keep`
    /// returns false afterwards are dropped.
    pub(crate) fn rebase(
        &mut self,
        start: usize,
        old_end: usize,
        delta: isize,
        map: impl Fn(&Range<usize>) -> Range<usize>,
        keep: impl Fn(&T) -> bool,
    ) {
        for chunk in &mut self.chunks {
            if chunk.base > old_end {
                chunk.base = chunk.base.wrapping_add_signed(delta);
                chunk.max_end = chunk.max_end.wrapping_add_signed(delta);
                continue;
            }
            if chunk.max_end < start {
                continue;
            }
            let base = chunk.base as isize;
            let mut items: Vec<T> = chunk
                .items
                .drain(..)
                .map(|mut t| {
                    *t.range_mut() = map(&shift(t.range(), base));
                    t
                })
                .filter(&keep)
                .collect();
            let Some(first) = items.first() else { continue };
            chunk.base = first.range().start;
            chunk.max_end = items
                .iter()
                .map(|t| t.range().end)
                .max()
                .unwrap_or(chunk.base);
            let rebase = -(chunk.base as isize);
            for t in &mut items {
                *t.range_mut() = shift(t.range(), rebase);
            }
            chunk.items = items;
        }
        self.chunks.retain(|c| !c.items.is_empty());
    }

    /// Rewrites the items starting inside `region` (plus any in the same
    /// chunks): `edit` gets them as an absolute, sorted vector to change in
    /// place, and must leave it sorted.
    pub(crate) fn edit_region(&mut self, region: Range<usize>, edit: impl FnOnce(&mut Vec<T>)) {
        let first = self
            .chunks
            .partition_point(|c| c.base <= region.start)
            .saturating_sub(1);
        let last = self
            .chunks
            .partition_point(|c| c.base < region.end)
            .max(first + 1)
            .min(self.chunks.len());
        let mut items: Vec<T> = self.chunks[first..last]
            .iter()
            .flat_map(|c| c.items.iter().map(move |t| Self::absolute(c, t)))
            .collect();
        edit(&mut items);
        let rebuilt = Self::from_vec(items).chunks;
        self.chunks.splice(first..last, rebuilt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct R(Range<usize>);

    impl Ranged for R {
        fn range(&self) -> &Range<usize> {
            &self.0
        }
        fn range_mut(&mut self) -> &mut Range<usize> {
            &mut self.0
        }
    }

    fn naive_rebase(items: &[R], start: usize, old_end: usize, delta: isize) -> Vec<R> {
        let map = |o: usize| {
            if o >= old_end {
                o.wrapping_add_signed(delta)
            } else {
                o.min(start)
            }
        };
        items
            .iter()
            .map(|r| R(map(r.0.start)..map(r.0.end)))
            .filter(|r| !r.0.is_empty())
            .collect()
    }

    #[test]
    fn rebase_matches_naive_including_enclosing_ranges() {
        // Contiguous small ranges plus one range enclosing most of them.
        let mut items: Vec<R> = (0..5000).map(|i| R(i * 2..i * 2 + 2)).collect();
        items.insert(10, R(20..9000));
        let mut chunked = Chunked::from_vec(items.clone());
        for (start, old_end, delta) in [(5000, 5000, 7), (100, 140, -40), (8990, 8995, 3)] {
            let map = |r: &Range<usize>| {
                let f = |o: usize| {
                    if o >= old_end {
                        o.wrapping_add_signed(delta)
                    } else {
                        o.min(start)
                    }
                };
                f(r.start)..f(r.end)
            };
            chunked.rebase(start, old_end, delta, map, |r| !r.0.is_empty());
            items = naive_rebase(&items, start, old_end, delta);
            assert_eq!(chunked.iter().collect::<Vec<_>>(), items);
        }
    }

    #[test]
    fn iter_from_and_back_start_near_offset() {
        let items: Vec<R> = (0..2000).map(|i| R(i..i + 1)).collect();
        let chunked = Chunked::from_vec(items);
        let first = chunked.iter_from(1500).next().unwrap();
        assert!(first.0.start <= 1500 && first.0.start > 1500 - CHUNK);
        assert_eq!(
            chunked.iter_from(1500).find(|r| r.0.start == 1500),
            Some(R(1500..1501))
        );
        assert_eq!(
            chunked.iter_back_from(1500).find(|r| r.0.start <= 1500),
            Some(R(1500..1501))
        );
        assert_eq!(chunked.len(), 2000);
    }

    #[test]
    fn edit_region_replaces_items() {
        let items: Vec<R> = (0..2000).map(|i| R(i..i + 1)).collect();
        let mut chunked = Chunked::from_vec(items);
        chunked.edit_region(600..610, |v| {
            v.retain(|r| !(600..610).contains(&r.0.start));
            let at = v.partition_point(|r| r.0.start < 600);
            v.insert(at, R(600..610));
        });
        let all: Vec<_> = chunked.iter().collect();
        assert_eq!(all.len(), 1991);
        assert_eq!(all[600], R(600..610));
        assert_eq!(all[601], R(610..611));
    }
}
