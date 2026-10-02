use std::borrow::Cow;
use std::fmt;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ropey::Rope;

use crate::edit::{Change, Edit, LineChange, Selection};
use crate::file::{self, DiskStamp, DiskStatus, Encoding, OpenError};
use crate::history::{EditKind, History, Transaction};
use crate::log::EditLog;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    OutOfBounds { range: Range<usize>, len: usize },
    NotCharBoundary { offset: usize },
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds { range, len } => {
                write!(f, "edit range {range:?} outside document of {len} bytes")
            }
            Self::NotCharBoundary { offset } => {
                write!(f, "edit offset {offset} is not on a char boundary")
            }
        }
    }
}

impl std::error::Error for EditError {}

/// The source of truth: a rope addressed by UTF-8 byte offsets, with one
/// undo history and an epoch that bumps on every change.
pub struct Document {
    rope: Rope,
    epoch: u64,
    log: EditLog,
    history: History,
    encoding: Encoding,
    path: Option<PathBuf>,
    disk_stamp: Option<DiskStamp>,
}

impl Default for Document {
    fn default() -> Self {
        Self::from_text("")
    }
}

impl Document {
    /// A new, unsaved document. `text` must already use `\n` line endings.
    pub fn from_text(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            epoch: 0,
            log: EditLog::default(),
            history: History::default(),
            encoding: Encoding::default(),
            path: None,
            disk_stamp: None,
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let path = path.as_ref();
        let stamp = DiskStamp::of(path)?;
        let (text, encoding) = file::decode(std::fs::read(path)?)?;
        Ok(Self {
            encoding,
            path: Some(path.to_path_buf()),
            disk_stamp: Some(stamp),
            ..Self::from_text(&text)
        })
    }

    /// Saves to the current path. Errors with `NotFound` if there isn't one.
    pub fn save(&mut self) -> io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "document has no path"))?;
        self.save_as(path)
    }

    pub fn save_as(&mut self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        let stamp = file::write_atomic(&path, |out| file::encode(&self.rope, self.encoding, out))?;
        self.path = Some(path);
        self.disk_stamp = Some(stamp);
        self.history.mark_saved();
        Ok(())
    }

    /// Sets where the next `save` writes, for a file that doesn't exist yet.
    pub fn set_path(&mut self, path: impl Into<PathBuf>) {
        self.path = Some(path.into());
        self.disk_stamp = None;
    }

    /// Accepts the file's current on-disk state as seen ("keep my version"),
    /// so `disk_status` reports `Unchanged` until it changes again.
    pub fn acknowledge_disk_state(&mut self) -> io::Result<()> {
        if let Some(path) = &self.path {
            self.disk_stamp = match DiskStamp::of(path) {
                Ok(stamp) => Some(stamp),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
        }
        Ok(())
    }

    /// Whether the file changed on disk since we opened or saved it.
    pub fn disk_status(&self) -> io::Result<DiskStatus> {
        match (&self.path, self.disk_stamp) {
            (Some(path), Some(stamp)) => file::disk_status(path, stamp),
            _ => Ok(DiskStatus::Unchanged),
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn log(&self) -> &EditLog {
        &self.log
    }

    pub fn log_mut(&mut self) -> &mut EditLog {
        &mut self.log
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn is_dirty(&self) -> bool {
        self.history.is_dirty()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Text in a byte range. Borrowed when it sits in one rope chunk.
    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        self.rope.byte_slice(range).into()
    }

    /// Number of lines; a trailing `\n` starts one more (empty) line.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn byte_to_line(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset)
    }

    pub fn line_to_byte(&self, line: usize) -> usize {
        self.rope.line_to_byte(line)
    }

    /// Byte range of `line`, excluding its `\n`.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        let start = self.rope.line_to_byte(line);
        let end = if line + 1 < self.line_count() {
            self.rope.line_to_byte(line + 1) - 1
        } else {
            self.len()
        };
        start..end
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        offset <= self.len() && self.rope.char_to_byte(self.rope.byte_to_char(offset)) == offset
    }

    /// The char boundary before `offset` (0 stays 0).
    pub fn prev_char_boundary(&self, offset: usize) -> usize {
        let c = self.rope.byte_to_char(offset);
        self.rope.char_to_byte(c.saturating_sub(1))
    }

    /// The char boundary after `offset` (end stays end).
    pub fn next_char_boundary(&self, offset: usize) -> usize {
        let c = self.rope.byte_to_char(offset);
        self.rope.char_to_byte((c + 1).min(self.rope.len_chars()))
    }

    /// Applies `edits` in order (each one's offsets are against the document
    /// as left by the previous) as a single undo step. All or nothing.
    pub fn apply(
        &mut self,
        edits: Vec<Edit>,
        selection_before: Selection,
        selection_after: Selection,
        kind: EditKind,
    ) -> Result<(), EditError> {
        self.apply_at(
            edits,
            selection_before,
            selection_after,
            kind,
            Instant::now(),
        )
    }

    /// [`apply`](Self::apply) with an explicit clock, for undo-grouping tests.
    pub fn apply_at(
        &mut self,
        edits: Vec<Edit>,
        selection_before: Selection,
        selection_after: Selection,
        kind: EditKind,
        now: Instant,
    ) -> Result<(), EditError> {
        let inverses = self.apply_all(&edits)?;
        if !edits.is_empty() {
            self.history.record(
                edits,
                inverses,
                selection_before,
                selection_after,
                kind,
                now,
            );
        }
        Ok(())
    }

    /// Ends the current typing run, e.g. when the caret is moved by hand.
    pub fn seal_undo_step(&mut self) {
        self.history.seal();
    }

    /// Undoes one step and returns the selection to restore.
    pub fn undo(&mut self) -> Option<Selection> {
        let t = self.history.pop_undo()?;
        let inverses: Vec<Edit> = t.inverses.iter().rev().cloned().collect();
        self.apply_all(&inverses)
            .expect("inverse edits are valid by construction");
        let selection = t.selection_before;
        self.history.push_undone(t);
        Some(selection)
    }

    /// Redoes one step and returns the selection to restore.
    pub fn redo(&mut self) -> Option<Selection> {
        let t: Transaction = self.history.pop_redo()?;
        self.apply_all(&t.edits)
            .expect("redo edits are valid by construction");
        let selection = t.selection_after;
        self.history.push_done(t);
        Some(selection)
    }

    /// Applies edits in order, rolling back on the first invalid one.
    /// Returns the inverse of each edit, in application order.
    fn apply_all(&mut self, edits: &[Edit]) -> Result<Vec<Edit>, EditError> {
        let mut inverses = Vec::with_capacity(edits.len());
        for edit in edits {
            match self.apply_one(edit) {
                Ok(inverse) => inverses.push(inverse),
                Err(e) => {
                    for inverse in inverses.iter().rev() {
                        self.apply_one(inverse)
                            .expect("rollback of a just-applied edit");
                    }
                    return Err(e);
                }
            }
        }
        Ok(inverses)
    }

    fn apply_one(&mut self, edit: &Edit) -> Result<Edit, EditError> {
        let Range { start, end } = edit.range.clone();
        if start > end || end > self.len() {
            return Err(EditError::OutOfBounds {
                range: edit.range.clone(),
                len: self.len(),
            });
        }
        for offset in [start, end] {
            if !self.is_char_boundary(offset) {
                return Err(EditError::NotCharBoundary { offset });
            }
        }
        let removed = String::from(self.rope.byte_slice(start..end));
        let lines = LineChange {
            start: self.rope.byte_to_line(start),
            removed: removed.matches('\n').count(),
            inserted: edit.insert.matches('\n').count(),
        };
        let char_start = self.rope.byte_to_char(start);
        if start < end {
            self.rope.remove(char_start..self.rope.byte_to_char(end));
        }
        if !edit.insert.is_empty() {
            self.rope.insert(char_start, &edit.insert);
        }
        let new_end = start + edit.insert.len();
        self.epoch += 1;
        self.log.push(
            self.epoch,
            Change {
                start,
                old_end: end,
                new_end,
                lines,
            },
        );
        Ok(Edit::replace(start..new_end, removed))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn caret(at: usize) -> Selection {
        Selection::caret(at)
    }

    fn type_text(doc: &mut Document, at: usize, text: &str, now: Instant) {
        let mut pos = at;
        for (i, ch) in text.char_indices() {
            let t = now + Duration::from_millis(50 * i as u64);
            let next = pos + ch.len_utf8();
            doc.apply_at(
                vec![Edit::insert(pos, ch)],
                caret(pos),
                caret(next),
                EditKind::Typing,
                t,
            )
            .unwrap();
            pos = next;
        }
    }

    #[test]
    fn edits_use_byte_offsets_across_multibyte_text() {
        let mut doc = Document::from_text("héllo 日本");
        let at = "héllo ".len();
        doc.apply(
            vec![Edit::replace(at..at + "日".len(), "中")],
            caret(at),
            caret(at),
            EditKind::Other,
        )
        .unwrap();
        assert_eq!(doc.slice(0..doc.len()), "héllo 中本");
        assert_eq!(doc.epoch(), 1);
    }

    #[test]
    fn rejects_mid_char_and_out_of_bounds_and_rolls_back() {
        let mut doc = Document::from_text("é!");
        let err = doc.apply(
            vec![Edit::insert(1, "x")],
            caret(0),
            caret(0),
            EditKind::Other,
        );
        assert_eq!(err, Err(EditError::NotCharBoundary { offset: 1 }));
        let err = doc.apply(
            vec![Edit::insert(0, "ok"), Edit::delete(2..99)],
            caret(0),
            caret(0),
            EditKind::Other,
        );
        assert!(matches!(err, Err(EditError::OutOfBounds { .. })));
        assert_eq!(doc.slice(0..doc.len()), "é!");
        assert!(!doc.can_undo());
    }

    #[test]
    fn typing_coalesces_until_pause_newline_or_jump() {
        let t0 = Instant::now();
        let mut doc = Document::default();
        type_text(&mut doc, 0, "abc", t0);
        // Pause > 300 ms: new step.
        type_text(&mut doc, 3, "de", t0 + Duration::from_secs(2));
        // Newline: its own step, and it ends the run.
        type_text(&mut doc, 5, "\nf", t0 + Duration::from_millis(2100));
        assert_eq!(doc.slice(0..doc.len()), "abcde\nf");

        assert_eq!(doc.undo(), Some(caret(6)));
        assert_eq!(doc.slice(0..doc.len()), "abcde\n");
        assert_eq!(doc.undo(), Some(caret(5)));
        assert_eq!(doc.slice(0..doc.len()), "abcde");
        assert_eq!(doc.undo(), Some(caret(3)));
        assert_eq!(doc.undo(), Some(caret(0)));
        assert_eq!(doc.slice(0..doc.len()), "");
        assert_eq!(doc.undo(), None);
    }

    #[test]
    fn caret_jump_starts_a_new_step() {
        let t0 = Instant::now();
        let mut doc = Document::default();
        type_text(&mut doc, 0, "ab", t0);
        // Typing again at 0 (caret moved) within the idle window.
        doc.apply_at(
            vec![Edit::insert(0, "x")],
            caret(0),
            caret(1),
            EditKind::Typing,
            t0 + Duration::from_millis(100),
        )
        .unwrap();
        doc.undo();
        assert_eq!(doc.slice(0..doc.len()), "ab");
    }

    #[test]
    fn undo_and_redo_restore_selection() {
        let mut doc = Document::from_text("hello world");
        let before = Selection {
            anchor: 6,
            head: 11,
        };
        doc.apply(
            vec![Edit::replace(6..11, "there")],
            before,
            caret(11),
            EditKind::Other,
        )
        .unwrap();
        assert_eq!(doc.undo(), Some(before));
        assert_eq!(doc.slice(0..doc.len()), "hello world");
        assert_eq!(doc.redo(), Some(caret(11)));
        assert_eq!(doc.slice(0..doc.len()), "hello there");
        // Undo and redo are changes too: views must see them.
        assert_eq!(doc.epoch(), 3);
    }

    #[test]
    fn multi_edit_steps_undo_in_reverse() {
        let mut doc = Document::from_text("a b");
        // Wrap "b" in ** **: offsets of the second edit see the first.
        doc.apply(
            vec![Edit::insert(2, "**"), Edit::insert(5, "**")],
            caret(2),
            caret(7),
            EditKind::Other,
        )
        .unwrap();
        assert_eq!(doc.slice(0..doc.len()), "a **b**");
        doc.undo();
        assert_eq!(doc.slice(0..doc.len()), "a b");
    }

    #[test]
    fn dirty_tracks_the_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        let mut doc = Document::default();
        assert!(!doc.is_dirty());
        let t0 = Instant::now();
        type_text(&mut doc, 0, "ab", t0);
        assert!(doc.is_dirty());
        doc.save_as(&path).unwrap();
        assert!(!doc.is_dirty());

        // Typing right after a save doesn't merge into the saved step.
        type_text(&mut doc, 2, "c", t0 + Duration::from_millis(100));
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty());
        doc.undo();
        assert!(doc.is_dirty());
        doc.redo();
        assert!(!doc.is_dirty());

        // Undo past the save, then branch: the saved state is gone for good.
        doc.undo();
        type_text(&mut doc, 0, "z", t0 + Duration::from_secs(5));
        assert!(doc.is_dirty());
        doc.undo();
        assert!(doc.is_dirty());
    }

    #[test]
    fn open_save_round_trip_preserves_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.md");
        std::fs::write(&path, b"\xEF\xBB\xBF# Title\r\n\r\nBody\r\n").unwrap();

        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.slice(0..doc.len()), "# Title\n\nBody\n");
        assert_eq!(doc.line_count(), 4);
        assert_eq!(doc.line_range(2), 9..13);
        let end = doc.len();
        doc.apply(
            vec![Edit::insert(end, "More\n")],
            caret(end),
            caret(end + 5),
            EditKind::Other,
        )
        .unwrap();
        doc.save().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xEF\xBB\xBF# Title\r\n\r\nBody\r\nMore\r\n"
        );
        assert_eq!(doc.disk_status().unwrap(), DiskStatus::Unchanged);
    }

    #[test]
    fn changes_report_line_deltas() {
        let mut doc = Document::from_text("one\ntwo\nthree\n");
        let before = doc.epoch();
        // Replace "o\ntwo\nth" (spanning lines 0-2) with "X\nY".
        doc.apply(
            vec![Edit::replace(2..10, "X\nY")],
            caret(2),
            caret(5),
            EditKind::Other,
        )
        .unwrap();
        let change = *doc.log().changes_since(before).unwrap().next().unwrap();
        assert_eq!(
            change.lines,
            crate::edit::LineChange {
                start: 0,
                removed: 2,
                inserted: 1
            }
        );
        assert_eq!(doc.slice(0..doc.len()), "onX\nYree\n");
        assert_eq!(doc.line_count(), 4 - 2 + 1);
    }

    #[test]
    fn edit_log_maps_old_offsets_forward() {
        let mut doc = Document::from_text("# A\n\nPara\n");
        let parsed_at = doc.epoch();
        doc.apply(
            vec![Edit::insert(0, "Intro\n\n")],
            caret(0),
            caret(7),
            EditKind::Other,
        )
        .unwrap();
        let para = 5;
        let mapped = doc
            .log()
            .map_since(parsed_at, para, crate::edit::Bias::Left)
            .unwrap();
        assert_eq!(&doc.slice(mapped..mapped + 4), "Para");
    }
}
