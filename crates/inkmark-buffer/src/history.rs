use std::time::{Duration, Instant};

use crate::edit::{Edit, Selection};

/// Typing pauses longer than this start a new undo step.
const COALESCE_IDLE: Duration = Duration::from_millis(300);

/// How an edit was made, which decides whether it merges into the previous undo step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// Inserting typed text.
    Typing,
    /// Backspace / Delete of single characters.
    Deleting,
    /// Anything else (paste, formatting command, smart edit): always its own step.
    Other,
}

/// One undo step: edits applied in order, with their inverses for undo.
#[derive(Debug)]
pub(crate) struct Transaction {
    pub(crate) id: u64,
    pub(crate) edits: Vec<Edit>,
    /// Inverses in application order; undo applies them reversed.
    pub(crate) inverses: Vec<Edit>,
    pub(crate) selection_before: Selection,
    pub(crate) selection_after: Selection,
    kind: EditKind,
    last_edit_at: Instant,
    sealed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct History {
    done: Vec<Transaction>,
    undone: Vec<Transaction>,
    next_id: u64,
    /// Id of the top of `done` when the file was last saved (`None`: empty stack).
    saved_id: Option<u64>,
    /// `true` once the saved state has been undone past and then overwritten.
    saved_unreachable: bool,
}

impl History {
    /// Records applied edits, merging into the previous step if this is
    /// continued typing or deleting at the same caret.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        edits: Vec<Edit>,
        inverses: Vec<Edit>,
        selection_before: Selection,
        selection_after: Selection,
        kind: EditKind,
        now: Instant,
    ) {
        if !self.undone.is_empty() {
            if self.undone.iter().any(|t| Some(t.id) == self.saved_id) {
                self.saved_unreachable = true;
            }
            self.undone.clear();
        }
        let has_newline = edits.iter().any(|e| e.insert.contains('\n'));
        if let Some(top) = self.done.last_mut()
            && !top.sealed
            && kind != EditKind::Other
            && top.kind == kind
            && top.selection_after == selection_before
            && now.duration_since(top.last_edit_at) <= COALESCE_IDLE
            && !has_newline
        {
            top.edits.extend(edits);
            top.inverses.extend(inverses);
            top.selection_after = selection_after;
            top.last_edit_at = now;
            return;
        }
        self.next_id += 1;
        self.done.push(Transaction {
            id: self.next_id,
            edits,
            inverses,
            selection_before,
            selection_after,
            kind,
            last_edit_at: now,
            // A newline ends the current typing run; other edits stand alone.
            sealed: kind == EditKind::Other || has_newline,
        });
    }

    /// Stops the current step from absorbing further edits.
    pub(crate) fn seal(&mut self) {
        if let Some(top) = self.done.last_mut() {
            top.sealed = true;
        }
    }

    pub(crate) fn pop_undo(&mut self) -> Option<Transaction> {
        let mut t = self.done.pop()?;
        t.sealed = true;
        Some(t)
    }

    pub(crate) fn push_undone(&mut self, t: Transaction) {
        self.undone.push(t);
    }

    pub(crate) fn pop_redo(&mut self) -> Option<Transaction> {
        self.undone.pop()
    }

    pub(crate) fn push_done(&mut self, t: Transaction) {
        self.done.push(t);
    }

    pub(crate) fn mark_saved(&mut self) {
        self.seal();
        self.saved_id = self.done.last().map(|t| t.id);
        self.saved_unreachable = false;
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.saved_unreachable || self.done.last().map(|t| t.id) != self.saved_id
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }
}
