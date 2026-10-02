//! External create, rename and delete show up in a watched tree. The wait
//! polls until a deadline; it does not sleep for a fixed time and hope.

use std::fs;
use std::time::{Duration, Instant};

use inkmark_files::{NewFileError, Tree, Watch, create_new_file};

fn wait_until(mut pred: impl FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if pred() {
            return;
        }
        if start.elapsed() > Duration::from_secs(1) {
            panic!("watched tree did not update within 1s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn reload(tree: &mut Tree, watch: &mut Watch) -> bool {
    let changed = watch.changed();
    if changed.is_empty() {
        return false;
    }
    for path in changed {
        let dirs = tree.dirs_at(&path);
        for index in dirs {
            tree.invalidate(index);
        }
    }
    tree.load_pending();
    true
}

#[test]
fn a_watched_folder_picks_up_create_rename_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let mut watch = Watch::new(|| {}).unwrap();
    watch.sync(&tree.expanded_dirs());

    let created = dir.path().join("a.md");
    fs::write(&created, "a\n").unwrap();
    wait_until(|| {
        reload(&mut tree, &mut watch);
        tree.rows().iter().any(|row| row.name == "a.md")
    });

    let renamed = dir.path().join("b.md");
    fs::rename(&created, &renamed).unwrap();
    wait_until(|| {
        reload(&mut tree, &mut watch);
        let names = tree.rows();
        names.iter().any(|row| row.name == "b.md") && !names.iter().any(|row| row.name == "a.md")
    });

    fs::remove_file(&renamed).unwrap();
    wait_until(|| {
        reload(&mut tree, &mut watch);
        !tree.rows().iter().any(|row| row.name == "b.md")
    });
}

#[test]
fn new_file_appends_md_refuses_existing_names_and_writes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = create_new_file(dir.path(), "notes").unwrap();
    assert_eq!(path, dir.path().join("notes.md"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "");

    let kept = create_new_file(dir.path(), "guide.md").unwrap();
    assert_eq!(kept, dir.path().join("guide.md"));
    let upper = create_new_file(dir.path(), "README.MD").unwrap();
    assert_eq!(upper, dir.path().join("README.MD"));

    assert_eq!(
        create_new_file(dir.path(), "notes"),
        Err(NewFileError::Exists(dir.path().join("notes.md")))
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    assert_eq!(create_new_file(dir.path(), "  "), Err(NewFileError::Empty));
    assert_eq!(
        create_new_file(dir.path(), "nested/nope"),
        Err(NewFileError::Invalid)
    );
}
