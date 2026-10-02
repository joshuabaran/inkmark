# Markdown editor — build plan

**Name (working):** `inkmark` · Rust · egui/eframe · Linux/Wayland (Omarchy/Hyprland) first · no Electron/WebView

**Status:** Signed off 2026-10-01. Changes to locked decisions require updating this doc first.

---

## Locked decisions

| Area | Decision |
|------|----------|
| Buffer | `ropey::Rope`. **All positions are UTF-8 byte offsets**; the buffer crate wraps ropey's char-indexed API so nothing else sees char indices. |
| UI | egui/eframe, `wgpu` renderer by default (`glow` kept buildable as a fallback feature). |
| Text | cosmic-text shapes, lays out and hit-tests; swash rasterizes into **our own glyph atlas** (egui `TextureHandle`s), painted as egui `Mesh`es. No egui text layout inside editor panes; no glyphon. |
| Parser | `pulldown-cmark` (with `into_offset_iter`) behind `MarkdownParser`; CommonMark only in MVP; GFM later via `comrak` or similar implementing the same trait. |
| Parse strategy | Background full reparse is authoritative; synchronous **local reparse** of the edited top-level block for immediate feedback (see §2). Incremental parsing beyond that is a later optimization. |
| Live editing model | **Reveal-at-cursor** (Typora/Obsidian style). Live is the source with syntax hidden away from the caret; every live edit is a direct source byte-range patch. No AST→Markdown re-emit. |
| History | One document epoch, one undo stack shared by both panes. |
| Scope | Single-file open/save, dark theme, keyboard-usable. |
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
│    MarkdownParser trait → pulldown-cmark (MVP)              │
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
  - *Block markers* (heading `#`, list marker, `>` prefix, fence lines and info string, thematic break): reveal on the line that contains the caret.
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

### Future (do not design these out)

Full GFM, then math and diagrams. Tables in particular will need a block-specific live editor that still emits source patches.

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

---

## Testing

- **Buffer:** property tests check that random edit sequences give the same result on the rope and on a `String`, and that undo followed by redo returns to the same state.
- **Parse/SourceMap:** the CommonMark spec examples are fixtures. Checks:
  - spans cover each byte exactly once
  - a span rebased through the EditLog equals a fresh parse
  - after a local reparse, the next full reparse agrees on the edited block
- **Live editing:**
  - Scripted live edits must produce exactly the expected source.
  - Fuzzed live keystrokes must equal the same keystrokes applied at the mapped source offsets.
  - Patch size: a single character typed in live changes ≤ 1 inserted character plus any explicit smart-rule bytes.
- **Benches:** criterion benches in each crate (open, full reparse, local reparse, rebase, layout of the visible window). End-to-end latency is recorded by an `instrument` feature in the app binary.

---

## Risks

| Risk | Why it hurts | Mitigation |
|------|----------------|------------|
| **Live → source mapping** | Wrong offsets corrupt the file | Reveal-at-cursor model (no re-emit); inline SourceMap with full byte coverage; spike in M2; fuzz tests |
| **Glyph atlas / text rendering** | Custom text drawing in egui is new code on the critical path | Spike first in M1; `glow` fallback kept buildable |
| **Local reparse misses non-local effects** | Live view briefly wrong after edits that open fences or define link references | Background reparse is authoritative and lands within the debounce plus parse time; test this specifically |
| **Height estimation** | Scrollbar and minimap jitter as estimates turn into measured heights | Anchor scroll to the top visible block when heights change |
| **Full reparse cost** | Multi-MB files would hitch if parsed on the UI thread | Always off-thread; debounce; rebase stale results |
| **IME on Wayland** | Broken compose or CJK input under Hyprland | Test in M1; track egui/winit issues; degrade gracefully |
| **Scroll sync drift** | Code and live have different line heights | Sync on block, not pixels |
| **Portal / clipboard** | Missing portal means no file dialog; paste may be flaky | Document runtime deps; CLI path argument for open |

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
  fixtures/                  # CommonMark spec examples, large synthetic .md
  pack/                      # PKGBUILD / install notes
  README.md
  PLAN.md                    # this document
```

Benches live in each crate's own `benches/` folder. A later `inkmark-parse-gfm` crate implements the same parser trait with `comrak`.

---

## What to measure

- **Startup** to first paint (empty, and with a 5 MB file passed on the command line).
- **Idle RSS** after opening 1 MB and 5 MB files.
- **Edit latency:** keystroke → buffer commit → screen update (p50/p95) on a 5 MB file, mid-document, **in both panes**.
- **Reparse latency:** full reparse (background, must not block input) and local reparse (UI thread, target < 1ms).
- **Live patch size:** bytes changed per live edit (guards against rewriting whole blocks or files).
- **Scroll FPS** in split mode with both minimaps on.
