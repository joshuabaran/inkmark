use std::collections::VecDeque;

use crate::edit::{Bias, Change};

/// Entries kept before the oldest are dropped. Consumers further behind than
/// this get `None` from [`EditLog::changes_since`] and must start over.
const MAX_ENTRIES: usize = 10_000;

/// Every applied change, tagged with the epoch it produced, so a parse result
/// from an older epoch can be shifted forward instead of thrown away.
#[derive(Debug, Default)]
pub struct EditLog {
    entries: VecDeque<(u64, Change)>,
    /// Epoch the log starts from: changes after it are all present.
    base_epoch: u64,
}

impl EditLog {
    pub(crate) fn push(&mut self, epoch: u64, change: Change) {
        if self.entries.len() == MAX_ENTRIES
            && let Some((dropped, _)) = self.entries.pop_front()
        {
            self.base_epoch = dropped;
        }
        self.entries.push_back((epoch, change));
    }

    /// Changes that turn the document at `epoch` into the current one, oldest
    /// first, or `None` if some of them were already dropped.
    pub fn changes_since(&self, epoch: u64) -> Option<impl Iterator<Item = &Change>> {
        if epoch < self.base_epoch {
            return None;
        }
        let first = self.entries.partition_point(|&(e, _)| e <= epoch);
        Some(self.entries.range(first..).map(|(_, c)| c))
    }

    /// Maps an offset in the document at `epoch` to the current document.
    pub fn map_since(&self, epoch: u64, offset: usize, bias: Bias) -> Option<usize> {
        Some(
            self.changes_since(epoch)?
                .fold(offset, |o, c| c.map(o, bias)),
        )
    }

    /// Drops changes up to and including `epoch` once nobody needs them.
    pub fn discard_through(&mut self, epoch: u64) {
        while self.entries.front().is_some_and(|&(e, _)| e <= epoch) {
            self.entries.pop_front();
        }
        self.base_epoch = self.base_epoch.max(epoch);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(at: usize, len: usize) -> Change {
        Change {
            start: at,
            old_end: at,
            new_end: at + len,
        }
    }

    #[test]
    fn maps_through_changes_after_epoch() {
        let mut log = EditLog::default();
        log.push(1, ins(0, 5));
        log.push(2, ins(10, 3));
        log.push(3, ins(0, 1));
        // An offset of 12 at epoch 1 → +3 (insert at 10) → +1 (insert at 0).
        assert_eq!(log.map_since(1, 12, Bias::Left), Some(16));
        assert_eq!(log.map_since(3, 12, Bias::Left), Some(12));
        assert_eq!(log.map_since(0, 0, Bias::Right), Some(6));
    }

    #[test]
    fn discarded_history_is_reported() {
        let mut log = EditLog::default();
        log.push(1, ins(0, 1));
        log.push(2, ins(0, 1));
        log.discard_through(1);
        assert!(log.changes_since(0).is_none());
        assert_eq!(log.changes_since(1).unwrap().count(), 1);
    }

    #[test]
    fn overflow_drops_oldest() {
        let mut log = EditLog::default();
        for epoch in 1..=(MAX_ENTRIES as u64 + 5) {
            log.push(epoch, ins(0, 1));
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        assert!(log.changes_since(4).is_none());
        assert_eq!(log.changes_since(5).unwrap().count(), MAX_ENTRIES);
    }
}
