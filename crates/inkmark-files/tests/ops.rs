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
fn a_renamed_note_keeps_its_extension_unless_one_is_typed() {
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    let ideas = rename(&a, "ideas").unwrap();
    assert_eq!(ideas, dir.path().join("notes/ideas.md"));
    assert_eq!(fs::read_to_string(&ideas).unwrap(), "a");
    assert!(!a.exists());
    let typed = rename(&ideas, "ideas.markdown").unwrap();
    assert_eq!(typed, dir.path().join("notes/ideas.markdown"));
    // A non-Markdown file is renamed exactly as typed.
    let todo = rename(&dir.path().join("todo.txt"), "later").unwrap();
    assert_eq!(todo, dir.path().join("later"));
    // Renaming to the same name is nothing to do.
    assert_eq!(rename(&typed, "ideas.markdown").unwrap(), typed);
}

#[test]
fn rename_never_overwrites() {
    let dir = tree();
    let a = dir.path().join("notes/a.md");
    assert_eq!(
        rename(&a, "b"),
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
    // A file can't take a folder's name.
    assert_eq!(
        rename(&a, "deep.md").map(|p| p.exists()),
        Ok(true),
        "deep.md is free; only the folder `deep` exists"
    );
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
