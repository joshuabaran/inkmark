//! Rename and move on temp trees: the new paths, and every refusal.

use std::fs;

use inkmark_files::{OpError, move_into, rename};

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("notes/deep")).unwrap();
    fs::create_dir(dir.path().join("archive")).unwrap();
    fs::write(dir.path().join("notes/a.md"), "a").unwrap();
    fs::write(dir.path().join("notes/b.md"), "b").unwrap();
    fs::write(dir.path().join("notes/deep/c.md"), "c").unwrap();
    fs::write(dir.path().join("todo.txt"), "t").unwrap();
    dir
}

#[test]
fn rename_uses_the_name_exactly_as_given() {
    // Review of #24: guessing whether a name has an extension broke dotted
    // names. The prompt keeps the extension by selecting only the stem.
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    let dotted = rename(&a, "2024.01.02.md").unwrap();
    assert_eq!(dotted, dir.path().join("notes/2024.01.02.md"));
    assert_eq!(fs::read_to_string(&dotted).unwrap(), "a");
    // Unchanged: nothing to do.
    assert_eq!(rename(&dotted, "2024.01.02.md").unwrap(), dotted);
    // Without an extension, as typed.
    let todo = rename(&dir.path().join("todo.txt"), "later").unwrap();
    assert_eq!(todo, dir.path().join("later"));
}

#[test]
fn rename_never_overwrites() {
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    assert_eq!(
        rename(&a, "b.md"),
        Err(OpError::Exists(dir.path().join("notes/b.md")))
    );
    assert_eq!(fs::read_to_string(&a).unwrap(), "a");
    assert_eq!(
        fs::read_to_string(dir.path().join("notes/b.md")).unwrap(),
        "b"
    );
    // A folder's name is taken too, even by a file.
    fs::write(dir.path().join("taken"), "").unwrap();
    assert_eq!(
        rename(&dir.path().join("notes"), "taken"),
        Err(OpError::Exists(dir.path().join("taken")))
    );
    // Only the folder `deep` exists, so `deep.md` is free.
    assert_eq!(rename(&a, "deep.md").map(|p| p.exists()), Ok(true));
    fs::create_dir(dir.path().join("notes/folder.md")).unwrap();
    assert_eq!(
        rename(&dir.path().join("notes/b.md"), "folder.md"),
        Err(OpError::Exists(dir.path().join("notes/folder.md")))
    );
}

#[test]
fn rename_refuses_paths_and_empty_names() {
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    for bad in ["", "  ", "x/y", "..", "."] {
        assert_eq!(rename(&a, bad), Err(OpError::Invalid), "{bad:?}");
    }
    assert!(a.exists());
    assert_eq!(
        rename(&dir.path().join("notes/gone.md"), "x"),
        Err(OpError::NotFound)
    );
}

#[test]
fn folders_rename_and_move_with_their_contents() {
    let dir = tree();
    let notes = rename(&dir.path().join("notes"), "journal").unwrap();
    assert_eq!(notes, dir.path().join("journal"));
    assert!(notes.join("deep/c.md").exists());
    let moved = move_into(&notes, &dir.path().join("archive")).unwrap();
    assert_eq!(moved, dir.path().join("archive/journal"));
    assert!(moved.join("a.md").exists());
}

#[test]
fn moving_a_file_into_a_folder() {
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    let moved = move_into(&a, &dir.path().join("archive")).unwrap();
    assert_eq!(moved, dir.path().join("archive/a.md"));
    assert_eq!(fs::read_to_string(moved).unwrap(), "a");
    // Into the folder it's already in: nothing happens.
    let b = dir.path().join("notes/b.md");
    assert_eq!(move_into(&b, &dir.path().join("notes")).unwrap(), b);
    // Review of #24: the same folder spelled differently, as a folder
    // picker may return it, is still "already there".
    assert_eq!(move_into(&b, &dir.path().join("notes/.")).unwrap(), b);
    std::os::unix::fs::symlink(dir.path().join("notes"), dir.path().join("link")).unwrap();
    assert_eq!(move_into(&b, &dir.path().join("link")).unwrap(), b);
    assert!(b.exists());
}

#[test]
fn moves_never_overwrite_or_nest_a_folder_in_itself() {
    let dir = tree();
    fs::write(dir.path().join("archive/a.md"), "other").unwrap();
    assert_eq!(
        move_into(&dir.path().join("notes/a.md"), &dir.path().join("archive")),
        Err(OpError::Exists(dir.path().join("archive/a.md")))
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("archive/a.md")).unwrap(),
        "other"
    );
    let notes = dir.path().join("notes");
    assert_eq!(
        move_into(&notes, &notes),
        Ok(notes.clone())
            .and(Err(OpError::IntoItself))
            .or(move_into(&notes, &notes))
    );
    assert_eq!(
        move_into(&notes, &notes.join("deep")),
        Err(OpError::IntoItself)
    );
    assert!(notes.join("deep/c.md").exists());
}
