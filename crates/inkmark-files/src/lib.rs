//! Folder tree for the file browser: what the command line opens, a lazy
//! listing of one folder, and watches on the folders that are expanded.
//! No egui here, so it can be tested on temp directories.

mod list;
mod new_file;
mod root;
mod sort;
mod tree;
mod watch;

pub use list::{Entry, Kind, list_dir};
pub use new_file::{NewFileError, create_new_file};
pub use root::{Launch, choose_root};
pub use tree::{Pending, Row, Tree};
pub use watch::Watch;
