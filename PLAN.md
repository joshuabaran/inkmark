# Markdown editor — build plan

**Name (working):** `inkmark` · Rust · egui/eframe · Linux/Wayland (Omarchy/Hyprland) first · no Electron/WebView

**Status:** Signed off 2026-10-01. MVP (M1–M6), GFM (G1–G4) and the [file browser](#file-browser-next) (F1–F3) are complete as of 2026-10-02; the first outside review's 21 issues are fixed. See [Results](#results) for measurements and the [Roadmap](#roadmap) for what's planned. Changes to locked decisions require updating this doc first.

---

## Locked decisions

| Area | Decision |
|------|----------|
| Buffer | `ropey::Rope`. **All positions are UTF-8 byte offsets**; the buffer crate wraps ropey's char-indexed API so nothing else sees char indices. |
| UI | egui/eframe, `wgpu` renderer by default (`glow` kept buildable as a fallback feature). |
| Text | cosmic-text shapes, lays out and hit-tests; swash rasterizes into **our own glyph atlas** (egui `TextureHandle`s), painted as egui `Mesh`es. No egui text layout inside editor panes; no glyphon. |
| Parser | `pulldown-cmark` (with `into_offset_iter`) behind `MarkdownParser`; CommonMark only in MVP. **GFM (decided 2026-10-02):** a `GfmParser` behind the same trait, using pulldown-cmark's tables, strikethrough and task-list extensions plus an inkmark pass for autolink literals (bare URLs, `www.`, emails), which pulldown-cmark lacks. Chosen over `comrak` because live editing depends on byte-exact source positions, which pulldown-cmark's offsets give and comrak's line/column positions don't guarantee for inline content. |
| Parse strategy | Background full reparse is authoritative; synchronous **local reparse** of the edited top-level block for immediate feedback (see §2). Incremental parsing beyond that is a later optimization. |
| Live editing model | **Reveal-at-cursor** (Typora/Obsidian style). Live is the source with syntax hidden away from the caret; every live edit is a direct source byte-range patch. No AST→Markdown re-emit. |
| History | One document epoch, one undo stack shared by both panes. |
| Scope | Single-file open/save, dark theme, keyboard-usable. **Since 2026-10-02:** still one open document at a time, but browsing a folder to pick it is planned ([file browser](#file-browser-next)). |
| Perf target | Responsive on 5–10 MB CommonMark (~100k+ lines of typical prose). |
| Platform | Native on the Omarchy host (Arch, Wayland, Hyprland). No Windows/macOS for MVP. |

---

## One-page architecture

```
┌─ shell (eframe / winit / wgpu) ─────────────────────────────┐
│  AppState: mode, focused pane, per-pane minimap on/off, path│
│                                                             │
│  Document (source of truth)                                 │
│    Rope · epoch:u64 · EditLog (since last parse)            │
│    UndoStack (byte-range ops + selection) · dirty · path    │
│           │                                                 │
│           ├─▶ local reparse of edited top-level block (sync) │
│           └─▶ full reparse (debounced, background thread)    │
│  ParsePipeline                                              │
│    MarkdownParser trait → pulldown-cmark (+ GFM extensions) │
│    → BlockTree: blocks with [start,end) byte spans          │
│    → SourceMap: block/inline runs ↔ source ranges,          │
│                 plus hidden-syntax ranges                   │
│           │                                                 │
│     ┌─────┴──────┐                                          │
│     ▼            ▼                                          │
│  CodeView     LiveView                                      │
│  (raw rope)   (projection of BlockTree; syntax revealed     │
│                at caret; edits → source range patches)      │
│     │            │                                          │
│     └─ shared Document / UndoStack / epoch ─┘               │
│  TextRender: cosmic-text layout + swash glyph atlas → Mesh  │
│  HeightCache per pane (measured or estimated)               │
│  Minimap ×2 (line strips / block structure; viewport rect)  │
└─────────────────────────────────────────────────────────────┘
```

### 1. Document model

- **Edits** are `(start_byte, end_byte, insert: String)`. Each edit bumps `epoch` and is appended to the `EditLog`.
- **File I/O:**
  - Detect and preserve the line-ending style (LF or CRLF) and any UTF-8 byte order mark (BOM).
  - Refuse to open a file that isn't valid UTF-8, with a clear error. No lossy decoding.
  - Save atomically: write a temp file in the same directory, fsync it, then rename it over the original.
  - Detect changes made on disk (mtime and size) and prompt to reload or keep, never silently overwrite.
- **Undo/redo:** one stack of inverse range ops, shared by both panes. Each entry records the selection before and after, so undo puts the caret back where it was. Typing is grouped into one entry until a pause (~300ms), a newline, or a cursor jump.
- **Paint spans** (syntax colors, live styles) come from the BlockTree and SourceMap. They are never kept as a second copy of the text.
- **Very long lines:** for lines above a threshold (e.g. 64 KB), shape only the visible part. Syntax coloring may fall back to plain text.

### 2. Parse pipeline

- **Background full reparse:** after edits settle (~16–32ms debounce), copy the rope to a `String` and parse it on a worker thread. The result is tagged with the epoch it was parsed at.
- **Rebase, don't discard:** if the result is older than the current epoch, shift its spans forward through the `EditLog` entries since then and use it. Drop it and reparse only if the log is too long or an edit touched a span in a way that can't be shifted. Typing continuously never leaves the tree permanently stale.
- **Local sync reparse:** on each edit, reparse the enclosing top-level block on the UI thread (µs for typical blocks), splice it into the BlockTree, and shift later spans by the size of the edit. This is what makes live typing feel instant.
  - **Size limit:** if the enclosing block is larger than ~64 KB (e.g. a huge top-level list), only shift spans and wait for the background parse.
  - **Non-local effects** are left to the authoritative background parse: link reference definitions, opening code fences, lazy continuation lines, list tightness.
- **Seam:** `trait MarkdownParser { fn parse(&self, src: &str) -> ParseOutput; }` (BlockTree + SourceMap). Views depend only on the trait's output types.
- **SourceMap granularity:** blocks *and* inline runs. Each visible text run maps to its exact source range. Syntax (delimiters, `#`, list markers, `> ` prefixes, fence lines, link destinations) is recorded as hidden ranges. Container blocks (blockquote, list) do not own clean ranges; their per-line prefixes are hidden ranges inside the child's span.

### 3. View modes

- **Modes:** Split | Code | Live. Switching is instant; Document, epoch and caret are unchanged. Cycle modes with `Ctrl+E`.
- **Caret and selection:** always source byte offsets (plus affinity). Because syntax is revealed at the caret, the caret is always on a real source position. No snapping is needed.
- **Reveal-at-cursor rules (live):**
  - *Inline* (emphasis, strong, code span, link, image): reveal the delimiters and destination while the caret is inside or touching the span.
  - *Block markers* (heading `#`, fence lines and info string, thematic break): reveal on the line that contains the caret.
  - *Container markup* (list markers, `>` prefixes, their indentation, table pipes and padding): drawn as bullets, numbers, bars, margins and grid lines and **never revealed**. They're edited through the smart editing rules (as built; revealing them made lists jump sideways as the caret moved).
  - With multi-line selections, reveal everything the selection covers.
- **Live editing:**
  - *Typing:* inserts source characters at the caret. Nothing is auto-escaped; what you type is Markdown, as in Typora/Obsidian.
  - *Formatting commands* (`Ctrl+B`, `Ctrl+I`, `` Ctrl+` ``, `Ctrl+K` link, heading level): add or remove delimiters as source patches.
  - *Smart editing rules* (list item continue/outdent on Enter, blockquote prefix on Enter, Tab/Shift+Tab nesting in lists, Backspace at the start of a marker): explicit rules that each produce one or more source patches.
- **Raw HTML** (block and inline) shows in live as source text styled like code and edited as plain source. It is never rendered.
- **Scroll sync (split):** sync on the source block or line of the top visible line in the focused pane. The other pane scrolls so the same block's top lines up. No pixel sync.
- **Focus:** only one pane takes keyboard and IME input. `Ctrl+1` focuses code and `Ctrl+2` focuses live. Tab is never used to switch panes; it indents.
- **Soft wrap** is on in both panes.

### 4. Minimap

- **Code:** plain line strips or downsampled glyphs from the rope. The viewport box marks the visible line range.
- **Live:** a cheap structure strip. Headings are thick bars by level; other blocks are density ticks. Positions come from the HeightCache, never from laying out the whole document.
- **When to rebuild:** rebuild the sample when the BlockTree is swapped. On scroll, update only the viewport box.
- **Interaction and state:** click or drag sets the pane's scroll target. `show_minimap` is per pane and survives mode switches.

### 5. Rendering

- **Virtualization:** both panes lay out and paint only lines or blocks in the viewport, plus a small overscan.
- **HeightCache:** each pane keeps a height per line (code) or per block (live). Heights are measured once laid out and estimated before that (from text length × average glyph advance ÷ wrap width). This drives the scrollbar, scroll sync and minimap. When a block's real height replaces its estimate, adjust the scroll position so the view stays anchored to the top visible block.
- **Glyph atlas:**
  - swash rasterizes into a few egui textures, keyed by font, glyph, size and subpixel bin.
  - Each frame emits one `Mesh` per pane for the visible lines.
  - Evict least-recently-used glyphs when the atlas fills.
  - Rebuild on DPI change.
- **Perf target:** edit → visible update **< 16ms p95** on a ~5 MB CommonMark file, measured mid-document, in either pane. Opening 10 MB must not freeze the UI; the parse runs off-thread and the code pane is usable before the first parse finishes.
- **Images:** load local files only (relative to the open file, or absolute paths). No network fetches in MVP. Decode on a worker thread with a size cap; show a placeholder of the estimated size until ready.

### 6. Wayland / Hyprland (egui gaps — plan around them)

- **IME:** use winit's IME. Smoke-test fcitx5 under Hyprland in M1. Known risk: if IME is broken, compose key and dead keys still have to work. Don't claim full CJK support.
- **Clipboard:** use egui-winit's built-in clipboard (smithay-clipboard). Test pasting large text (5 MB) in M1. Add `arboard` only if that fails.
- **File dialog:** `rfd` through the xdg-desktop-portal (a runtime dependency). Passing a file path on the command line is the fallback.
- **DPI / scale:** follow winit/egui's scale factor; test at 1.0, 1.25 and 2.0. Fractional-scaling quirks get tuned, not treated as blockers.

---

## Product summary

A fast local Markdown editor. Plain `.md` files on disk are the only source of truth. No accounts, no sync, no plugins in v1.

Three view modes, toggled instantly, with the same document and cursor kept across switches:

1. **Split:** raw Markdown on the left, live document on the right.
2. **Code only.**
3. **Live only.**

Both panes are editable. An edit in the live pane is a direct, minimal patch to the Markdown source. It is never a whole-block rewrite and never a lossy HTML round-trip. There is one undo history for the document, whichever pane made the edit.

Each pane can show or hide its own minimap (VS Code style): a narrow overview you can click or drag to navigate, with the viewport highlighted. The code minimap shows the source; the live minimap shows rendered structure. Minimap state is per pane and survives mode switches.

### MVP

- Open, edit and save a single local `.md` file. Recent files optional.
- CommonMark only. The parser is swappable so GFM can be added without rewriting the editor.
- The three view modes.
- Per-pane minimap toggle.
- Live rendering with reveal-at-cursor: headings, emphasis, lists, links, images, code blocks, blockquotes, thematic breaks. Raw HTML shown as source.
- Scroll sync in split mode.
- Keyboard-usable. Dark theme only.

### Explicitly out of MVP

GFM tables, task lists, strikethrough, autolinks, math, Mermaid, wikilinks, multi-file vaults, git, AI, export, collaboration, plugins, accessibility (AccessKit) for the custom editor widgets, light theme.

*Since the MVP:* GFM is done (see [GFM milestone](#gfm-milestone-started-2026-10-02)), and browsing a folder of files is the next milestone ([file browser](#file-browser-next)). Multi-file vault features (links between notes, search) are still out.

### Future (do not design these out)

Math and diagrams. (GFM, including a live table editor that emits source patches, is done.) Multi-file features beyond the [file browser](#file-browser-next): links between notes, search.

### Stack constraints

Performance is a requirement, not a polish pass.

- Rust. No WebView or Electron; no Tauri, Milkdown or ProseMirror. No GPUI.
- Custom editor widget on a rope. Not one big `String`, and not a stock multiline widget.
- Unicode shaping and layout come from cosmic-text. Don't invent our own.
- Reference shape: Ferrite (Rust, egui, custom editor, split view, both panes editable, shared source epoch). Same idea; do not copy or fork it.

---

## Milestones (vertical slices; each one runs on the Omarchy host)

| # | Slice | Done when |
|---|--------|-----------|
| **M1** | Window + rope + code-only editor | **First:** glyph-atlas spike (cosmic-text → swash → egui Mesh) holds 60 fps scrolling 100k lines. Open/save one `.md` with the §1 file-handling rules; type, select, undo/redo with selection restored; soft wrap + HeightCache; virtualized scroll; dark UI. **Platform tests:** IME smoke test with fcitx5; 5 MB paste; DPI 1.0/1.25/2.0 |
| **M2** | Parser seam + BlockTree + SourceMap | Background reparse with epoch rebase; local sync reparse; inline-level SourceMap with hidden ranges; code-pane syntax coloring. **Headless live-mapping spike:** for every CommonMark spec example, SourceMap covers every byte exactly once, as visible text or hidden syntax |
| **M3** | Live view (navigable, read-only) + split + scroll sync | CommonMark blocks rendered; reveal-at-cursor as the caret moves; split/code/live modes; block-based sync; `Ctrl+1`/`Ctrl+2` focus |
| **M4** | Live editing + unified undo | Typing, deleting, formatting commands and smart Enter/Tab/Backspace in live all produce source patches; undo works across panes; patch-size guard passes |
| **M5** | Per-pane minimaps | Toggle, navigate, survive mode switch; stays cheap on 10 MB |
| **M6** | Hardening + Arch package path | Recent files (optional); benches meet targets; `cargo build --release`; PKGBUILD or install notes; risk burn-down |

The live view comes after the SourceMap (M2 → M3), and the M2 spike tests the mapping before any live UI is built. Minimaps come last among features, so the scroll and layout code they depend on already exists.

**Done:** M1 `812e4f9`–`81eb15e` · M2 `7edbcdf` · M3 `d75f765` · M4 `3e6b606` · live-view images `5dec137` (after M4, by choice) · M5 `a35bda7` · M6 `e769e58`–`6f9d693`.

---

## Testing

- **Buffer:** property tests check that random edit sequences give the same result on the rope and on a `String`, and that undo followed by redo returns to the same state.
- **Parse/SourceMap:** the CommonMark and GFM spec examples are fixtures. Checks:
  - spans cover each byte exactly once
  - a span rebased through the EditLog equals a fresh parse
  - after a local reparse, the next full reparse agrees on the edited block
- **Live editing:**
  - Scripted live edits must produce exactly the expected source.
  - Fuzzed live keystrokes must equal the same keystrokes applied at the mapped source offsets.
  - Patch size: a single character typed in live changes ≤ 1 inserted character plus any explicit smart-rule bytes.
- **Benches** (as built): `#[ignore]`d tests that print timings, run with `cargo test --release --workspace -- --ignored --nocapture` (full and local reparse, code and live keystroke→frame, live scrolling and random jumps, glyph-atlas spike). App-level numbers (startup, RSS, real-GPU scroll fps) come from `INKMARK_MEASURE=1` via `scripts/measure.sh`, instead of an `instrument` feature.
- **Regressions:** every bug from the outside review has a test in the suite where it lives, ported from the review's smoke tests.
- **App shell:** unit tests in `crates/inkmark/src/main.rs` (open, reload, save-as, dialogs, mode switching), with the recent-files list in a temp directory.
- **Also built:** headless egui tests that drive both panes with synthetic keyboard, mouse, clipboard and IME input; a typing fuzz (random clicks + keystrokes, each must insert exactly the typed character) and a mixed-operation fuzz (map stays valid, undo-all restores the original), both clean at 5000 iterations.

---

## Risks

| Risk | Why it hurts | Mitigation | Outcome |
|------|----------------|------------|---------|
| **Live → source mapping** | Wrong offsets corrupt the file | Reveal-at-cursor model (no re-emit); inline SourceMap with full byte coverage; spike in M2; fuzz tests | **Retired.** All 652 spec examples map every byte; fuzzing found two multi-byte slicing panics (fixed, with tests), no mapping errors. |
| **Glyph atlas / text rendering** | Custom text drawing in egui is new code on the critical path | Spike first in M1; `glow` fallback kept buildable | **Retired.** 240 fps (monitor cap) on wgpu and glow; textures capped to the GPU limit after a crash found in testing. |
| **Local reparse misses non-local effects** | Live view briefly wrong after edits that open fences or define link references | Background reparse is authoritative and lands within the debounce plus parse time; test this specifically | **Retired.** Tested (opening a fence); full parse lands in ~24 ms + parse time. |
| **Height estimation** | Scrollbar and minimap jitter as estimates turn into measured heights | Anchor scroll to the top visible block when heights change | **Mitigated.** Line-anchored scrolling; small jitter possible while far-off blocks get measured. |
| **Full reparse cost** | Multi-MB files would hitch if parsed on the UI thread | Always off-thread; debounce; rebase stale results | **Retired.** 64 ms for 5 MB on the worker; UI-thread catch-up 0.02 ms p95 after chunking spans. |
| **IME on Wayland** | Broken compose or CJK input under Hyprland | Test in M1; track egui/winit issues; degrade gracefully | **Retired.** fcitx5 checked by hand on Omarchy; both panes handle preedit/commit. |
| **Scroll sync drift** | Code and live have different line heights | Sync on block, not pixels | **Retired.** Sync is source line + fraction through the block. |
| **Portal / clipboard** | Missing portal means no file dialog; paste may be flaky | Document runtime deps; CLI path argument for open | **Retired.** Dialogs and 5 MB paste checked by hand; deps documented in README and PKGBUILD. |

---

## Repo layout

```
inkmark/
  Cargo.toml                 # workspace
  crates/
    inkmark/                 # bin: eframe app, modes, chrome, keymap
    inkmark-buffer/          # Rope wrapper (byte offsets), edits, EditLog, UndoStack, epoch, file I/O
    inkmark-parse/           # MarkdownParser trait, pulldown impl, BlockTree, SourceMap, rebase
    inkmark-text/            # cosmic-text layout, swash glyph atlas, Mesh building, HeightCache
    inkmark-view/            # CodeView, LiveView, reveal rules, smart edit rules, hit-test, scroll sync
    inkmark-minimap/         # sampling + paint helpers
    inkmark-files/           # folder tree model, listing, watching
  fixtures/                  # CommonMark and GFM spec examples (CC-BY-SA 4.0)
  pack/                      # PKGBUILD / install notes
  README.md
  PLAN.md                    # this document
```

As built, benches are `#[ignore]`d tests inside each crate, `scripts/measure.sh` drives the app-level measurements, and `pack/` holds the PKGBUILD, desktop entry and icon. GFM lives in `inkmark-parse` as `GfmParser` (pulldown-cmark plus an autolink pass), not a separate comrak crate as first sketched. Large test files stay outside the repo (`~/Projects/inkmark`).

---

## What to measure

- **Startup** to first paint (empty, and with a 5 MB file passed on the command line).
- **Idle RSS** after opening 1 MB and 5 MB files.
- **Edit latency:** keystroke → buffer commit → screen update (p50/p95) on a 5 MB file, mid-document, **in both panes**.
- **Reparse latency:** full reparse (background, must not block input) and local reparse (UI thread, target < 1ms).
- **Live patch size:** bytes changed per live edit (guards against rewriting whole blocks or files).
- **Scroll FPS** in split mode with both minimaps on.

---

## Results

Measured 2026-10-02 on the Omarchy host (240 Hz 1440p monitor, scale 1, Mesa radv Vulkan), release build. The 5 MB file is `War and Peace` + `Anna Karenina` as Markdown (5.3 MB, 102k lines); the 10 MB file is that twice.

| Measure | Target | Result |
|---|---|---|
| Startup to first frame | — | empty 116 ms · 1 MB 121 ms · 5 MB 128 ms · 10 MB 143 ms |
| Open 10 MB without freezing | parse off-thread | first frame 143 ms; full parse settled at 174 ms, off the UI thread |
| Idle RSS | — | empty 116 MB · 1 MB 125 MB · 5 MB 145 MB · 10 MB 168 MB (an empty eframe/wgpu window alone is 109 MB) |
| Edit latency, code pane, 5 MB mid-file | < 16 ms p95 | 0.34 ms p95 keystroke → frame (CPU), parsing included |
| Edit latency, live pane, longest paragraph (6.9 KB) | < 16 ms p95 | 4.2 ms p95 |
| Full reparse, 5 MB | off the UI thread | 64 ms on the worker |
| Local reparse / catch-up | < 1 ms | 0.02 ms p95 |
| Live patch size | = typed text | fuzzed: every keystroke inserts exactly its character |
| Scroll, split mode, both minimaps | smooth | 240 fps (monitor cap), p95 frame interval 4.5 ms, at 1, 5 and 10 MB |
| Live pane frame | — | scrolling 0.65 ms p95 · random jumps (all cold) 1.64 ms p95 |

**Decisions taken from the numbers**

- **Span size left at 48 bytes.** Prose produces few spans per paragraph, so halving them would save a few MB of 145; nearly all memory is the GPU stack.
- **No shape-run cache.** cosmic-text caches whole same-script runs, and a Latin paragraph is one run that changes on every keystroke. 4.2 ms on the longest paragraph is well inside budget; incremental paragraph layout is the lever if it's ever needed.

## GFM milestone (started 2026-10-02)

| # | Slice | Done when |
|---|--------|-----------|
| **G1** | `GfmParser` + source map | Every GFM spec example (cmark-gfm `spec.txt`) maps every byte exactly once and reproduces pulldown-cmark's text; table pipes and delimiter rows, task markers and strikethrough delimiters are classified; autolink literals match the spec's links |
| **G2** | Code pane + commands | Code pane colors the new syntax; the app parses GFM; Ctrl+Shift+X toggles strikethrough, Ctrl+Enter toggles a task, Enter continues task items |
| **G3** | Live rendering | Strikethrough, clickable task checkboxes (a click is a one-byte source patch), autolinks styled as links |
| **G4** | Live tables | Grid layout with alignment; typing in a cell patches that cell; Tab/Shift+Tab between cells; Enter in the last row adds a row; a typed `\|` is escaped. Re-padding columns to keep pipes aligned is out of scope |

**GFM done 2026-10-02:** G1 `57ad8a8` · G2 `9114853` · G3 `9c9f01b` · G4 `b5ca0ca`. All 672 GFM spec examples pass the byte-coverage test, autolinks match the spec's links, and the editing fuzzes run with a table, task list and strikethrough in the document. GFM full parse of 5.2 MB: 89 ms on the worker (61 ms for CommonMark; the autolink pass rebuilds the map when it finds links). Not done: re-padding table columns to keep pipes aligned, adding or removing columns, footnotes.

## Review (2026-10-02)

An outside review of `b5ca0ca` (Grok; `~/Projects/inkmark/REVIEW.md`, smoke tests in `~/Projects/inkmark/smoke`, both outside the repo) filed 21 issues, #1–#21. All are fixed with a regression test each, in `b8a50bb` (buffer), `1874a5f` (parse), `f4f7bb5` (live editing), `88bd259` (panes) and `4388bc5` (app). Testing #13 also turned up two scroll bugs, fixed in `88bd259`.

One smoke test is intentionally left failing: it wants the space after a task marker to be visible text. pulldown-cmark's item text doesn't include that space, and every visible span must reproduce that text exactly, so the task-marker span owns the space instead (`[x] `). It can no longer be dropped, which was the problem raised.

**Suggestions from the review not acted on yet:**

- Detect on-disk changes that keep both length and mtime (compare content or inode generation).
- If the directory fsync after the atomic rename fails, the save has in fact happened; don't report it as failed.
- `Recent::add` should canonicalize paths that don't exist yet, so one file isn't listed twice.
- Announce on-disk changes even while an error banner is showing.
- Say why Shift+Enter does nothing in a table.
- `Document` could implement `Debug`.

## Roadmap

### File browser (next)

A sidebar on the left showing a folder's structure, for opening other Markdown files in it.

**Done 2026-10-02:** F1 `5c52f01`, F2 `5620cd6`, F3 `c7e6fb5`.

**Decisions (2026-10-02)**

| Question | Decision |
|---|---|
| Root folder | `inkmark notes.md` browses the file's parent folder. `inkmark ~/notes` browses that folder, with no file open. `inkmark` with no argument browses the current directory, with the recent-files list shown as today. Inside the app, **Open Folder…** (Ctrl+Shift+O, portal folder picker) and an **Up** button (parent folder) change the root. |
| What's listed | Folders and Markdown files (`.md`, `.markdown`, `.mdown`, `.mkd`). Dot-files and other files are hidden; a "show all files" toggle lists them, with non-Markdown files greyed out and not openable. Empty folders are shown (hiding them would need a recursive scan). |
| File operations | Browse, open, change root, refresh on disk changes, and **New file** in a folder. Rename, move and delete come later (delete needs a trash and undo story). |
| Placement | A resizable left panel beside the panes in every mode. Ctrl+Shift+E shows/hides and focuses it (Ctrl+B is bold). Width and visibility persist in `$XDG_STATE_HOME/inkmark`. |
| Opening a file | Same path as Ctrl+O: the unsaved-changes prompt first, then the file opens in the panes. The open file is highlighted and its folders expanded. |
| Code layout | A new `inkmark-files` crate holds the tree model, listing and watching (no egui), so it can be tested on temp directories; the sidebar widget lives in `inkmark-view`. |

**Slices**

| # | Slice | Done when |
|---|--------|-----------|
| **F1** | Tree model + root selection | `inkmark-files` lists a folder lazily (a folder's children are read when it's first expanded), sorted folders-first with natural, case-insensitive order; filters as above; symlinks shown without following loops; unreadable folders marked, not fatal. The app takes a file, a folder or nothing on the command line and picks the root as decided. Unit tests on temp trees. |
| **F2** | Sidebar UI | Left panel with the tree: expand/collapse, keyboard navigation (Up/Down, Left/Right to collapse/expand, Enter to open), click to open, current file highlighted and revealed. Header with root name, Up, Open Folder… and refresh. Show/hide and width persist. Headless egui tests like the panes'. |
| **F3** | Live updates + New file | Expanded folders are watched (inotify via the `notify` crate), so files created, renamed or deleted elsewhere appear or disappear within a second; the open file's own disk banners keep working. **New file** (Ctrl+N, or from a folder) asks for a name, adds `.md` if missing, refuses existing names, creates the file and opens it. |

**Targets:** listing a folder with 10,000 entries doesn't stall the UI (listing off the UI thread, rows virtualized, frames under 16 ms); an external change shows up in the tree within 1 s; the panes' editing and scrolling numbers in [Results](#results) are unchanged with the sidebar open.

**Measured 2026-10-02** on the same host, release build, sidebar open at its default width:

| Measure | Target | Result |
|---|---|---|
| 10,000-entry folder | frames under 16 ms, only visible rows painted | frame p50 0.55 ms, p95 0.55 ms, max 0.56 ms; 26 rows painted |
| External create / rename / delete | visible within 1 s | inotify on expanded folders; the tests poll until it appears and fail at 1 s |
| Startup, sidebar open | unchanged | empty 116 ms · 1 MB 122 ms · 5 MB 126 ms · 10 MB 138 ms |
| Parse settled, sidebar open | off the UI thread | 5 MB 153 ms · 10 MB 173 ms |
| Idle RSS, sidebar open | unchanged | empty 117 MB · 1 MB 126 MB · 5 MB 156 MB · 10 MB 188 MB |
| Scroll, split, both minimaps, sidebar open | unchanged | 240 fps, p95 frame interval 4.5 ms, at 1, 5 and 10 MB |
| Pane benches (same harness as the table above; they don't open the sidebar) | unchanged | code edit p95 0.33 ms · live paragraph p95 4.24 ms · full reparse 68 ms · GFM 5.2 MB 96 ms · local reparse p95 0.02 ms · live scroll p95 0.66 ms · cold jumps p95 1.97 ms |

A same-day run of `master` (no sidebar) on this host was empty 113 ms / 116 MB, 5 MB 120 ms / 159 MB, 10 MB 150 ms / 189 MB, scroll still 240 fps at p95 4.5 ms. Large-file RSS is higher than the [Results](#results) table on both trees; opening the sidebar does not add it. Startup and scroll with the sidebar open match that run.

**Risks:** inotify watch limits on very large trees (only expanded folders are watched); slow or network filesystems (listing is off-thread and can be cancelled); symlink loops (not followed when expanding).

**Out of scope for this milestone:** rename, move, delete; search across files; links between notes and backlinks; tabs or several open files; git status in the tree.

### Later

- Footnotes (pulldown-cmark supports them; not part of the GFM spec).
- Tables: re-pad columns so pipes stay aligned as you type; add and remove columns.
- File management in the browser: rename, move, delete to trash.
- Code pane: snap fallback glyphs (box drawing, CJK) to whole monospace cells.
- Images: SVG; remote images (needs a network policy).
- The review suggestions above.
- Still standing from the MVP: a link reference deleted by a local edit lingers until the next full parse.
