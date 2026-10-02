//! Rope-backed document: byte-offset edits, EditLog, UndoStack, epoch, file I/O.

mod document;
mod edit;
mod file;
mod history;
mod log;

pub use document::{Document, EditError};
pub use edit::{Bias, Change, Edit, Selection};
pub use file::{DiskStamp, DiskStatus, Encoding, LineEnding, OpenError};
pub use history::EditKind;
pub use log::EditLog;
