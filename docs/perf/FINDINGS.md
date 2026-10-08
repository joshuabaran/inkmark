# Performance findings (CRO-108)

This spike reviewed inkmark's hot paths at `e356be2` (0.2.2) and measured every hypothesis
from the ticket on the benchmark suite it added. No product
code changed for speed; the follow-ups are ranked at the end.

- **Baseline:** [`baseline-196f110f0631.md`](baseline-196f110f0631.md) (and
  `.json` for `scripts/bench.sh --compare`) is `scripts/bench.sh full` at
  `196f110`, which is `e356be2` plus the bench code. Host: Omarchy, Ryzen 9
  9900X (24 threads), Radeon RX 9070 (Mesa 26.2.2, radv), two 2560×1440
  monitors at 240 Hz, scale 1, kernel 7.2.5, NVMe on btrfs. Frame benches
  time the CPU only (the egui pass plus tessellation, no GPU), at
  1600×1000 points with the code pane on the left half and the live pane on
  the right.
- **Fixtures:** generated from a fixed seed into `target/fixtures`
  (`inkmark-bench`). The 5 MB book imitates `tolstoy.md`: lines wrapped at 72
  columns, paragraph p50 158 B and p90 629 B, a 6,907-byte longest paragraph.
  Checked against the real book: GFM parse 25 ms against 25 ms, CommonMark
  11 ms against 8 ms (the fixture has more inline markup), 92k lines against
  103k.
- **Names:** `frames/…` rows are frame benches, `micro/…` rows are criterion
  medians, `app/…` rows come from a real window (`scripts/measure.sh`), and
  `search/…` rows come from the folder-search bench test. "Budget" is
  PLAN.md's 16 ms p95.
- **Variance:** two full runs of the same code on this host differed by
  10–12% on most frame benches, all in the same direction (CPU clocks and
  background load). The compare step's 15% threshold sits just above that,
  so re-run before acting on a flag. Real-window keystroke times varied
  more than that (30–60%) between runs. Read them for their order of
  magnitude.
- **Profiles:** CPU profiles (samply) weren't possible on the host. samply
  needs `perf_event_paranoid` ≤ 1 and a `perf_event_mlock_kb` large enough
  for 24 CPUs. Costs were split instead with targeted benches that each
  change one variable, and with allocations per frame from a counting
  allocator. Each verdict names the benches that isolate it.

## Summary

Ordinary editing is fast and stays fast at 10 MB. At 5 MB, typing costs
1.4–2.0 ms of CPU per frame in split mode with both panes drawn, and the
longest paragraph costs 5 ms. Split scrolling holds 240 fps. A debounced
full parse lands about 58 ms after a keystroke, with no hitch when it
starts or when it lands.

The 16 ms budget breaks mostly where the live pane **lays out everything
above the viewport in one frame**, which is P6's measure-before-scroll.
Ctrl+End on a freshly opened 5 MB file takes 2.6 s. Dragging the window while
the live pane is narrower than its 75-character measure costs 1.4 s **per
frame**. One zoom step costs 280 ms at 1 MB.

The other problems, in order:

- very long lines: 1.6 s per keystroke on a 1 MB line;
- big tables: 210 ms to re-pad a 500×8 table;
- a regex query in find: 123 ms per keystroke on 5 MB;
- CJK and emoji scrolling: 20 ms p95;
- a sidebar timer that wakes the app 4–5 times a second while idle.

## Hot paths

Each hypothesis gets one verdict:

- **Confirmed:** the hypothesis holds and costs something.
- **Confirmed, minor:** it holds but stays well inside budget.
- **Rejected:** it doesn't hold.
- **New:** found while measuring.

### 1. Typing and edit latency (both panes): Confirmed, minor

| Bench | p50 | p95 | max |
|---|---:|---:|---:|
| `frames/split/code_typing_mid_5mb` | 1.95 ms | 2.13 ms | 2.58 ms |
| `frames/split/code_enter_mid_5mb` | 1.14 ms | 1.78 ms | 2.26 ms |
| `frames/split/live_typing_short_paragraph_5mb` (160 B) | 1.36 ms | 1.54 ms | 1.73 ms |
| `frames/split/live_typing_longest_paragraph_5mb` (6.9 KB) | 5.04 ms | 5.44 ms | 5.94 ms |
| `frames/split/live_enter_5mb` | 1.10 ms | 1.55 ms | 2.17 ms |
| `app/code_keystroke_to_frame_end_10mb` (real window) | 1.51 ms | 1.64 ms | 1.68 ms |
| `app/live_keystroke_to_frame_end_10mb` | 0.61 ms | 0.75 ms | see §7 |

- **The parse side is small.** `micro/catch_up/1_edit_prose_5mb` is 19 µs: a
  rebase of 5 MB of spans plus the local reparse. Enter costs the same.
- **`HeightCache::splice` is O(lines) when the line count changes.**
  `micro/heights/splice_new_line_100k` is 181 µs for two splices, about 90 µs
  per Enter at 100k lines. The cost is real but negligible. `lines.rs`
  `sync` re-estimates only the changed lines, which doesn't show up.
- **The live keystroke is mostly re-shaping the paragraph.** cosmic-text
  shapes and wraps the whole paragraph again: `micro/shape/cold_6900b` is
  3.2 ms of the 5 ms frame. Incremental paragraph layout is still the fix,
  and it isn't needed yet.
- **Local reparse at the limit misses its 1 ms target.**
  `micro/local_reparse/63kb` is 1.09 ms (16 KB: 0.28 ms, 1 KB: 20 µs). An
  edit in a block just under `LOCAL_REPARSE_LIMIT` spends about 1 ms here.
- **P1 made catch-up grow with the number of link definitions.** Bisected
  on the old synthetic-sample bench (`bench_parse_5mb`, 20k definitions):
  0.02 ms p95 at `108aeea`, 0.12 ms at `8ab99b6` ("Restyle references when a
  definition changes"), 0.32 ms at `2c20a78` ("Skip restyle when only the
  destination changes"). Every keystroke sorts and scans every `Definition`
  in `replace_definitions`, `append_standins` and `resolved`. On the
  2,000-definition fixture, `micro/catch_up/1_edit_references` is 145 µs
  against 19 µs on prose. That's inside budget, but it grows with the note.

### 2. Full-parse handoff: Rejected

- The UI thread's share is tiny. `micro/handoff/rope_to_string_5mb` (the
  copy when a debounced parse starts) is 96 µs.
  `micro/handoff/drop_output_5mb` (dropping the replaced output in `accept`)
  is 85 µs.
- Typing at a human pace doesn't hitch either. `frames/split/paced_*` sends
  a keystroke, then draws a frame every 4 ms until the parse lands:

  | Frame | p95 |
  |---|---:|
  | The keystroke | 1.97 ms |
  | Waiting, including the frame that sends the copy | 2.03 ms |
  | The frame that lands the parse | 2.73 ms |

  Keystroke to settled full parse takes 58 ms (p50).
- The worker's cost: `micro/parse/gfm_prose_5mb` is 25 ms, CommonMark
  11 ms. The GFM autolink pass rebuilds the map, which more than doubles the
  time, all on the worker.
- PLAN's "64 ms full parse" came from the markup-dense synthetic sample in
  `bench_parse_5mb` (68–80 ms today), not from the book.

### 3. Shaping and layout (cosmic-text): Confirmed, minor (long lines: see [A3](#a3-very-long-lines-cro-112))

- **The line-cache key is hashed twice per call.** SipHash runs once in
  `cached_line` and once more in its callers, `rich_geometry` and
  `draw_rich`, and a live block calls both every frame.
  `micro/shape/warm_geometry_6900b` is 14 µs for a 6.9 KB paragraph, about
  half of it hashing. That's negligible next to drawing: `warm_draw_6900b`
  is 278 µs, mostly atlas lookups per glyph.
- **Eviction drops everything at capacity.** Past 4,096 cached lines,
  `end_frame` keeps only the current frame's lines, so scrolling back shapes
  the text again. After 7,500 lines, `frames/code/scroll_back_5mb` (2.47 ms
  p50) costs about the same as `scroll_down_5mb` (2.73 ms). That's inside
  budget at 30 lines per frame; an LRU would make scrolling back free.
- **`wide_scales` lays out every CJK or emoji line twice in the code pane**:
  a probe layout, then the real one. §4 has what CJK costs overall.
- **Every live keystroke re-shapes the whole paragraph** (§1).
- **There's no long-line threshold** ([A3](#a3-very-long-lines-cro-112)).

### 4. Glyph atlas: Rejected (no resets); the CJK cost is in shaping (New)

- `atlas_resets` stayed at 0 in every bench, including CJK and emoji
  scrolling across six zoom levels: `frames/split/cjk_emoji_zoom_scroll`
  filled 3 of the 4 pages with 3,709 glyphs. Overflow still clears the
  whole atlas, but realistic text didn't reach it.
- **New: CJK and emoji are slow to shape.**
  `frames/split/cjk_emoji_scroll` (7 lines per frame, no zoom) is 14.1 ms
  p50 and **20.1 ms p95**, with 102k allocations per frame.
  `micro/shape/cold_cjk_emoji_300b` takes 310 µs against 147 µs for Latin
  text of the same length. The extra comes from cosmic-text's per-run font
  fallback, and the code pane shapes those lines twice (§3).
- **Without a font that covers the text, it gets far worse.** The GitHub
  runner has only DejaVu and Noto Core (no CJK, no color emoji), and there
  the same scroll took **763 ms per frame** (p50), with 2.3 million
  allocations: every run searches every installed font and finds nothing.
  A user without a CJK font who opens a CJK note pays this.

### 5. Code pane per frame: Confirmed, minor

- `frames/code/idle_frame_5mb` is 0.47 ms, with 668 allocations per frame.
- Split mode costs 1.04 ms per frame, and 0.72 ms with both minimaps off, so
  the minimaps cost about 0.3 ms per frame. `paint_minimap` walks
  `rope().line(l).chars()` and calls `spans_in` for every minimap row.
- `doc.slice` per line, `spans_in` per line (a new `Vec` each time), and the
  cloned events cost allocations, not time. They aren't worth a ticket on
  their own.

### 6. Live pane per frame: Confirmed (cheap for prose, expensive for tables)

- `place()` runs for every visible leaf on every frame, and there's no
  per-block cache. For prose that's cheap because the line cache is warm:
  `frames/live/idle_frame_5mb` is 0.53 ms.
- Image blocks re-parse their source every frame (`inline_images`), but it
  doesn't show: `frames/live/image_note_paging` p50 is 96 µs.
- **Tables are the exception.** On every frame, `place_table` lays out every
  cell twice (at natural width, then at the final width) and measures every
  word of every cell through the line cache. A warm 500×8 table costs
  1.93 ms per frame with 15k allocations
  (`frames/live/table_typing_500x8`). Anything that changes every cell pays
  the cold cost for the whole table (§13).

### 7. P6 reading scroll and huge-document jumps: Confirmed (the worst finding)

Before the live pane scrolls, it measures every block from
`measured_prefix` down to the target, in one UI-thread frame. The prefix
resets to 0 on `Synced::Rebuilt`: opening a file, a font or zoom change, or
any change to the live wrap width.

| Bench | p50 | max |
|---|---:|---:|
| `frames/live/jump_end_cold_5mb` (Ctrl+End after opening) | **2,559 ms** | 2,618 ms |
| `frames/split/jump_end_cold_5mb` | **2,546 ms** | 2,569 ms |
| `frames/live/scroll_from_third_5mb` (the first frame lands a third of the way down) | 0.29 ms | **866 ms** |
| `frames/live/random_jumps_5mb` | 1.57 ms | **962 ms** |
| `frames/split/resize_drag_narrow_5mb` (live pane narrower than 75 chars) | **1,355 ms per frame** | 1,380 ms |
| `frames/split/resize_drag_narrow_1mb` | **276 ms per frame** | 279 ms |
| `frames/split/prose_zoom_steps_1mb` (Ctrl+plus/minus) | **277 ms** | 284 ms |
| `app/live_keystroke_to_frame_end_5mb` (first keystroke after a mid-document jump) | 0.72 ms | **42.9 ms** |
| `app/live_keystroke_to_frame_end_10mb` | 0.61 ms | **89.5 ms** |

- **The cost is cold shaping.** The Ctrl+End frame makes about 3 million
  allocations: every block above the target is shaped cold
  (`micro/shape/cold_160b` 66 µs, `cold_630b` 257 µs) and stored in the line
  cache.
- **Once the prefix is measured, the same jump is cheap.**
  `frames/live/jump_end_after_top_edit_5mb` is 0.43 ms. An edit moves the
  prefix back, but the blocks below it stay measured.
- **Zoom cost is all re-measuring.** An empty document zooms in 0.13 ms
  (`frames/split/empty_zoom_steps`).
- **The real-window spike takes a different path.** It comes from the app's
  `jump_to`, not from Ctrl+End, and costs less, but it still runs 3–6×
  over budget. In the earlier full run it showed up on the code pane's
  first keystroke instead (43.7 and 92.1 ms).
- **PLAN's cold-jump number only covered p95.** The 1.64 ms "cold jump"
  p95 in PLAN's Results still roughly holds (2.2–2.7 ms today). The max was
  never recorded, and the max is the problem.
- **Even a plain width change costs O(lines).** Every height is
  re-estimated: `frames/split/resize_drag_5mb`, where only the code pane
  re-wraps, is 9.5 ms per frame at 5 MB, so about 19 ms at 10 MB.

### 8. Images: Confirmed (memory); worker time is fine

- **Decoding is fine on the worker.** Done the way `images.rs` does it
  (`frames/worker/image_decode_*`): 1.4 ms at 800×600, 12 ms at 3000×2000,
  and **190 ms at 5000×3000** (scaled down to 4096 with the triangle filter).
  The UI frame never waits: the max in `frames/live/image_note_paging` is
  5.7 ms.
- **Textures are never evicted.** After paging through 24 images, 119–129 MB
  of RGBA textures were left across runs (`texture_mb`). Each one keeps its
  decoded size, up to 4096² (64 MB), whatever size it's drawn at.
  `ImageCache` also survives opening another document, because
  `LiveView::reset` doesn't clear it, so this grows for the whole session.

### 9. Math: Rejected at this density

- On a note with 300 inline and 50 display formulas,
  `frames/live/math_first_sight` is 3.30 ms and `math_paging` 2.98 ms p50
  (3.67 ms max). About 20 formulas get typeset synchronously for each new
  screen, well inside budget.
- `MathCache::begin_frame` drops every formula the previous frame didn't
  use, so scrolling back typesets them again. That's the cost above, every
  time.

### 10. File open and save: Open rejected; save confirmed on real disks

- **Opening is fast.** `micro/file/open_5mb` is 3.4 ms and `open_10mb`
  6.8 ms (1.5 GB/s).
- **Saving takes 6–13 ms on the UI thread.** On the NVMe (btrfs), fsync
  included, `micro/file/save_5mb` is **6.4 ms** and `save_10mb` **11.3 ms**.
  In the real window, `app/save_5mb` is 7.0 ms, `save_10mb` 12.3 ms, and
  `save_1mb` has a 10.5 ms p95. That's under budget here, but on a slow
  disk, a USB stick or a network mount the window freezes for the whole
  fsync.
- **The once-a-second disk check is negligible.** `micro/file/disk_status_*`
  is 270 ns.

### 11. Idle cost: Confirmed, and worse than expected

- **The idle app wakes 4.5–4.9 times a second, not once.** That's the
  `frames_per_s` in `app/idle_*`. The measure mode logs egui's repaint
  causes, and most name `inkmark-view/src/browser.rs:408`: `poll_watch`
  asks for a repaint 250 ms out whenever any folder is expanded, and the
  root always is. The 1 s `DISK_CHECK_INTERVAL` repaint is the other
  source.
- **Each wakeup draws a full frame.** `frames/split/idle_frame_5mb` is
  1.04 ms with 1,423 allocations. Idle CPU across two runs: 0.2–0.4% with no
  file, and 0.8–1.7% at 5 and 10 MB.

### 12. Find and folder search: Confirmed for regex

- **Find runs on the UI thread on every query change.** Each change compiles
  the query and runs `collect_matches`:

  | Query on 5 MB | Time per change |
  |---|---:|
  | Common word (`micro/find/query_common_word_5mb`) | 1.6 ms |
  | Rare word (`query_rare_word_5mb`) | 0.56 ms |
  | Regex `\b\w+ly\b` (`query_regex_5mb`) | **123 ms** |

  With Regex on, every keystroke in the query field freezes the window that
  long.
- **The other find operations are fine.** `census` takes 1.6 ms.
  `prev_from_middle` takes 0.79 ms, because it walks from the start once
  the list is past its limit. `replace_all_common_word_1mb` takes 3.9 ms.
- **Folder search is fine: it runs off the UI thread.** On 10,000 notes
  (`search/folder_*`):

  | Query | Names | First content hit | Done |
  |---|---:|---:|---:|
  | Rare word | 33 ms | 33 ms | 70 ms |
  | Common word (49k hits) | 33 ms | 33 ms | 103 ms |
  | Regex | 32 ms | 32 ms | 320 ms |

### 13. Table editing: Confirmed

| Bench (500 rows × 8 columns) | p50 | max |
|---|---:|---:|
| `frames/live/table_typing_500x8` | 1.93 ms | 3.24 ms |
| `frames/code/table_typing_500x8` | 1.87 ms | 2.14 ms |
| `frames/live/table_first_paint_500x8` (cold) | **137 ms** | 140 ms |
| `frames/live/table_repad_on_leave_500x8` | **210 ms** | 210 ms |
| `frames/live/table_insert_column_500x8` (Ctrl+Alt+Right) | **84 ms** | 163 ms |

Typing in a cell is fine because only that cell's lines are new. Re-padding
and column operations rewrite every row. Every cell is then laid out cold,
twice (§6), on top of the edit and the reparse.

### A3: very long lines (CRO-112)

| Bench (`long-line-1mb.md`, one 1 MB line) | p50 | max |
|---|---:|---:|
| `frames/code/long_line_1mb_typing` | **1,552 ms** per keystroke | 1,571 ms |
| `frames/live/long_line_1mb_typing` | **1,550 ms** per keystroke | 1,567 ms |
| Open and settle (code pane / live pane) | 1,828 / 2,320 ms | |

Every keystroke shapes and wraps the whole megabyte again, with 1.46 million
allocations per frame in the code pane. PLAN §1's long-line threshold is
needed.

## Fresh run of PLAN.md › Results

The old numbers are PLAN.md's (2026-10-02, on Tolstoy). The new numbers
come from the same host and fall into three kinds:

- **Same harness, Tolstoy:** the old `#[ignore]`d benches re-run on
  `tolstoy.md` at `e356be2`.
- **Same harness, fixture:** the same benches at `196f110`, which now read
  the generated book.
- **New benches:** the suite this spike added.

| Measure | 2026-10-02 | 2026-10-07 |
|---|---|---|
| Startup to first frame | empty 116 · 1 MB 121 · 5 MB 128 · 10 MB 143 ms | empty 119 · 1 MB 125 · 5 MB 129 · 10 MB 138 ms; Tolstoy 136 ms |
| Parse settled, 10 MB | 174 ms | 194 ms |
| Idle RSS | empty 116 · 1 MB 125 · 5 MB 145 · 10 MB 168 MB | empty 117 · 1 MB 129 · 5 MB 161 · 10 MB 196 MB; Tolstoy 159 MB |
| Edit latency, code, 5 MB | 0.34 ms p95 (code pane alone) | Same harness (its own synthetic text) 0.35 ms at `e356be2`, 0.40 ms at `196f110`. Split, both panes 2.13 ms. Real window 1.27 ms (fixture) / 2.26 ms (Tolstoy). |
| Edit latency, live, longest paragraph | 4.2 ms p95 | Same harness 4.44 ms (Tolstoy) / 4.74 ms (fixture). Split 5.44 ms. |
| Full reparse, 5 MB | 64 ms (synthetic sample) | Sample 68–80 ms. The book: 8 ms CommonMark, 25 ms GFM. |
| GFM, 5.2 MB with tables | 89–96 ms | 111–131 ms (same sample) |
| Local reparse / catch-up | 0.02 ms p95 | Sample 0.30–0.46 ms p95 (P1, see §1). Prose: 19 µs. |
| Split scroll, both minimaps | 240 fps, p95 4.5 ms | 240 fps, p95 4.4–4.6 ms at 1, 5 and 10 MB |
| Live pane frame | scroll 0.65 · random jumps 1.64 ms p95 | Tolstoy: scroll 0.73 · jumps 2.00 ms p95, **max 914 and 1,301 ms**. Fixture: 0.72 · 2.21 ms p95, **max 921 and 1,012 ms**. |
| 10k-entry sidebar | p50 0.55 ms | Same harness 0.53 ms. With note content and a 1,000-pt-tall sidebar: 0.71 ms. |

At 5 and 10 MB, RSS is higher than in the first round; the file-browser
round had already noted that. Neither RSS nor startup is a budget problem.

## Follow-up tickets, ranked

Ranked by what a user feels first, then by effort and by risk to byte-exact
editing. Each ticket lists the bench that proves it and a target. Numbers
are p95 unless noted.

| # | Title | Bench | Now | Target | Effort / risk |
|---|---|---|---|---|---|
| 1 | Live pane: stop measuring the whole prefix in one frame. A jump lands on an estimate and measures around the target. A wrap, font or zoom change keeps measured heights (scaled) or re-measures over several frames. | `frames/live/jump_end_cold_5mb`, `split/resize_drag_narrow_5mb`, `split/prose_zoom_steps_1mb`, `live/random_jumps_5mb` (max) | 2.6 s · 1.4 s per frame · 280 ms · 962 ms max | All < 16 ms, max included | M / M (P6's no-shove guarantee must still hold) |
| 2 | Idle: stop the 4 Hz sidebar wake. Repaint only from the watch thread, or back off to the disk-check interval. | `app/idle_*` `frames_per_s` | 4.5–4.9 per second | ≤ 1.1 per second | S / low |
| 3 | Very long lines: shape only the visible part (CRO-112). | `frames/code/long_line_1mb_typing`, `live/long_line_1mb_typing` | 1.6 s per keystroke | Code pane < 16 ms; live pane doesn't freeze | L / M |
| 4 | Find: run a regex query off the UI thread, first match first and the count after, as folder search does. | `micro/find/query_regex_5mb` | 123 ms per query keystroke | < 16 ms on the UI thread | M / low |
| 5 | Tables: cache cell layouts and measure each cell once, taking the natural width from the same layout instead of per-word geometry. | `frames/live/table_repad_on_leave_500x8`, `table_insert_column_500x8`, `table_first_paint_500x8` | 210 · 163 · 140 ms | < 16 ms | M / low |
| 6 | CJK and emoji: cache wide-character scales per font and size instead of a probe layout per line, and cache cosmic-text's fallback misses (a script no installed font covers). | `frames/split/cjk_emoji_scroll`, `micro/shape/cold_cjk_emoji_300b` | 20.1 ms p95 here; 763 ms per frame with no CJK font | < 12 ms p95, with or without the font | M / low |
| 7 | Images: evict textures (LRU by bytes), drop them when another file opens, and keep them at drawn size instead of decoded size. | `frames/live/image_note_paging` `texture_mb` | 119–129 MB for 24 images, never freed | Bounded (e.g. 256 MB), freed on open | S–M / low |
| 8 | Save off the UI thread: write a rope snapshot on a worker and keep the disk-stamp rules. | `micro/file/save_10mb`, `app/save_*` | 11–13 ms on NVMe; unbounded on slow disks | < 1 ms on the UI thread | M / M (detecting changes on disk) |
| 9 | Width changes: make the estimate of every line's height lazy or incremental. | `frames/split/resize_drag_5mb` | 10 ms per frame at 5 MB (~19 ms at 10 MB) | < 4 ms at 10 MB | S–M / low |
| 10 | Definitions: index them by label so a keystroke doesn't sort and scan them all (the P1 regression). | `micro/catch_up/1_edit_references`, `bench_parse_5mb` catch-up | 145 µs; 0.46 ms | About prose (20 µs); 0.05 ms | S / low |
| 11 | Line cache: evict the least recently used lines instead of keeping only the current frame's. | `frames/code/scroll_back_5mb` against `scroll_down_5mb` | 2.5 ms against 2.7 ms | Scrolling back < 0.5 ms | S / low |
| 12 | Local reparse limit: lower it (e.g. to 32 KiB), or accept 1 ms at the top. | `micro/local_reparse/63kb` | 1.09 ms | < 1 ms | S / low |

Not proposed, because each is well inside budget:

- the full-parse handoff (§2);
- math (§9);
- the glyph atlas (§4);
- file open (§10);
- the code pane's per-frame work (§5);
- folder search (§12).

Allocation churn can ride along with the tickets that touch those paths:
thousands of allocations per frame in the panes, and 20k in the sidebar.

## Running it

```sh
scripts/bench.sh quick                    # about 3 minutes; what CI runs
scripts/bench.sh full                     # about 20 minutes, plus app windows
scripts/bench.sh full --compare docs/perf/baseline-196f110f0631.json
cargo bench -p inkmark-view --bench frames -- jump_end   # one frame bench
cargo bench -p inkmark-parse -- catch_up                 # a criterion filter
```

Reports land in `target/bench/<sha>.{json,md}`. `--compare` flags any median
more than 15% slower than the baseline, and any p95 over 16 ms. `--strict`
turns those flags into a failing exit status. The benches that are over
budget today keep being flagged until their tickets land.
