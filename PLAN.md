# Markdown editor — build plan

**Name (working):** `inkmark` · Rust · egui/eframe · Linux/Wayland (Omarchy/Hyprland) first · no Electron/WebView

**Status:** Signed off 2026-10-01. MVP (M1–M6), GFM (G1–G4) and the [file browser](#file-browser-next) (F1–F3) are complete as of 2026-10-02; the first outside review's 21 issues and seven suggestions are fixed, and footnotes are in ([hardening and footnotes](#hardening-and-footnotes-2026-10-02)); [notes and links](#notes-and-links) is in review. [Configurable key bindings](#configurable-key-bindings) are in as of 2026-10-02. v0.2.0 is released, and the silent-exit fix is merged. 0.2.1 is the unsaved mark in the [product review](#product-review-2026-10-03). 0.2.2 is the sidebar and outline chrome. See [Results](#results) for measurements and the [Roadmap](#roadmap) for what's planned. Changes to locked decisions require updating this doc first.

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
| Scope | Single-file open/save, dark theme, keyboard-usable. **Since 2026-10-02:** still one open document at a time; a folder browser picks which one ([file browser](#file-browser-next)). **Since 2026-10-03:** one open document stays the model ([product review](#product-review-2026-10-03)). |
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
- **Very long lines:** for lines above a threshold (e.g. 64 KB), shape only the visible part. Syntax coloring may fall back to plain text. **As built (CRO-112, 2026-10-08):** the threshold is 64 KiB. In the code pane, such a line wraps on its monospace cell grid without being shaped: each character takes one cell (two if wide, a tab to the next stop), so the rows, the line's height, and every caret position come from counting cells. Only the rows on screen are shaped, each as a short line, and syntax colors stay. In the live pane, a block holding such a line shows as a one-line notice to edit it in the code pane. The caret steps over the block, and typing at its start or end edits there.

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
  - *Smart editing rules* (list item continue/outdent on Enter, blockquote prefix on Enter, Tab/Shift+Tab nesting in lists, Backspace at the start of a marker): explicit rules that each produce one or more source patches. Tables, tasks and Enter-continues-list are the set. The [product review](#product-review-2026-10-03) stops new rules of this kind.
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
- **HeightCache:** each pane keeps a height per line (code) or per block (live). Heights are measured once laid out and estimated before that (from text length × average glyph advance ÷ wrap width). This drives the scrollbar, scroll sync and minimap. When a block's real height replaces its estimate, adjust the scroll position so the view stays anchored to the top visible block. The [product review](#product-review-2026-10-03) tightens the live pane: measure a block before it can move the scroll position, and estimate only below the viewport.
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
- **Benches** (since 2026-10-07, CRO-108): `scripts/bench.sh [quick|full]` is the one command. It generates fixtures from a fixed seed into `target/fixtures` (`inkmark-bench`: book-like prose at 1/5/10 MB, a 1 MB line, a list past the local-reparse limit, tables, images, formulas, CJK and emoji, references, a 10k-note folder), runs the criterion micro benches in `inkmark-buffer`, `inkmark-parse` and `inkmark-text` and the frame benches in `inkmark-view/benches/frames.rs` (both panes through egui, p50/p95/max and allocations per frame), and writes `target/bench/<sha>.{json,md}`. `full` adds the older `#[ignore]`d bench tests (which keep their 16 ms asserts) and the real-window numbers from `scripts/measure.sh` (`INKMARK_MEASURE=1`: startup, RSS, split scroll fps, keystroke → frame in both panes, save, idle CPU), run with an empty config and state directory. `--compare` against `docs/perf/baseline-<sha>.json` flags a median 15% slower or a p95 over 16 ms. CI compiles the benches and runs `quick` as a smoke test; the authoritative numbers come from `full` on the Omarchy host. Findings and the ranked follow-ups are in [docs/perf/FINDINGS.md](docs/perf/FINDINGS.md).
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

As built, `crates/inkmark-bench` generates the benchmark fixtures and reports, each crate's `benches/` holds its benches, `scripts/bench.sh` and `scripts/measure.sh` drive them, `docs/perf/` holds the findings and baselines, and `pack/` holds the PKGBUILD, desktop entry and icon. GFM lives in `inkmark-parse` as `GfmParser` (pulldown-cmark plus an autolink pass), not a separate comrak crate as first sketched. Large test files are generated by `inkmark-bench`, not kept outside the repo.

---

## What to measure

`scripts/bench.sh` measures all of these; each has a named bench in [docs/perf/FINDINGS.md](docs/perf/FINDINGS.md), with the baseline in [`docs/perf/baseline-196f110f0631.md`](docs/perf/baseline-196f110f0631.md).

- **Startup** to first paint (empty, and with a 5 MB file passed on the command line).
- **Idle RSS** after opening 1 MB and 5 MB files.
- **Edit latency:** keystroke → buffer commit → screen update (p50/p95) on a 5 MB file, mid-document, **in both panes**.
- **Reparse latency:** full reparse (background, must not block input) and local reparse (UI thread, target < 1ms).
- **Live patch size:** bytes changed per live edit (guards against rewriting whole blocks or files).
- **Scroll FPS** in split mode with both minimaps on.

---

## Results

The table below is the first round, kept for comparison. A fresh run of every row, on the same host, is in `docs/perf/` (CRO-108, 2026-10-07). Note: the "64 ms full reparse" came from the markup-dense synthetic sample in `bench_parse_5mb`; the Tolstoy book itself parses in about 8 ms (CommonMark) and 25 ms (GFM).

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

**Suggestions from the review:** all seven were done in the [hardening round](#hardening-and-footnotes-2026-10-02).

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

### Hardening and footnotes (2026-10-02)

Built on branch `hardening-footnotes` and reviewed by Grok before merging.

**The review's suggestions**

| Suggestion | Done |
|---|---|
| Changes that keep both length and mtime go unnoticed | The disk stamp also holds the file's (device, inode) and its ctime. Any write sets ctime and it can't be set back; replacing the file changes the inode. Same `stat` call, so no extra cost. A `chmod` now also counts as a change, which errs on the side of asking. |
| A failed directory fsync after the rename reports the save as failed | The save counts once the rename succeeds; the directory sync is best effort. |
| A file added to recents before it exists can be listed twice | A missing file is stored as its folder's canonical path plus its name. |
| An error banner hides disk changes | Errors have their own row above the disk banner; the disk check always runs. |
| Shift+Enter in a table is silent | The live pane explains it in the status bar for four seconds (`LiveView::take_hint`). |
| Item 999999999 continues as a ten-digit non-marker | Enter repeats 999999999. |
| `Document` has no `Debug` | It shows path, length, epoch, dirty and encoding, never the text. |

**Footnotes (decisions)**

| Question | Decision |
|---|---|
| Syntax | pulldown-cmark's `ENABLE_FOOTNOTES` (the GFM / cmark-gfm form), in `GfmParser` only. A reference without a definition stays text, as in GFM. |
| A reference `[^label]` | One `Replaced` span showing `[label]` in link color, with a new `Style::FOOTNOTE`. Like an entity, it shows its source when the caret touches it. |
| A definition `[^label]: …` | A container block (`BlockKind::FootnoteDefinition`), like a list item: its paragraphs are ordinary leaves. `[^label]: ` is `Syntax::FootnoteLabel`, drawn in the margin as `[label]` (shortened with `…` to fit) and never revealed, like a list marker; it's edited in the code pane. |
| Typing next to a reference | A local reparse only sees the edited block, and a reference needs its definition to parse. So the local parse gets stand-in definitions for the labels the document defines (`ParseOutput::footnotes`), appended after the region and dropped from the result. Without them the reference would flip to `[^1]` text on every keystroke. |
| Not done | Jumping between a reference and its definition (links aren't clickable yet either); numbering notes in order of first use, as GitHub's HTML does. The live view shows the label as written. |

GFM full parse of the 5.2 MB benchmark: 92 ms, unchanged.

### Notes and links

Working with a folder of notes: manage files from the sidebar, and follow links between them. Built on branch `notes-and-links` and reviewed by Grok before merging.

**Decisions (2026-10-02)**

| Question | Decision |
|---|---|
| Delete | Moves to the freedesktop trash (restorable from the file manager), after a confirmation. No in-app undo. Trashing the open file keeps its buffer, with the missing-file banner, as when another program deletes it. |
| Rename | F2 or the context menu, in a prompt holding the whole name with the part before the extension selected (as file managers do), so typing keeps the extension; the name is used exactly as written. Never overwrites: an existing name is refused, atomically (`renameat2` with `RENAME_NOREPLACE`). |
| Move | Drag a row onto a folder, or **Move to…** with the portal folder picker. Same no-overwrite rule; a folder can't move into itself; moving to another filesystem is refused rather than copied. |
| The open file | When it (or a folder above it) is renamed or moved, the document follows it: same buffer, undo history and unsaved edits, new path, and no "changed on disk" banner caused by the move itself. |
| Links | Ctrl+click in either pane follows the link under the pointer: `http(s)` and `mailto` open in the default app (`xdg-open`); a relative or absolute path to a Markdown file opens it in inkmark, through the unsaved-changes prompt; `#heading` jumps to the heading (GitHub's slug rules), also after a file path; a footnote reference jumps to its definition. Other targets (non-Markdown files, a Markdown file that doesn't exist, other schemes, `//host/…`, `file:` on another host) aren't opened: the status bar says why. |
| Back | Alt+Left returns to where the last followed link was clicked, in the same file or the previous one. The history follows renames and moves; a place in a file deleted since is reported, never reopened as a new empty file. |
| Links to a renamed note | Not rewritten this round; that needs a cross-file search and a preview. |

**Slices**

| # | Slice | Done when |
|---|--------|-----------|
| **N1** | File operations | `inkmark-files` renames, moves and trashes files and folders with the rules above; the trash is behind a seam, so tests never touch the real one. Unit tests on temp trees, including refusals. |
| **N2** | Sidebar UI | Right-click menu (Rename…, Move to…, Move to Trash, New file here), F2 and Delete keys, drag and drop with a highlighted target, the delete confirmation, and the open document following a rename or move. Headless egui and app tests. |
| **N3** | Following links | Ctrl+click in both panes (pointer cursor while Ctrl is held over a link), the targets above, and Alt+Left. Tests for link resolution (relative paths, `%20`, anchors, reference links, footnotes) and for following and going back in the app. |

**As built:** the trash is the `trash` crate behind a `Trash` trait; renames use `renameat2(RENAME_NOREPLACE)` via `rustix`. Link destinations come from re-parsing the clicked block with pulldown-cmark (reference links resolve through the document's definitions; autolink literals are the link-styled text itself). The sidebar tests check the right-click menu by finding its items in egui's drawn output, since sending clicks to the desktop isn't an option. Cancelling the unsaved-changes prompt puts the history step back (`settle_history`). Alt+Right is Forward, and non-Markdown links open through the allowlist, both in [Cleanup](#cleanup).

### Release v0.1.0

Decided 2026-10-03: the first release is **v0.1.0** (the version already in `Cargo.toml`), with a **Linux x86_64 binary** only. No CHANGELOG yet (release notes are generated from the merged PRs), no versioned Arch package (`inkmark-git` stays), no aarch64 build.

- `scripts/package.sh` builds `inkmark-<version>-<target>.tar.gz` (stripped binary, desktop entry, icon, licenses, README) and a `.sha256`; it runs the same locally and in CI.
- `.github/workflows/release.yml` runs on a `v*` tag: checks the tag matches the crate version, runs the tests, packages, and publishes the GitHub release.
- The owner pushes the tag after the release PR merges: `git tag v0.1.0 && git push origin v0.1.0`.
- `inkmark --help` / `--version` exist, and `--` ends options (the desktop entry uses `inkmark -- %f`).

### Theme and fonts

Decided 2026-10-03: follow the OS. No in-app theme picker; fonts are configurable.

| Question | Decision |
|---|---|
| Colors | If Omarchy's `~/.local/state/omarchy/current/theme/colors.toml` exists, it's used, and `omarchy theme set` applies live. Otherwise the built-in dark or light theme follows the desktop's light/dark setting. `INKMARK_THEME=<colors.toml>` overrides both (testing, screenshots). |
| Mapping a palette | Accent for headings and the caret, blue for links, green for code, yellow for emphasis, muted for markup; structural shades blended from background and foreground. Text colors too faint on the background are moved toward the foreground until they reach WCAG contrast (4.5:1 text, 3:1 markup). All 22 bundled Omarchy palettes pass. |
| Fonts | The document defaults to fontconfig's `monospace` (code) and `sans-serif` (live text), which is what `omarchy font set` changes. `~/.config/inkmark/config.toml` `[font]` overrides family and size for each; the file and `~/.config/fontconfig/fonts.conf` are checked every second and changes apply live. A missing font is reported and the default kept. The chrome (sidebar, status, dialogs) is Hack, which egui already bundles and which has the folder marks and ●. |

**Slices:** T1 one `Theme` for every color, built-in light, contrast test; T2 the OS theme (Omarchy palette, live switch, desktop light/dark); T3 fonts (fontconfig defaults, config file, live re-shaping).

### Table editing

Decided 2026-10-03.

| Question | Decision |
|---|---|
| Re-padding | When the caret leaves a table it was editing (in the live pane), the table is re-padded so the pipes line up, as its own undo step. A table you only moved through is left as it is. Typing in a cell still changes only that cell. Ctrl+Alt+F formats the table at the caret in either pane. |
| Layout | Leading and trailing pipes, one space inside each pipe, cells padded to the column's widest cell by display width (CJK counts double), padding on the side the column's alignment calls for. The delimiter row gets `:` where the alignment needs it. Each line keeps its container prefix (`> `, list indentation). Cell text is never changed, escaped pipes included; extra cells past the header's count are kept. |
| Rows and columns | A right-click menu on a live-pane cell: insert row above/below, insert column left/right, delete row/column, move row up/down, move column left/right, alignment (left, center, right, none). Shortcuts in both panes: Ctrl+Alt+Up/Down insert a row, Ctrl+Alt+Left/Right a column, Ctrl+Alt+Backspace deletes the row and Ctrl+Alt+Shift+Backspace the column, Alt+Shift+arrows move the row or column. (Ctrl+Alt+Delete is Omarchy's "close all windows".) The header row can't be deleted or moved below the delimiter. |
| New table | Ctrl+Alt+T, or "Insert table" in the live pane's right-click menu, inserts a 3×3 table with a header row and puts the caret in its first cell. |

**Slices:** TB1 a table model and its operations as pure source edits (`tables.rs`), tested by parsing the result; TB2 the live pane: re-pad on leaving, the right-click menu, shortcuts in both panes; TB3 inserting a new table.

### Cleanup

Small items left from earlier rounds, done 2026-10-03 on branch `cleanup`. Decided 2026-10-03:

- **Wide characters in the code pane** take exactly two monospace cells (box drawing and symbols already snap to one), so mixed CJK and Latin columns line up.
- **SVG images** render in the live pane like the other formats.
- **A link definition deleted by a local edit** stops resolving at once, not only after the next full parse.
- **Alt+Right** goes forward again after Alt+Left.
- **Links to non-Markdown files** open in the default app (`xdg-open`) only for an allowlist of safe types: images, PDF, plain text, audio, video, office documents. Anything else (scripts, `.desktop`, unknown types), and any file with an executable bit, is refused with a status-bar message.

### Configurable key bindings

Done 2026-10-02 on branch `key-bindings`. Decided 2026-10-02:

| Question | Decision |
|---|---|
| Where | A `[keys]` table in `~/.config/inkmark/config.toml`, applied live with the fonts. `bold = "Ctrl+B"`, or a list (`insert_row_below = ["Ctrl+Alt+Down", "F6"]`). An empty list unbinds. Anything left out keeps its default. |
| One table | `inkmark-view`'s `keys` module is the only list of actions and default chords. The shell, the sidebar and both panes read it. The chords that shipped are unchanged. Ctrl/Alt editing chords are actions too, so they can be unbound: word left/right, delete word left/right, document start/end, select all. |
| Matching | A chord matches when Shift and Alt are exactly the ones it names. Extra Shift or Alt does not fire the shorter chord (Ctrl+Shift+B is not bold). Linux Ctrl still matches: those events set both `ctrl` and `command`, and matching uses `matches_exact`. Shift on a word or document motion extends the selection. Shift on delete-word still deletes. |
| Table chords | Outside a table the key keeps its ordinary meaning, so Alt+Shift+Left still extends a selection. Shift on a table chord that doesn't name Shift is not that command: Ctrl+Alt+Shift+Left/Right selects a word, in a table or out of one, as Ctrl+Shift+Left/Right does. Ctrl+Alt+Shift+Backspace is delete-column, and outside a table that deletes a word. The shell consumes only its own actions, so a pane chord is never eaten by an app one. |
| Errors | An unknown action, a chord that doesn't parse, a value that isn't a chord or a list, or two actions on one chord is named in the error banner, and that entry keeps its default. So is a typing key with no Ctrl, Alt, or Super (`B`, `Shift+B`): the character would be inserted as well as the action running. The rest of the file, fonts included, still applies. A file that doesn't parse at all keeps the previous settings. A swap (each action taking the other's chord) applies. If rejecting one entry makes another collide with the default that snapped back, that one is rejected too. |
| Desktop keys | Super, Ctrl+Alt+Delete, Alt+Tab, Alt+Shift+Tab, Ctrl+Alt+Tab and Ctrl+Alt+Shift+Tab are the compositor's. Binding one is kept, and the banner says the desktop will take it. None of them is a default. |
| Listing | `inkmark --list-keys` prints the bindings that would apply, as TOML ready to paste back. The README shortcut table is generated from the same defaults; a test fails if the README drifts. |
| Not bindings | Arrows, Backspace, Delete, Enter, Tab, Home, End, Page Up/Down, and Shift held to extend a selection. Ctrl+click and IME. |

### Product review (2026-10-03)

From `~/Projects/inkmark/PRODUCT_REVIEW.md`, outside the repo. This is the order of work after v0.2.0. It replaces "search across the folder, then tabs, then math."

**Already shipped, so not scheduled again**

- Alt+Right, and cancelling the unsaved-changes prompt restores the history step ([Cleanup](#cleanup), [Notes and links](#notes-and-links)).
- Deleting a link or footnote definition drops that label on the local reparse (`replace_definitions`). References to that label, including in blocks the edit did not touch, are restyled in the same pass ([P1](#product-review-2026-10-03)).
- Clicking the live pane moves the caret. Ctrl+1 focuses the code pane at that spot.

**Decisions (2026-10-03)**

| Question | Decision |
|---|---|
| Live editing | Keep the model: a keystroke is a byte-range patch, both panes share one undo stack, and a typed character inserts that character. No mode that rewrites a block. No new smart rules, column choreography, or Typora parity. Tables, tasks, and Enter-continues-list stay. New live work is rendering and navigation. |
| One document | Tabs, backlinks, a graph, and tags stay off. One open document keeps a single undo stack and one scroll sync. |
| Find before folder search | Find and replace in the current file comes before search across the folder. Vim mode, multi-cursor, and an LSP wait until find and the outline have been in daily use for a week. |
| Math | A live-only overlay from the source span, drawn and never written back, and only once a note you have open uses it. Mermaid and other diagrams wait until a document you have open needs them. |
| Live line length | The live pane wraps to the width of its panel, as the code pane does (changed by CRO-127, 2026-10-08; it was about 70–80 characters). |
| Reading scroll | Measure a live block before its height can move the scroll position. Estimate only below the viewport. |
| Unsaved mark | The open file shows ● on its sidebar row, the same mark the footer puts beside the path. |

**Slices, in order**

| # | Slice | Done when |
|---|--------|-----------|
| **P0** | Unsaved mark | With unsaved edits, the open file's sidebar row shows ● beside its name, as the footer does. A saved file does not. Headless tests cover both. |
| **P1** | Stale reference paint | Deleting or changing a definition restyles every reference to that label on the local reparse, including references in blocks the edit did not touch. A regression test deletes a definition far from its reference and checks the reference's span before the full parse. |
| **P2** | Find and replace in this file | Rope byte offsets. Incremental, case-sensitive and case-insensitive, plain and regex, next and previous. Replace one, and replace all as one undo group. No widget that copies the document. |
| **P3** | Heading outline | Drawn from the `BlockTree`. Click jumps both panes. Same map as Ctrl+click on a heading in the live pane, kept visible. |
| **P4** | Go to line, fold by heading | Go to line in the code pane. Folds are display state on the code pane only; the live pane ignores them. Stored as source ranges so a reparse keeps them. |
| **P5** | Structural selection | In the code pane: select word, select paragraph, jump to the matching fence or link brackets, using the source map. |
| **P6** | Live reading | The live pane's measure is about 70–80 characters whatever the window width. Far-off blocks are not estimated in a way that shoves the viewport; only blocks at and above the viewport are measured, and only blocks below it are estimated. |
| **Layout** | Remembered layout | Reopening restores split, code, or live, each pane's minimap, the split between the panes, and the outline width. |
| **P7** | Math | When a note needs it: a live-only overlay for the math span. The source is unchanged, and the editor does not re-emit it. |
| **P8** | Search across the folder | Filename search first. Content search second, off the UI thread, the same way the parse is. This is also how a rename finds the references it does not rewrite. |

**P0, as built (0.2.1):** the open file's sidebar row draws ● beside its name while the buffer is dirty, the same mark the footer shows. A saved file does not, and neither does any other row. `the_open_file_shows_the_unsaved_mark` covers a clean file, a dirty file, and the mark going away again. The chrome is Hack, so ●, ▸, and ▾ have glyphs; the document font is unchanged.

**P1, as built:** when a label starts or stops resolving, every reference to that label is restyled during the local reparse, including references in blocks the edit did not touch. A destination change updates where the link goes and does not rebuild those spans, because a reference span does not carry the URL. The edited region is parsed with stand-in definitions for labels defined outside it, so a reference next to the caret does not flicker, and any other top-level block that mentions a changed label is re-parsed the same way. A block larger than 64 KiB still waits for the full parse, as every local reparse does. `deleting_a_far_definition_restyles_the_reference` deletes a link definition and a footnote definition that sit past twenty paragraphs and checks the reference spans before any full parse.

**P2, as built:** Ctrl+F opens find in this file and Ctrl+H opens it on the replacement. The search walks the rope by byte offset; the bar holds the query and the replacement. Each change to the query selects the next match from the caret where find was opened. Match case and regex are toggles, and an invalid pattern is reported in the bar. F3 and Shift+F3 move to the next and previous match and wrap around the file. Enter in the query does the same as F3; Enter in the replacement replaces the current match and selects the next one. Replace all is a single undo step. A selected single line of at most 256 bytes seeds the query when the bar opens.

**P3, as built:** the outline sits to the right of the panes and stays up when the file sidebar is hidden. It lists every heading in the block tree, indented by level, with the same words and byte offset a Ctrl+click on that heading uses. Clicking an entry puts the caret there in both panes and records the previous place for Alt+Left. A file with no headings shows "No headings".

**P4, as built:** Ctrl+G asks for a line number and puts the caret on that line in both panes, recording the previous place for Alt+Left. The number is 1-based. A line past the end lands on the last line, and an empty or non-numeric entry leaves the caret where it is. In the code pane, a heading with anything under it shows ▾ in the left margin. Clicking it hides the body through the next heading of the same or higher level, and the heading line stays. ▸ opens it again. The live pane still draws that text. The fold is the body's source range, so an edit moves it with the bytes and a reparse leaves it in place. A jump into the hidden lines opens the fold. Arrow keys stop on the last visible line, and Enter at the end of a folded heading leaves the new line on screen and keeps the fold. Opening another file clears the folds.

**P5, as built:** In the code pane, Ctrl+D selects the word under the caret, the same span a double-click selects. Selecting that word again stays on it. Ctrl+Shift+P selects the innermost block: a paragraph, heading, code block, or table, and the paragraph inside a list item or quote rather than the marker. A caret on the blank line between blocks selects that blank line. The same blank-line span is used when the parse has no blocks yet. Ctrl+Shift+Backslash jumps to the other side of a fence or of a link's brackets. A selection or a jump opens a fold that was hiding either end. Inside a fenced code block the jump is between the fences, and from the body it lands on the closing fence. An indented code block has no fence, so the caret stays. Otherwise `[` pairs with `]`, the `(` that opens a link destination pairs with the `)` that closes it, and `<` pairs with `>` in an autolink, including the `[` of an image. Parentheses inside the URL or the title stay put, and so does a parenthesis written with a backslash. A caret sitting just after a bracket counts as that bracket. The live pane ignores these three chords. The selection is shared, so the code pane's selection shows in the live pane.

**P6, as built:** The live pane wraps at the width of its panel, inside the padding. (Until CRO-127, 2026-10-08, it stopped at 75 characters of the live font.) Tables and images lay out in that same width. The code pane keeps the width of its split. The view is anchored to a source line, not a height. Before a scroll lands, the live pane measures every block the scroll passes through, above or below, and the screen it lands on. A caret move measures its own block and the span it crosses, and revealing the caret measures its block and one screen above it. A block that begins in the viewport is measured through to its end. A block entirely below the viewport stays estimated, and so can a block far above it that nothing has passed through. Because the anchor is a line, measuring that block later can't move the view. A measured block keeps its height until an edit changes it, which lays it out again, or a font, zoom, or width change, which drops every measurement. Then only the screen is measured again. Until CRO-116 (2026-10-08), every block from the start of the document was measured instead. On a 5 MB file, a jump to the end took 2.6 s (now 4 ms), and a width or zoom change took about 1.4 s (now under 15 ms); see docs/perf/FINDINGS.md §7.

**Layout, as built:** Reopening restores the last mode, so a code-only or live-only window comes back that way, and a live-only window focuses the live pane. Each pane keeps its own minimap. The code pane's share of the split, and the outline's width, come back too. Both dividers drag, and the width is written when the drag is released. A window too narrow to honor a width draws the panes and the outline smaller for that frame and leaves the stored width as it was. The file is `$XDG_STATE_HOME/inkmark/layout`, beside the sidebar's file. `reopening_restores_panes_and_minimaps` and `reopening_restores_the_split_and_the_outline` cover it.

**P7, as built:** `$...$` and `$$...$$` in ordinary text are drawn in the live pane. The source bytes stay as typed, and the editor does not write the formula back. A caret in the formula shows those bytes. Code, code blocks, and HTML are left as text. A formula that does not parse is shown as its source. `a_formula_is_drawn_and_the_source_stays` and `a_wide_formula_stays_on_one_line` cover it.

**P8, as built:** Ctrl+Shift+F searches the open folder. A file whose name matches is listed first. Matches inside those files follow, from a worker thread, the same way a parse does. Opening a file name opens that note. Opening a match inside a file selects it. The search covers the Markdown files the sidebar would list. `a_filename_query_opens_that_note` and `a_content_query_opens_the_match` cover it.

**Chrome, as built (0.2.2):** Ctrl+Shift+B shows or hides the outline, and that choice is remembered with the outline's width. « at the left of the status bar shows or hides the file sidebar, and » at the right does the same for the outline. Each tooltip names its chord. The sidebar's header is ↑, ↗, ↻, +, ⊞, and ∗, with tooltips; All files stays words. ∗ searches the open folder. ⊞ asks for a folder name in the selected folder or the open folder, and the right-click menu can create one beside an entry. An empty name asks for a name. A slash, `.`, or `..` is refused on its own. A dot folder is created and left unselected while All files is off. A folder that is gone before its row is listed does not claim a later folder of the same name. F1 lists the keys in effect and closes on Escape or F1. `the_outline_toggle_is_remembered`, `a_new_folder_is_created_beside_the_selection`, `a_hidden_new_folder_stays_unselected`, `a_missing_folder_does_not_keep_the_reveal`, and `f1_lists_the_keys_in_effect` cover it.

**Not this round:** vim mode, multi-cursor, LSP or Marksman-style diagnostics, tabs, backlinks, a graph, tags, Mermaid and other diagrams, images from the network, rewriting links in other notes when one is renamed or moved, screen-reader support (AccessKit). Link rewriting waits on P8 and a change preview. Network images still need a policy: inkmark makes no network requests.

**Crashes.** Issue 35: the window closed during ordinary use and left no core dump and no Omarchy notification. Leaving the 5 MB file idle reproduced `overflow when subtracting durations`, exit 101, no core. The theme settle (150 ms) and the status hint (4 s) each read the clock twice and subtracted. Both now use one `checked_sub`. Fixed in #36. A separate clipboard-thread segfault on shutdown does dump core; those exits did not.

The session log is `$XDG_STATE_HOME/inkmark/session.log` (or `INKMARK_LOG`). It records the start, the first frame, focus changes, an `alive` line while the UI draws, a close request and whether it was cancelled, and whether the event loop returned ok or with an error. `log` and `tracing` warnings go there too. A panic writes its backtrace. Past 1 MiB the file is renamed with a `.1` suffix and a new one starts. When stderr is `/dev/null` or closed, it is duplicated onto that file; a redirect or a pipe is left alone. `scripts/e2e.sh` can hold a window open or soak it. The next unexpected close should say which of those lines was last.

### Later

Held until the [product review](#product-review-2026-10-03) says to pick them up:

- Images from the network (needs a network policy: inkmark makes no network requests today).
- Updating links in other notes when a note is renamed or moved (after P8, with a preview).
- Tabs, backlinks, a graph, tags, Mermaid and other diagrams, vim mode, multi-cursor, an LSP, and AccessKit for the editor panes.
