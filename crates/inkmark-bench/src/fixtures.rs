//! Benchmark documents, generated from a fixed seed so every machine (CI,
//! a laptop, the Omarchy host) measures the same bytes.
//!
//! The prose imitates the Tolstoy test book the first results were taken
//! on: Gutenberg-style lines wrapped at 72 columns, paragraphs with a
//! median of about 160 bytes and a long tail (p90 about 630, p99 about
//! 1.7 KB), a 6.9 KB longest paragraph, a heading every few dozen
//! paragraphs, and about 1% non-ASCII (accented names, curly quotes).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Bumped whenever any generator's output changes, so stale fixture
/// directories are rebuilt.
pub const VERSION: u32 = 1;

/// The seed every fixture starts from.
pub const SEED: u64 = 0x1D0C_5EED_2026;

/// Bytes in the longest planted paragraph, as in the Tolstoy book.
pub const LONGEST_PARAGRAPH: usize = 6907;

/// A small, fast PRNG (xorshift64*). Not for anything but fixtures.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const WORDS: &[&str] = &[
    "the",
    "of",
    "and",
    "to",
    "a",
    "in",
    "he",
    "that",
    "was",
    "his",
    "with",
    "had",
    "it",
    "her",
    "as",
    "at",
    "not",
    "for",
    "him",
    "on",
    "she",
    "said",
    "but",
    "you",
    "is",
    "all",
    "by",
    "be",
    "from",
    "this",
    "were",
    "they",
    "what",
    "so",
    "which",
    "have",
    "one",
    "there",
    "me",
    "an",
    "been",
    "would",
    "only",
    "now",
    "their",
    "who",
    "when",
    "them",
    "into",
    "could",
    "more",
    "out",
    "no",
    "up",
    "if",
    "felt",
    "about",
    "will",
    "do",
    "did",
    "man",
    "how",
    "face",
    "eyes",
    "again",
    "know",
    "look",
    "very",
    "went",
    "come",
    "prince",
    "princess",
    "count",
    "countess",
    "room",
    "door",
    "voice",
    "hand",
    "smile",
    "army",
    "battle",
    "regiment",
    "horse",
    "officer",
    "general",
    "soldiers",
    "moscow",
    "letter",
    "evening",
    "morning",
    "dinner",
    "drawing",
    "suddenly",
    "quietly",
    "himself",
    "herself",
    "something",
    "nothing",
    "everything",
    "always",
    "never",
    "though",
    "without",
    "before",
    "after",
    "while",
    "around",
    "toward",
    "between",
    "under",
    "against",
    "understood",
    "remembered",
    "thought",
    "answered",
    "looked",
    "turned",
    "walked",
    "glanced",
    "listened",
    "replied",
    "asked",
    "began",
    "seemed",
    "wished",
    "loved",
    "young",
    "old",
    "little",
    "great",
    "beautiful",
    "pale",
    "silent",
    "happy",
    "strange",
    "dear",
    "whole",
    "same",
    "other",
    "first",
    "last",
    "long",
    "own",
    "such",
    "French",
    "Russian",
    "Emperor",
    "life",
    "death",
    "war",
    "peace",
    "God",
    "heart",
    "soul",
    "time",
    "day",
    "night",
];

const NAMES: &[&str] = &[
    "Pierre",
    "Natásha",
    "Andréi",
    "Nikolái",
    "Hélène",
    "Anna Pávlovna",
    "Prince Vasíli",
    "Bolkónski",
    "Rostóv",
    "Kutúzov",
    "Denísov",
    "Sónya",
    "Dólokhov",
    "Márya",
    "Levin",
    "Kitty",
    "Vronsky",
    "Stepan Arkádyevitch",
    "Karénin",
    "Dárya Alexándrovna",
];

/// Paragraph length quantiles (fraction, bytes) from the Tolstoy book,
/// interpolated in log space.
const PARAGRAPH_QUANTILES: &[(f64, f64)] = &[
    (0.0, 18.0),
    (0.25, 70.0),
    (0.5, 158.0),
    (0.75, 360.0),
    (0.9, 629.0),
    (0.99, 1684.0),
    (1.0, 3800.0),
];

fn paragraph_len(rng: &mut Rng) -> usize {
    let u = rng.unit();
    let i = PARAGRAPH_QUANTILES
        .windows(2)
        .position(|w| u < w[1].0)
        .unwrap_or(PARAGRAPH_QUANTILES.len() - 2);
    let ((q0, b0), (q1, b1)) = (PARAGRAPH_QUANTILES[i], PARAGRAPH_QUANTILES[i + 1]);
    let t = (u - q0) / (q1 - q0);
    (b0.ln() + (b1.ln() - b0.ln()) * t).exp() as usize
}

/// One word, sometimes a name, sometimes with inline Markdown around it.
fn word(rng: &mut Rng, markup: bool) -> String {
    let w = if rng.chance(0.04) {
        (*rng.pick(NAMES)).to_owned()
    } else {
        (*rng.pick(WORDS)).to_owned()
    };
    if !markup {
        return w;
    }
    let r = rng.unit();
    if r < 0.012 {
        format!("*{w}*")
    } else if r < 0.016 {
        format!("**{w}**")
    } else if r < 0.018 {
        format!("`{w}`")
    } else if r < 0.021 {
        format!("[{w}](https://example.com/{})", rng.below(1000))
    } else {
        w
    }
}

/// Sentences until the paragraph holds about `len` bytes, unwrapped.
fn paragraph_text(rng: &mut Rng, len: usize, markup: bool) -> String {
    let mut out = String::with_capacity(len + 64);
    let dialogue = rng.chance(0.15);
    if dialogue {
        out.push('“');
    }
    while out.len() < len {
        let words = 6 + rng.below(20);
        let mut sentence = String::new();
        for i in 0..words {
            let mut w = word(rng, markup);
            if i == 0 {
                let mut c = w.chars();
                if let Some(first) = c.next() {
                    w = first.to_uppercase().chain(c).collect();
                }
            }
            if i > 0 {
                sentence.push(if rng.chance(0.08) { ',' } else { ' ' });
                if sentence.ends_with(',') {
                    sentence.push(' ');
                }
            }
            sentence.push_str(&w);
        }
        sentence.push(*rng.pick(&['.', '.', '.', '!', '?', ';']));
        if !out.is_empty() && !out.ends_with('“') {
            out.push(' ');
        }
        out.push_str(&sentence);
    }
    if dialogue {
        out.push('”');
    }
    out
}

/// Wraps `text` at `width` columns on spaces, the way Gutenberg texts are.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / width + 1);
    let mut col = 0;
    for (i, w) in text.split(' ').enumerate() {
        let n = w.chars().count();
        if i > 0 {
            if col + 1 + n > width {
                out.push('\n');
                col = 0;
            } else {
                out.push(' ');
                col += 1;
            }
        }
        out.push_str(w);
        col += n;
    }
    out
}

/// Book-like prose of about `bytes` bytes (never less), ending in `\n`.
/// A [`LONGEST_PARAGRAPH`]-byte paragraph is planted near every megabyte,
/// so the longest paragraph is the same at every size from 1 MB up.
pub fn prose(bytes: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(bytes + 8 * 1024);
    out.push_str("# A Book of Prose\n\n");
    let mut chapter = 0;
    let mut next_long = 500_000;
    while out.len() < bytes {
        chapter += 1;
        let _ = write!(out, "## Chapter {chapter}\n\n");
        let paragraphs = 20 + rng.below(25);
        for _ in 0..paragraphs {
            let text = if out.len() >= next_long {
                next_long += 1_000_000;
                exact_paragraph(&mut rng, LONGEST_PARAGRAPH)
            } else {
                let len = paragraph_len(&mut rng);
                wrap(&paragraph_text(&mut rng, len, true), 72)
            };
            out.push_str(&text);
            out.push_str("\n\n");
        }
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

/// A plain, wrapped paragraph of exactly `len` bytes.
fn exact_paragraph(rng: &mut Rng, len: usize) -> String {
    let mut text = wrap(&paragraph_text(rng, len + 200, false), 72);
    // Cut at a char boundary at or below `len`, then pad with ASCII.
    let mut cut = len.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    while text.len() < len {
        text.push('x');
    }
    text
}

/// One line of about `bytes` bytes with no line break, then `\n`.
pub fn long_line(bytes: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(bytes + 64);
    while out.len() < bytes {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word(&mut rng, true));
    }
    out.push('\n');
    out
}

/// A top-level list longer than the 64 KiB local-reparse limit, between
/// two paragraphs.
pub fn huge_list(items: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::from("# A long list\n\nAn opening paragraph before the list.\n\n");
    for i in 0..items {
        let len = 30 + rng.below(80);
        let _ = writeln!(out, "- Item {i}: {}", paragraph_text(&mut rng, len, true));
        if rng.chance(0.1) {
            let len = 20 + rng.below(40);
            let _ = writeln!(out, "  - {}", paragraph_text(&mut rng, len, true));
        }
    }
    out.push_str("\nA closing paragraph after the list.\n");
    out
}

fn table(rng: &mut Rng, rows: usize, cols: usize) -> String {
    let mut out = String::new();
    let cell = |rng: &mut Rng| {
        let n = 1 + rng.below(4);
        (0..n)
            .map(|_| word(rng, false))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let header: Vec<String> = (0..cols).map(|c| format!("Column {c}")).collect();
    let _ = writeln!(out, "| {} |", header.join(" | "));
    let aligns: Vec<&str> = (0..cols)
        .map(|c| match c % 4 {
            0 => "---",
            1 => ":---",
            2 => ":---:",
            _ => "---:",
        })
        .collect();
    let _ = writeln!(out, "| {} |", aligns.join(" | "));
    for _ in 0..rows {
        let row: Vec<String> = (0..cols).map(|_| cell(rng)).collect();
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }
    out
}

/// One big table (`rows` × `cols`) and twenty small ones, between prose.
pub fn tables(rows: usize, cols: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::from("# Tables\n\nA paragraph before the big table.\n\n");
    out.push_str(&table(&mut rng, rows, cols));
    for i in 0..20 {
        let len = paragraph_len(&mut rng);
        let _ = write!(
            out,
            "\n## Table {i}\n\n{}\n\n",
            wrap(&paragraph_text(&mut rng, len, true), 72)
        );
        out.push_str(&table(&mut rng, 10, 4));
    }
    out
}

/// Prose with `inline` distinct inline formulas and `display` display
/// formulas, so a first frame has to typeset each one.
pub fn math(inline: usize, display: usize, seed: u64) -> String {
    const SHAPES: &[&str] = &[
        "x^{N} + y_{N}",
        "\\frac{a_{N}}{b + N}",
        "\\sqrt{N + z^2}",
        "\\alpha_{N} \\beta^{N}",
        "e^{i \\pi N}",
        "\\sum_{k=1}^{N} k",
    ];
    const DISPLAY: &[&str] = &[
        "\\int_0^{N} f(x)\\,dx = F(N) - F(0)",
        "\\sum_{k=1}^{N} k^2 = \\frac{N(N+1)(2N+1)}{6}",
        "\\begin{pmatrix} a & N \\\\ c & d \\end{pmatrix}",
        "\\lim_{n \\to \\infty} \\left(1 + \\frac{N}{n}\\right)^n",
    ];
    let mut rng = Rng::new(seed);
    let mut out = String::from("# Formulas\n\n");
    let mut placed_display = 0;
    let mut n = 0;
    while n < inline {
        let len = 80 + rng.below(240);
        let mut text = paragraph_text(&mut rng, len, false);
        for _ in 0..3.min(inline - n) {
            let f = rng.pick(SHAPES).replace('N', &n.to_string());
            let _ = write!(text, " Then ${f}$ holds.");
            n += 1;
        }
        out.push_str(&wrap(&text, 72));
        out.push_str("\n\n");
        if placed_display < display && rng.chance(display as f64 / (inline as f64 / 3.0)) {
            let f = rng.pick(DISPLAY).replace('N', &placed_display.to_string());
            let _ = write!(out, "$${f}$$\n\n");
            placed_display += 1;
        }
    }
    while placed_display < display {
        let f = rng.pick(DISPLAY).replace('N', &placed_display.to_string());
        let _ = write!(out, "$${f}$$\n\n");
        placed_display += 1;
    }
    out
}

/// Mixed CJK, emoji and Latin prose of about `bytes` bytes.
pub fn cjk_emoji(bytes: usize, seed: u64) -> String {
    const CJK: &[&str] = &[
        "戦争と平和",
        "春の日に",
        "東京の雨",
        "我们今天去公园",
        "这是一个很长的句子",
        "한국어 문장입니다",
        "静かな夜",
        "読書は楽しい",
        "学习中文",
        "山と川",
    ];
    const EMOJI: &[&str] = &["😀", "🎉", "📚", "🚀", "🌸", "👍🏽", "🇯🇵", "❤️", "🧪", "✨"];
    let mut rng = Rng::new(seed);
    let mut out = String::from("# 混合テキスト Mixed text 🌏\n\n");
    while out.len() < bytes {
        let mut para = String::new();
        let pieces = 8 + rng.below(30);
        for _ in 0..pieces {
            let r = rng.unit();
            let piece = if r < 0.45 {
                (*rng.pick(CJK)).to_owned()
            } else if r < 0.6 {
                (*rng.pick(EMOJI)).to_owned()
            } else {
                word(&mut rng, true)
            };
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(&piece);
        }
        para.push('。');
        out.push_str(&para);
        out.push_str("\n\n");
        if rng.chance(0.05) {
            let _ = write!(out, "## 章 {} {}\n\n", rng.below(100), rng.pick(EMOJI));
        }
    }
    out
}

/// Prose full of reference links and footnotes, with `defs` link
/// definitions and `notes` footnote definitions at the end.
pub fn references(defs: usize, notes: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut out = String::from("# References\n\n");
    let paragraphs = (defs + notes).max(1) / 2;
    for _ in 0..paragraphs {
        let len = 80 + rng.below(300);
        let mut text = paragraph_text(&mut rng, len, false);
        let _ = write!(
            text,
            " See [the source][ref{}] and the note[^n{}].",
            rng.below(defs.max(1)),
            rng.below(notes.max(1))
        );
        out.push_str(&wrap(&text, 72));
        out.push_str("\n\n");
    }
    for i in 0..defs {
        let _ = writeln!(
            out,
            "[ref{i}]: https://example.com/ref/{i} \"Reference {i}\""
        );
    }
    out.push('\n');
    for i in 0..notes {
        let len = 40 + rng.below(120);
        let _ = write!(out, "[^n{i}]: {}\n\n", paragraph_text(&mut rng, len, false));
    }
    out
}

/// Image sizes cycled through by [`write_images`]: thumbnails up to a
/// photo larger than the 4096 px decode cap.
pub const IMAGE_SIZES: &[(u32, u32)] = &[
    (64, 64),
    (320, 200),
    (800, 600),
    (1600, 1200),
    (3000, 2000),
    (5000, 3000),
];

/// `images.md` and `images/img-N.png` in `dir`: `count` images between
/// short paragraphs.
pub fn write_images(dir: &Path, count: usize, seed: u64) -> io::Result<()> {
    let mut rng = Rng::new(seed);
    fs::create_dir_all(dir.join("images"))?;
    let mut doc = String::from("# Images\n\n");
    for i in 0..count {
        let (w, h) = IMAGE_SIZES[i % IMAGE_SIZES.len()];
        let name = format!("img-{i}.png");
        let path = dir.join("images").join(&name);
        // Gradients compress well, so large images stay small on disk
        // but decode to their full size.
        let shade = (i * 37 % 255) as u8;
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, shade])
        });
        img.save(&path).map_err(io::Error::other)?;
        let len = 60 + rng.below(200);
        let _ = write!(
            doc,
            "{}\n\n![Figure {i}](images/{name})\n\n",
            wrap(&paragraph_text(&mut rng, len, true), 72)
        );
    }
    fs::write(dir.join("images.md"), doc)
}

/// The word planted in some notes of [`write_folder`], for content search.
pub const NEEDLE: &str = "zebracorn";

/// `count` short notes in one folder, as the sidebar and folder search
/// see a large notes directory. One note in a hundred mentions [`NEEDLE`].
pub fn write_folder(dir: &Path, count: usize, seed: u64) -> io::Result<()> {
    let mut rng = Rng::new(seed);
    fs::create_dir_all(dir)?;
    for i in 0..count {
        let mut note = format!("# Note {i}\n\n");
        let paragraphs = 1 + rng.below(4);
        for p in 0..paragraphs {
            let len = paragraph_len(&mut rng).min(1200);
            let mut text = paragraph_text(&mut rng, len, true);
            if p == 0 && i % 100 == 42 {
                let _ = write!(text, " A {NEEDLE} appears.");
            }
            note.push_str(&wrap(&text, 72));
            note.push_str("\n\n");
        }
        fs::write(dir.join(format!("note-{i:05}.md")), note)?;
    }
    Ok(())
}

/// FNV-1a, for the manifest's checksums. Stable across platforms.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A text fixture's file name and its generator.
pub type TextFixture = (&'static str, Box<dyn Fn() -> String>);

/// The text fixtures, by file name.
pub fn text_fixtures() -> Vec<TextFixture> {
    vec![
        ("prose-1mb.md", Box::new(|| prose(1_000_000, SEED))),
        ("prose-5mb.md", Box::new(|| prose(5_000_000, SEED))),
        ("prose-10mb.md", Box::new(|| prose(10_000_000, SEED))),
        ("long-line-1mb.md", Box::new(|| long_line(1_000_000, SEED))),
        ("huge-list.md", Box::new(|| huge_list(2_000, SEED))),
        ("tables.md", Box::new(|| tables(500, 8, SEED))),
        ("math.md", Box::new(|| math(300, 50, SEED))),
        ("cjk-emoji.md", Box::new(|| cjk_emoji(300_000, SEED))),
        ("references.md", Box::new(|| references(2_000, 500, SEED))),
    ]
}

/// Where fixtures live: `$INKMARK_FIXTURES`, else `target/fixtures` at
/// the workspace root.
pub fn dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("INKMARK_FIXTURES") {
        return PathBuf::from(dir);
    }
    workspace_root().join("target").join("fixtures")
}

/// The workspace root, from this crate's manifest directory.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate sits two levels below the workspace")
        .to_path_buf()
}

/// A text fixture's contents: read from [`dir`] when generated there, or
/// generated in memory (same bytes) when not.
pub fn load(name: &str) -> String {
    if let Ok(text) = fs::read_to_string(dir().join(name)) {
        return text;
    }
    let (_, make) = text_fixtures()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no fixture named {name}"));
    make()
}

/// Writes every fixture into `dir`, unless its manifest says this
/// generator version already did. Returns the manifest text.
pub fn generate(dir: &Path, notes: usize, images: usize) -> io::Result<String> {
    let manifest_path = dir.join("MANIFEST");
    let header =
        format!("inkmark fixtures v{VERSION} seed {SEED:#x} notes {notes} images {images}\n");
    if let Ok(existing) = fs::read_to_string(&manifest_path)
        && existing.starts_with(&header)
    {
        return Ok(existing);
    }
    fs::create_dir_all(dir)?;
    let mut manifest = header;
    for (name, make) in text_fixtures() {
        let text = make();
        fs::write(dir.join(name), &text)?;
        let _ = writeln!(
            manifest,
            "{name} {} {:016x}",
            text.len(),
            fnv64(text.as_bytes())
        );
    }
    let images_dir = dir.join("image-note");
    let _ = fs::remove_dir_all(&images_dir);
    write_images(&images_dir, images, SEED)?;
    let _ = writeln!(manifest, "image-note/ {images} images");
    let folder = dir.join("folder-10k");
    let _ = fs::remove_dir_all(&folder);
    write_folder(&folder, notes, SEED)?;
    let _ = writeln!(manifest, "folder-10k/ {notes} notes");
    fs::write(&manifest_path, &manifest)?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_is_deterministic_and_pinned() {
        let a = prose(200_000, SEED);
        assert_eq!(a, prose(200_000, SEED));
        assert!(a.len() >= 200_000);
        // Pinned so a change to the generator (or to platform behavior)
        // is noticed: bump VERSION and update this when it is deliberate.
        assert_eq!(
            fnv64(a.as_bytes()),
            PINNED_PROSE_200K,
            "{:#x}",
            fnv64(a.as_bytes())
        );
    }

    const PINNED_PROSE_200K: u64 = 0xb32b_f26f_f572_92bc;

    #[test]
    fn prose_looks_like_the_book() {
        let text = prose(1_000_000, SEED);
        let mut lens: Vec<usize> = text
            .split("\n\n")
            .filter(|p| !p.starts_with('#'))
            .map(str::len)
            .collect();
        lens.sort_unstable();
        let q = |f: f64| lens[((lens.len() - 1) as f64 * f) as usize];
        assert!((100..260).contains(&q(0.5)), "p50 {}", q(0.5));
        assert!((400..900).contains(&q(0.9)), "p90 {}", q(0.9));
        assert_eq!(*lens.last().unwrap(), LONGEST_PARAGRAPH);
        assert!(text.lines().all(|l| l.chars().count() <= 200));
    }

    #[test]
    fn the_list_is_past_the_local_reparse_limit() {
        let list = huge_list(2_000, SEED);
        let start = list.find("- Item 0").unwrap();
        let end = list.find("\nA closing").unwrap();
        assert!(end - start > 64 * 1024, "{}", end - start);
    }

    #[test]
    fn the_long_line_is_one_line() {
        let line = long_line(100_000, SEED);
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.len() > 100_000);
    }
}
