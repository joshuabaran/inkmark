# inkmark

A fast, local Markdown editor for Linux and Wayland. Plain `.md` files on
disk are the only source of truth: no accounts, sync or plugins.

- **Three views of one document**: split (raw Markdown left, rendered
  right), code only, or live only. Both panes are editable; one undo history.
- **Live editing as source patches**: typing in the rendered view edits the
  Markdown at that spot and nothing else. Syntax shows around the caret
  (the element you're in, block markers on your line) and hides elsewhere.
- **Fast on big files**: rope buffer, background parsing, virtualized panes;
  responsive on 5–10 MB documents.
- **GitHub Flavored Markdown**: tables edited as a grid, task lists with
  clickable checkboxes, strikethrough, footnotes, and bare URLs and emails
  linked automatically. Parsed by [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark)
  and checked against every example in the CommonMark and GFM specs.
- **Per-pane minimaps**, images (PNG, JPEG, GIF, WebP, BMP), recent files.
- **Folder sidebar**: browse a directory of notes, open one, and add a
  new Markdown file. Rename, move and delete are not in yet.

See [PLAN.md](PLAN.md) for the design, measurements and roadmap.

## Install

### Arch Linux

```sh
git clone https://github.com/joshuabaran/inkmark.git
cd inkmark/pack
makepkg -si
```

The `inkmark-git` package builds the latest commit and installs the binary,
a desktop entry (registered for Markdown files) and an icon.

### Any Linux, with Cargo

```sh
cargo build --release
install -Dm755 target/release/inkmark ~/.local/bin/inkmark
install -Dm644 pack/inkmark.desktop ~/.local/share/applications/inkmark.desktop
install -Dm644 pack/inkmark.svg ~/.local/share/icons/hicolor/scalable/apps/inkmark.svg
```

Runtime dependencies: a Wayland compositor (X11 also works), a Vulkan driver,
and `xdg-desktop-portal` for the open, save and folder dialogs (without
it, open files by passing them on the command line). Install CJK and emoji
fonts (e.g. `noto-fonts-cjk`, `noto-fonts-emoji`) to see those characters.

For machines without working Vulkan, build with `--features glow` and run
with `INKMARK_RENDERER=glow` to use OpenGL.

## Use

```sh
inkmark notes.md      # browses that file's folder; a missing path is a new file there
inkmark ~/notes       # browses the folder, with nothing open
inkmark               # browses the current directory and shows recent files
```

| Keys | Action |
|---|---|
| Ctrl+E | Cycle split → code → live |
| Ctrl+1 / Ctrl+2 | Focus the code / live pane (switching to it when only one pane shows) |
| Ctrl+M | Toggle the focused pane's minimap |
| Ctrl+O, Ctrl+S, Ctrl+Shift+S | Open, save, save as |
| Ctrl+Shift+O | Open a folder in the sidebar |
| Ctrl+Shift+E | Show or hide the sidebar |
| Ctrl+N | New Markdown file in the selected folder, or the folder you have open |
| Ctrl+R | Recent files |
| Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | Undo, redo (shared by both panes) |
| Ctrl+B, Ctrl+I, Ctrl+\` | Toggle bold, italic, code |
| Ctrl+Shift+X | Toggle strikethrough |
| Ctrl+K | Insert a link |
| Ctrl+Enter | Toggle the line's task checkbox (making it a task if needed) |
| Ctrl+Alt+0…6 | Paragraph / heading level |
| Enter (live) | Continue a list item or quote; on an empty one, leave it |
| Shift+Enter (live) | Hard line break |
| Tab / Shift+Tab | Indent / outdent list items (or selected lines in code); in a table, next / previous cell |
| Enter (live table) | Cell below; in the last row, a new row |

In the live pane, click a task's checkbox to tick it.

The sidebar sits to the left of the panes. Up moves to the parent folder,
Open Folder… picks a new root, and Refresh re-reads the folders that are
expanded. Arrow keys move through the tree, Left and Right collapse and
expand a folder, and Enter opens a Markdown file. A name without a
Markdown extension gets `.md`. Width and whether the sidebar is showing
are remembered under `$XDG_STATE_HOME/inkmark`.

inkmark keeps your line endings (LF/CRLF) and byte-order mark, saves
atomically, and never overwrites changes made by other programs without
asking: if the open file changes on disk, a banner offers **Reload** or
**Keep mine**; if it's deleted, your text stays and saving recreates it.
Opening another file or closing with unsaved changes asks first.

## Develop

```sh
cargo test --workspace                                  # unit, spec, fuzz tests
cargo test --release --workspace -- --ignored --nocapture   # benchmarks
scripts/measure.sh [big.md]                             # startup, memory, scroll fps
FUZZ_ITERS=5000 cargo test --release -p inkmark-view --test live_edit fuzzed
```

The workspace is split into `inkmark-buffer` (rope, edits, undo, file I/O),
`inkmark-parse` (parser seam, block tree, source map), `inkmark-text`
(cosmic-text layout and glyph atlas), `inkmark-view` (code and live panes,
and the folder sidebar), `inkmark-files` (folder tree, listing and
watching), `inkmark-minimap`, and the `inkmark` app. Large test documents
are not in the repo; `scripts/measure.sh` takes any big `.md` file.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option. The CommonMark and GFM spec examples
in `fixtures/` are CC-BY-SA 4.0 (see the README in each folder).
