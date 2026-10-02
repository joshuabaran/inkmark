//! Creating one new Markdown file in a folder.

use std::path::{Path, PathBuf};

use crate::list::is_markdown_name;

#[derive(Debug, PartialEq, Eq)]
pub enum NewFileError {
    Empty,
    /// The name is a path, not a single file in `dir`.
    Invalid,
    Exists(PathBuf),
    Io(String),
}

/// Writes an empty file in `dir`. A name without a Markdown extension gets
/// `.md` appended (`notes` becomes `notes.md`, `notes.md` stays). An existing
/// file is left untouched.
pub fn create_new_file(dir: &Path, name: &str) -> Result<PathBuf, NewFileError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NewFileError::Empty);
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err(NewFileError::Invalid);
    }
    let file_name = if is_markdown_name(name) {
        name.to_string()
    } else {
        format!("{name}.md")
    };
    let path = dir.join(file_name);
    if path.exists() {
        return Err(NewFileError::Exists(path));
    }
    std::fs::write(&path, "").map_err(|error| NewFileError::Io(error.to_string()))?;
    Ok(path)
}
