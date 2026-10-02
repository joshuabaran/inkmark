//! Folder tree for the file browser: what the command line opens, and a
//! lazy listing of one folder. No egui here, so it can be tested on temp
//! directories. Watching lives beside this once the sidebar is up.

mod list;
mod root;
mod sort;
mod tree;

pub use list::{Entry, Kind, list_dir};
pub use root::{Launch, choose_root};
pub use tree::{Pending, Row, Tree};
