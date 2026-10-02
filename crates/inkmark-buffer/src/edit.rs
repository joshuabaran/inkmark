use std::ops::Range;

/// Replace the bytes in `range` with `insert`. Offsets are UTF-8 bytes and
/// must fall on char boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub insert: String,
}

impl Edit {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: at..at,
            insert: text.into(),
        }
    }

    pub fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            insert: String::new(),
        }
    }

    pub fn replace(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            insert: text.into(),
        }
    }
}

/// Caret and selection as byte offsets. `anchor == head` is a bare caret.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Which side an offset sticks to when text is inserted exactly at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// The shape of one applied edit: bytes `start..old_end` became `start..new_end`.
/// This is what views and parse results need to shift their own offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

impl Change {
    /// Maps an offset from before this change to after it. Offsets inside the
    /// replaced range collapse to its start (`Left`) or end (`Right`).
    pub fn map(&self, offset: usize, bias: Bias) -> usize {
        if offset < self.start || (offset == self.start && bias == Bias::Left) {
            offset
        } else if offset > self.old_end || (offset == self.old_end && bias == Bias::Right) {
            offset - self.old_end + self.new_end
        } else if bias == Bias::Left {
            self.start
        } else {
            self.new_end
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_around_a_replacement() {
        // "abcdef" → replace "cd" (2..4) with "XYZ".
        let c = Change {
            start: 2,
            old_end: 4,
            new_end: 5,
        };
        assert_eq!(c.map(1, Bias::Right), 1);
        assert_eq!(c.map(2, Bias::Left), 2);
        assert_eq!(c.map(2, Bias::Right), 5);
        assert_eq!(c.map(3, Bias::Left), 2);
        assert_eq!(c.map(3, Bias::Right), 5);
        assert_eq!(c.map(4, Bias::Left), 2);
        assert_eq!(c.map(4, Bias::Right), 5);
        assert_eq!(c.map(6, Bias::Left), 7);
    }

    #[test]
    fn map_at_a_pure_insertion() {
        let c = Change {
            start: 3,
            old_end: 3,
            new_end: 6,
        };
        assert_eq!(c.map(3, Bias::Left), 3);
        assert_eq!(c.map(3, Bias::Right), 6);
        assert_eq!(c.map(4, Bias::Left), 7);
    }
}
