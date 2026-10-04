//! The folder walk a search uses. The sidebar never reads ahead of an
//! expanded directory; this does, and it has to stop for a symlink cycle.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use inkmark_files::walk_notes;

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

#[test]
fn walk_lists_markdown_notes_and_skips_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("file10.md"), "a\n");
    write(&dir.path().join("file2.md"), "b\n");
    write(&dir.path().join("notes/nested.md"), "c\n");
    write(&dir.path().join("picture.png"), "nope\n");
    write(&dir.path().join("plain.txt"), "nope\n");
    write(&dir.path().join(".hidden.md"), "dot\n");
    write(&dir.path().join(".secret/inside.md"), "dot\n");

    let notes = walk_notes(dir.path(), false, None).unwrap();
    let relative: Vec<_> = notes.iter().map(|note| note.relative.as_str()).collect();
    assert_eq!(relative, vec!["file2.md", "file10.md", "notes/nested.md"]);

    let all = walk_notes(dir.path(), true, None).unwrap();
    let relative: Vec<_> = all.iter().map(|note| note.relative.as_str()).collect();
    assert_eq!(
        relative,
        vec![
            ".hidden.md",
            ".secret/inside.md",
            "file2.md",
            "file10.md",
            "notes/nested.md",
        ]
    );
}

#[test]
fn a_symlink_cycle_is_walked_once() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("sub/note.md"), "x\n");
    symlink(dir.path().join("sub"), dir.path().join("sub/again")).unwrap();
    symlink(dir.path(), dir.path().join("up")).unwrap();

    let notes = walk_notes(dir.path(), false, None).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].relative, "sub/note.md");
    assert_eq!(notes[0].name, "note.md");
}

#[test]
fn an_unreadable_root_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o0);
    fs::set_permissions(path, perms).unwrap();
    let result = walk_notes(path, false, None);
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn an_unreadable_subdirectory_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("note.md"), "a\n");
    let locked = dir.path().join("locked");
    write(&locked.join("secret.md"), "b\n");
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o0);
    fs::set_permissions(&locked, perms).unwrap();
    let result = walk_notes(dir.path(), false, None);
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&locked, perms).unwrap();
    let notes = result.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].relative, "note.md");
}
