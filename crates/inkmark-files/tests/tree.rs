//! Temp-directory coverage for sorting, filtering, lazy expand, symlinks,
//! unreadable folders, and command-line root selection.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use inkmark_files::{Entry, Kind, Launch, Tree, choose_root, list_dir};

fn names(tree: &mut Tree) -> Vec<String> {
    tree.rows()
        .iter()
        .map(|row| format!("{}{}", "  ".repeat(row.depth as usize), row.name))
        .collect()
}

fn row(tree: &mut Tree, name: &str) -> inkmark_files::Row {
    tree.rows()
        .iter()
        .find(|row| row.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no row {name}"))
}

fn touch(path: &Path) {
    fs::write(path, "").unwrap();
}

#[test]
fn folders_sort_first_and_names_sort_naturally_ignoring_case() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["file10.md", "file2.md", "File2.md", "zed.md", "b.md"] {
        touch(&dir.path().join(name));
    }
    fs::create_dir(dir.path().join("dir10")).unwrap();
    fs::create_dir(dir.path().join("dir2")).unwrap();
    touch(&dir.path().join("dir2").join("nested.md"));

    let listed: Vec<_> = list_dir(dir.path(), false, None)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        listed,
        vec![
            "dir2".to_string(),
            "dir10".to_string(),
            "b.md".to_string(),
            "File2.md".to_string(),
            "file2.md".to_string(),
            "file10.md".to_string(),
            "zed.md".to_string(),
        ]
    );

    // The nested file is not part of the root listing.
    assert!(!listed.iter().any(|name| name == "nested.md"));

    let mut tree = Tree::new(dir.path());
    assert!(tree.rows().is_empty(), "root children are not read yet");
    assert_eq!(tree.pending().len(), 1);
    tree.load_pending();
    assert_eq!(
        names(&mut tree),
        vec![
            "dir2",
            "dir10",
            "b.md",
            "File2.md",
            "file2.md",
            "file10.md",
            "zed.md"
        ]
    );
    // Still lazy: dir2's child stays unread until dir2 is expanded.
    assert!(!names(&mut tree).iter().any(|name| name.contains("nested")));
    let dir2 = row(&mut tree, "dir2").index;
    tree.expand(dir2);
    assert!(names(&mut tree).iter().any(|name| name == "  nested.md"));
}

#[test]
fn markdown_extensions_dotfiles_and_the_show_all_toggle() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "notes.md",
        "guide.markdown",
        "old.mdown",
        "draft.mkd",
        "README.MD",
        "notes.txt",
        "photo.png",
        ".secret.md",
    ] {
        touch(&dir.path().join(name));
    }
    fs::create_dir(dir.path().join(".hidden")).unwrap();
    touch(&dir.path().join(".hidden").join("x.md"));
    fs::create_dir(dir.path().join("empty")).unwrap();

    let visible: Vec<_> = list_dir(dir.path(), false, None)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        visible,
        vec![
            "empty".to_string(),
            "draft.mkd".to_string(),
            "guide.markdown".to_string(),
            "notes.md".to_string(),
            "old.mdown".to_string(),
            "README.MD".to_string(),
        ]
    );

    let all = list_dir(dir.path(), true, None).unwrap();
    let find = |name: &str| -> Entry {
        all.iter()
            .find(|entry| entry.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    assert!(!find("notes.txt").kind.openable());
    assert!(!find("photo.png").kind.openable());
    assert!(find(".secret.md").kind.openable());
    assert!(find(".hidden").kind.is_dir());
    assert!(find("empty").kind.is_dir());
    assert!(find("notes.md").kind.openable());

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert!(!names(&mut tree).iter().any(|name| name.contains("secret")));
    tree.set_show_all(true);
    tree.load_pending();
    let shown = names(&mut tree);
    assert!(shown.iter().any(|name| name == ".secret.md"));
    assert!(shown.iter().any(|name| name == ".hidden"));
    assert!(shown.iter().any(|name| name == "photo.png"));
}

#[test]
fn a_folder_lists_only_when_expanded_and_sees_files_added_before_that() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert_eq!(names(&mut tree), vec!["sub".to_string()]);
    touch(&sub.join("late.md"));
    assert_eq!(names(&mut tree), vec!["sub".to_string()]);
    let sub_index = row(&mut tree, "sub").index;
    tree.expand(sub_index);
    assert_eq!(
        names(&mut tree),
        vec!["sub".to_string(), "  late.md".to_string()]
    );
}

#[test]
fn symlinks_list_their_target_but_loops_and_dangling_links_do_not_recurse() {
    let dir = tempfile::tempdir().unwrap();
    let docs = dir.path().join("docs");
    fs::create_dir(&docs).unwrap();
    touch(&docs.join("a.md"));
    symlink("docs", dir.path().join("linkdir")).unwrap();
    symlink("loop", dir.path().join("loop")).unwrap();
    symlink("missing.md", dir.path().join("gone.md")).unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    touch(&sub.join("note.md"));
    // Points at the tree root: expanding it must not walk the tree again.
    symlink("..", sub.join("up")).unwrap();

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let loop_row = row(&mut tree, "loop");
    assert!(matches!(loop_row.kind, Kind::Dir { looped: true, .. }));
    tree.expand(loop_row.index);
    assert_eq!(
        names(&mut tree)
            .iter()
            .filter(|name| *name == "loop")
            .count(),
        1
    );

    let gone = row(&mut tree, "gone.md");
    assert!(matches!(gone.kind, Kind::File { openable: true }));

    let link = row(&mut tree, "linkdir").index;
    tree.expand(link);
    assert!(names(&mut tree).iter().any(|name| name == "  a.md"));

    let sub_index = row(&mut tree, "sub").index;
    tree.expand(sub_index);
    let up = row(&mut tree, "up");
    tree.expand(up.index);
    let up = row(&mut tree, "up");
    assert!(matches!(up.kind, Kind::Dir { looped: true, .. }));
    // `sub` is not listed again under `up`.
    assert_eq!(
        names(&mut tree)
            .iter()
            .filter(|name| name.ends_with("sub"))
            .count(),
        1
    );
    assert!(names(&mut tree).iter().any(|name| name == "  note.md"));
}

#[test]
fn an_unreadable_folder_is_marked_and_the_rest_of_the_tree_still_lists() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("ok.md"));
    let secret = dir.path().join("secret");
    fs::create_dir(&secret).unwrap();
    touch(&secret.join("hidden.md"));
    let _restore = ModeGuard(secret.clone());
    let mut perms = fs::metadata(&secret).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret, perms).unwrap();
    if fs::read_dir(&secret).is_ok() {
        // Running as root ignores the mode bits; the case can't be asserted.
        return;
    }

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    assert!(names(&mut tree).iter().any(|name| name == "ok.md"));
    let secret_row = row(&mut tree, "secret");
    tree.expand(secret_row.index);
    let secret_row = row(&mut tree, "secret");
    assert!(matches!(
        secret_row.kind,
        Kind::Dir {
            unreadable: true,
            ..
        }
    ));
    assert!(!names(&mut tree).iter().any(|name| name.contains("hidden")));

    let mut perms = fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o000);
    let _root_restore = ModeGuard(dir.path().to_path_buf());
    fs::set_permissions(dir.path(), perms).unwrap();
    let mut rooted = Tree::new(dir.path());
    rooted.load_pending();
    assert!(matches!(
        rooted.root_kind(),
        Kind::Dir {
            unreadable: true,
            ..
        }
    ));
    assert!(rooted.rows().is_empty());
}

#[test]
fn the_root_follows_a_file_a_folder_a_missing_path_or_no_argument() {
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path();
    fs::create_dir(cwd.join("notes")).unwrap();
    touch(&cwd.join("notes.md"));
    touch(&cwd.join("notes").join("a.md"));
    symlink("notes.md", cwd.join("link.md")).unwrap();
    symlink("notes", cwd.join("linkdir")).unwrap();

    assert_eq!(
        choose_root(None, cwd),
        Launch::Folder {
            root: cwd.to_path_buf()
        }
    );
    assert_eq!(
        choose_root(Some(Path::new("notes.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("notes.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("notes.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("notes.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("notes")), cwd),
        Launch::Folder {
            root: cwd.join("notes")
        }
    );
    let missing = cwd.join("notes").join("brand-new.md");
    assert_eq!(
        choose_root(Some(&missing), cwd),
        Launch::File {
            root: cwd.join("notes"),
            file: missing,
        }
    );
    // A link to a file browses the link's parent, not the target's.
    assert_eq!(
        choose_root(Some(&cwd.join("link.md")), cwd),
        Launch::File {
            root: cwd.to_path_buf(),
            file: cwd.join("link.md"),
        }
    );
    assert_eq!(
        choose_root(Some(&cwd.join("linkdir")), cwd),
        Launch::Folder {
            root: cwd.join("linkdir")
        }
    );
    // No argument is the current directory even when a relative file would
    // have used that same directory as its parent.
    assert!(matches!(
        choose_root(None, Path::new("/tmp")),
        Launch::Folder { root } if root == Path::new("/tmp")
    ));
}

#[test]
fn a_folder_change_keeps_a_sibling_subfolder_expanded() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let src = projects.join("src");
    fs::create_dir_all(&src).unwrap();
    touch(&src.join("main.md"));
    touch(&dir.path().join("todo.md"));

    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let projects_index = row(&mut tree, "projects").index;
    tree.expand(projects_index);
    let src_index = row(&mut tree, "src").index;
    tree.expand(src_index);
    assert!(row(&mut tree, "projects").expanded);
    assert!(row(&mut tree, "src").expanded);

    // A sibling save re-lists the parent. The expanded folders have to stay.
    fs::remove_file(dir.path().join("todo.md")).unwrap();
    touch(&dir.path().join("later.md"));
    let root = tree.dirs_at(dir.path())[0];
    tree.invalidate(root);
    tree.load_pending();

    assert!(row(&mut tree, "projects").expanded, "projects collapsed");
    assert!(row(&mut tree, "src").expanded, "src collapsed");
    let listed = names(&mut tree);
    assert!(
        listed.iter().any(|name| name == "    main.md"),
        "{listed:?}"
    );
    assert!(listed.iter().any(|name| name == "later.md"), "{listed:?}");
    assert!(
        !listed.iter().any(|name| name == "todo.md"),
        "removed file stayed: {listed:?}"
    );
}

#[test]
fn a_listing_for_a_removed_folder_does_not_land_on_the_reused_slot() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    fs::create_dir(&old).unwrap();
    touch(&old.join("a.md"));
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let index = row(&mut tree, "old").index;
    tree.set_expanded(index, true);
    let generation = tree
        .pending()
        .into_iter()
        .find(|job| job.index == index)
        .expect("old should be waiting to list")
        .generation;

    fs::remove_dir_all(&old).unwrap();
    touch(&dir.path().join("new.md"));
    let root = tree.dirs_at(dir.path())[0];
    tree.invalidate(root);
    tree.load_pending();

    let replacement = row(&mut tree, "new.md");
    assert_eq!(
        replacement.index, index,
        "the removed folder's slot was not reused"
    );
    // The worker answers with the generation it started with.
    tree.apply(
        index,
        generation,
        Ok(vec![Entry {
            name: "a.md".into(),
            path: old.join("a.md"),
            kind: Kind::File { openable: true },
        }]),
    );
    let replacement = row(&mut tree, "new.md");
    assert!(
        matches!(replacement.kind, Kind::File { .. }),
        "stale listing rewrote the new file: {:?}",
        replacement.kind
    );
    assert!(!names(&mut tree).iter().any(|name| name.contains("a.md")));
}

#[test]
fn replacing_a_folders_entries_does_not_grow_the_node_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = Tree::new(dir.path());
    for round in 0..20 {
        if round > 0 {
            for i in 0..40 {
                fs::remove_file(dir.path().join(format!("f{}-{i}.md", round - 1))).unwrap();
            }
        }
        for i in 0..40 {
            touch(&dir.path().join(format!("f{round}-{i}.md")));
        }
        let root = tree.dirs_at(dir.path())[0];
        tree.invalidate(root);
        tree.load_pending();
    }
    assert_eq!(tree.rows().len(), 40);
    let stored = tree.stored_nodes();
    assert!(
        stored <= 50,
        "stored {stored} nodes after replacing the folder 20 times"
    );
}

#[test]
fn expanding_again_reads_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    touch(&sub.join("a.md"));
    let mut tree = Tree::new(dir.path());
    tree.load_pending();
    let index = row(&mut tree, "sub").index;
    tree.expand(index);
    assert!(names(&mut tree).iter().any(|name| name == "  a.md"));

    tree.collapse(index);
    touch(&sub.join("b.md"));
    tree.expand(index);
    let listed = names(&mut tree);
    assert!(listed.iter().any(|name| name == "  a.md"));
    assert!(listed.iter().any(|name| name == "  b.md"));
}

#[test]
fn a_finished_listing_says_when_a_path_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let pics = dir.path().join("Pics");
    let mut tree = Tree::new(dir.path());
    assert!(
        !tree.settled_without(&pics),
        "the root has not been read yet"
    );

    tree.load_pending();
    assert!(tree.settled_without(&pics));
    assert!(tree.settled_without(Path::new("/tmp/inkmark-not-in-this-tree")));

    fs::create_dir(&pics).unwrap();
    let root = tree.dirs_at(dir.path())[0];
    tree.invalidate(root);
    assert!(
        !tree.settled_without(&pics),
        "a reload in flight still counts as waiting"
    );
    tree.load_pending();
    assert!(!tree.settled_without(&pics));

    let nested = pics.join("More");
    tree.reveal(&nested);
    tree.load_pending();
    assert!(tree.settled_without(&nested));
    fs::create_dir(&nested).unwrap();
    let pics_index = row(&mut tree, "Pics").index;
    tree.invalidate(pics_index);
    assert!(!tree.settled_without(&nested));
    tree.load_pending();
    assert!(!tree.settled_without(&nested));
}

struct ModeGuard(PathBuf);

impl Drop for ModeGuard {
    fn drop(&mut self) {
        let Ok(meta) = fs::metadata(&self.0) else {
            return;
        };
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&self.0, perms);
    }
}
