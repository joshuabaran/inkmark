//! Images for the live view, decoded off the UI thread and kept as textures.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use egui::{ColorImage, TextureHandle, TextureOptions, Vec2, vec2};

/// Larger images are scaled down to this many pixels on their longest side
/// (or the GPU's texture limit, if smaller).
const MAX_DIMENSION: u32 = 4096;
/// Files bigger than this aren't decoded.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Decoded textures kept at once. One 4096² image is 64 MiB, so this holds
/// two of those and then the least recently shown one is dropped.
const TEXTURE_BUDGET: usize = 128 * 1024 * 1024;

/// What the live view can show for an image right now.
#[derive(Clone)]
pub(crate) enum ImageSlot {
    Loading,
    Ready {
        texture: TextureHandle,
        size: Vec2,
    },
    Failed(String),
    /// http(s) and data URLs: not fetched in the MVP.
    Remote,
}

enum State {
    Loading,
    Ready(TextureHandle, Vec2),
    Failed(String),
}

/// Modification time and length. A missing file has neither, so creating it
/// is a different stamp and the failure is decoded again.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn missing() -> Self {
        Self {
            modified: None,
            len: 0,
        }
    }
}

struct Job {
    path: PathBuf,
    stamp: Stamp,
}

struct Entry {
    stamp: Stamp,
    state: State,
    bytes: usize,
    /// Tabs that have asked for this image and not been closed or reloaded.
    users: HashSet<u64>,
    /// Bumped on each request. Eviction drops the smallest first.
    used: u64,
}

struct Inner {
    map: HashMap<PathBuf, Entry>,
    bytes: usize,
    clock: u64,
    budget: usize,
}

struct Shared {
    inner: Mutex<Inner>,
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
    /// Tab whose images are decoded before the others.
    active: AtomicU64,
    paused: AtomicBool,
    closed: AtomicBool,
    /// `ImageCache` clones. The worker does not count, so the last clone
    /// can stop it.
    owners: AtomicUsize,
}

/// One decoder and one texture map. Clone it into every live view so an
/// image used by two documents is decoded once. The key is the canonical
/// path plus the file's stamp, so a renamed link is not decoded twice and
/// a replaced file is decoded again. Closing a tab drops images nobody
/// else is showing. Past the byte budget, the least recently shown image
/// that is not on screen is dropped. The tab on screen is decoded first.
pub struct ImageCache {
    shared: Arc<Shared>,
}

impl Clone for ImageCache {
    fn clone(&self) -> Self {
        self.shared.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        if self.shared.owners.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.cv.notify_all();
        }
    }
}

impl ImageCache {
    pub fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                bytes: 0,
                clock: 0,
                budget: TEXTURE_BUDGET,
            }),
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            active: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            owners: AtomicUsize::new(1),
        });
        let worker = Arc::clone(&shared);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("inkmark-images".into())
            .spawn(move || serve(worker, ctx))
            .expect("spawn image thread");
        Self { shared }
    }

    /// The tab on screen. Its queued images move ahead of the others.
    pub fn set_active(&self, user: u64) {
        if self.shared.active.swap(user, Ordering::AcqRel) == user {
            return;
        }
        let wanted: HashSet<PathBuf> = {
            let inner = self.shared.inner.lock().expect("image cache poisoned");
            inner
                .map
                .iter()
                .filter(|(_, entry)| {
                    entry.users.contains(&user) && matches!(entry.state, State::Loading)
                })
                .map(|(path, _)| path.clone())
                .collect()
        };
        let mut queue = self.shared.queue.lock().expect("image queue poisoned");
        let mut jobs: Vec<Job> = queue.drain(..).collect();
        // Stable: the active tab's jobs stay in their own order, in front.
        jobs.sort_by_key(|job| usize::from(!wanted.contains(&job.path)));
        *queue = jobs.into();
        self.shared.cv.notify_all();
    }

    /// The tab closed, or its document was replaced. Images nobody else is
    /// showing are dropped, and their queued decodes are not started.
    pub fn release_user(&self, user: u64) {
        let mut drop_paths = Vec::new();
        {
            let mut inner = self.shared.inner.lock().expect("image cache poisoned");
            let mut dropped = 0;
            inner.map.retain(|path, entry| {
                entry.users.remove(&user);
                if entry.users.is_empty() {
                    dropped += entry.bytes;
                    drop_paths.push(path.clone());
                    false
                } else {
                    true
                }
            });
            inner.bytes = inner.bytes.saturating_sub(dropped);
        }
        if drop_paths.is_empty() {
            return;
        }
        let mut queue = self.shared.queue.lock().expect("image queue poisoned");
        queue.retain(|job| !drop_paths.iter().any(|path| path == &job.path));
    }

    /// The image at `dest` (as written in the Markdown), relative to `base`
    /// (the document's folder). The first request starts loading it. `user`
    /// is the tab, so closing that tab can drop the image.
    pub(crate) fn get(&self, dest: &str, base: Option<&Path>, user: u64) -> ImageSlot {
        let Some(path) = resolve(dest, base) else {
            return ImageSlot::Remote;
        };
        let (path, stamp) = identify(path);
        let promote = {
            let mut inner = self.shared.inner.lock().expect("image cache poisoned");
            let active = self.shared.active.load(Ordering::Acquire) == user;
            inner.clock = inner.clock.wrapping_add(1);
            let used = inner.clock;
            if let Some(entry) = inner.map.get_mut(&path)
                && entry.stamp == stamp
            {
                entry.used = used;
                entry.users.insert(user);
                matches!(entry.state, State::Loading) && active
            } else {
                if let Some(old) = inner.map.remove(&path) {
                    inner.bytes = inner.bytes.saturating_sub(old.bytes);
                }
                inner.map.insert(
                    path.clone(),
                    Entry {
                        stamp,
                        state: State::Loading,
                        bytes: 0,
                        users: HashSet::from([user]),
                        used,
                    },
                );
                drop(inner);
                self.enqueue(Job { path, stamp }, user);
                return ImageSlot::Loading;
            }
        };
        if promote {
            self.promote(&path);
        }
        // The slot was built while the lock was held. Re-read it: the
        // branch above returns early for a miss, so this is a hit.
        let inner = self.shared.inner.lock().expect("image cache poisoned");
        inner
            .map
            .get(&path)
            .map(slot_of)
            .unwrap_or(ImageSlot::Loading)
    }

    fn enqueue(&self, job: Job, user: u64) {
        let active = self.shared.active.load(Ordering::Acquire) == user;
        let mut queue = self.shared.queue.lock().expect("image queue poisoned");
        if let Some(existing) = queue.iter().position(|queued| queued.path == job.path) {
            if active {
                let job = queue.remove(existing).expect("position just found");
                queue.push_front(job);
            }
        } else if active {
            queue.push_front(job);
        } else {
            queue.push_back(job);
        }
        self.shared.cv.notify_one();
    }

    fn promote(&self, path: &Path) {
        let mut queue = self.shared.queue.lock().expect("image queue poisoned");
        if let Some(index) = queue.iter().position(|job| job.path == path) {
            let job = queue.remove(index).expect("position just found");
            queue.push_front(job);
        }
    }

    #[cfg(test)]
    fn stored(&self) -> usize {
        self.shared
            .inner
            .lock()
            .expect("image cache poisoned")
            .map
            .len()
    }

    #[cfg(test)]
    fn set_budget(&self, budget: usize) {
        self.shared
            .inner
            .lock()
            .expect("image cache poisoned")
            .budget = budget;
    }

    #[cfg(test)]
    fn resident_bytes(&self) -> usize {
        self.shared
            .inner
            .lock()
            .expect("image cache poisoned")
            .bytes
    }

    #[cfg(test)]
    fn pause(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
        self.shared.cv.notify_all();
    }

    #[cfg(test)]
    fn queued(&self) -> Vec<PathBuf> {
        self.shared
            .queue
            .lock()
            .expect("image queue poisoned")
            .iter()
            .map(|job| job.path.clone())
            .collect()
    }
}

fn slot_of(entry: &Entry) -> ImageSlot {
    match &entry.state {
        State::Loading => ImageSlot::Loading,
        State::Ready(texture, size) => ImageSlot::Ready {
            texture: texture.clone(),
            size: *size,
        },
        State::Failed(e) => ImageSlot::Failed(e.clone()),
    }
}

fn serve(shared: Arc<Shared>, ctx: egui::Context) {
    while let Some(job) = next_job(&shared) {
        if !still_wanted(&shared, &job) {
            continue;
        }
        let max_side = ctx.input(|i| i.max_texture_side) as u32;
        // A decoder bug on a bad file must not take the thread down.
        let decoded = std::panic::catch_unwind(|| decode(&job.path, MAX_DIMENSION.min(max_side)))
            .unwrap_or_else(|_| Err("the image decoder crashed on this file".into()));
        if !still_wanted(&shared, &job) {
            continue;
        }
        let built = match decoded {
            Ok(image) => {
                let bytes = image.width() * image.height() * 4;
                let size = vec2(image.width() as f32, image.height() as f32);
                let name = job.path.display().to_string();
                let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
                Ok((texture, size, bytes))
            }
            Err(e) => Err(e),
        };
        let wake = publish(&shared, &job, built);
        if wake {
            ctx.request_repaint();
        }
    }
}

/// The next job whose tab is still waiting, or `None` when the cache is gone.
fn next_job(shared: &Shared) -> Option<Job> {
    let mut queue = shared.queue.lock().expect("image queue poisoned");
    loop {
        if shared.closed.load(Ordering::Acquire) {
            return None;
        }
        if shared.paused.load(Ordering::Acquire) {
            let (guard, _) = shared
                .cv
                .wait_timeout(queue, std::time::Duration::from_millis(2))
                .expect("image queue poisoned");
            queue = guard;
            continue;
        }
        if let Some(job) = queue.pop_front() {
            return Some(job);
        }
        if shared.closed.load(Ordering::Acquire) {
            return None;
        }
        queue = shared.cv.wait(queue).expect("image queue poisoned");
    }
}

fn still_wanted(shared: &Shared, job: &Job) -> bool {
    let inner = shared.inner.lock().expect("image cache poisoned");
    matches!(
        inner.map.get(&job.path),
        Some(entry) if entry.stamp == job.stamp && matches!(entry.state, State::Loading)
    )
}

/// Stores a finished decode. Returns whether the tab on screen is waiting
/// for it. A hidden tab's completion does not ask for a frame, and neither
/// does a job whose tab closed while it was decoding.
fn publish(
    shared: &Shared,
    job: &Job,
    built: Result<(TextureHandle, Vec2, usize), String>,
) -> bool {
    let mut inner = shared.inner.lock().expect("image cache poisoned");
    let Some(entry) = inner.map.get_mut(&job.path) else {
        return false;
    };
    if entry.stamp != job.stamp || !matches!(entry.state, State::Loading) {
        return false;
    }
    let active = shared.active.load(Ordering::Acquire);
    let wake = entry.users.contains(&active);
    let (bytes, ready) = match built {
        Ok((texture, size, bytes)) => {
            entry.state = State::Ready(texture, size);
            entry.bytes = bytes;
            (bytes, true)
        }
        Err(error) => {
            let bytes = error.len();
            entry.bytes = bytes;
            entry.state = State::Failed(error);
            (bytes, false)
        }
    };
    inner.bytes += bytes;
    if ready {
        evict(&mut inner, &job.path, active);
    }
    wake
}

/// Drops the least recently shown images until `budget`. The image just
/// decoded stays, and so does anything still loading. An image the tab on
/// screen is using is dropped only after the hidden ones.
fn evict(inner: &mut Inner, protect: &Path, active: u64) {
    while inner.bytes > inner.budget {
        let victim = inner
            .map
            .iter()
            .filter(|(path, entry)| *path != protect && !matches!(entry.state, State::Loading))
            .min_by_key(|(_, entry)| (u8::from(entry.users.contains(&active)), entry.used))
            .map(|(path, _)| path.clone());
        let Some(path) = victim else {
            break;
        };
        if let Some(entry) = inner.map.remove(&path) {
            inner.bytes = inner.bytes.saturating_sub(entry.bytes);
        }
    }
}

/// Canonical path when the file exists, so two links to one file share an
/// entry. Otherwise a lexical absolute path, with `.` and `..` collapsed.
fn identify(path: PathBuf) -> (PathBuf, Stamp) {
    match std::fs::canonicalize(&path) {
        Ok(canon) => {
            let stamp = stamp_of(&canon);
            (canon, stamp)
        }
        Err(_) => {
            let path = lexical_normal(&path);
            let stamp = stamp_of(&path);
            (path, stamp)
        }
    }
}

fn stamp_of(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        Ok(meta) => Stamp {
            modified: meta.modified().ok(),
            len: meta.len(),
        },
        Err(_) => Stamp::missing(),
    }
}

fn lexical_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` stays at the root. A relative path may still climb.
                Some(Component::RootDir) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out
}

/// A local path for `dest`, or `None` for remote URLs.
pub(crate) fn resolve(dest: &str, base: Option<&Path>) -> Option<PathBuf> {
    let dest = dest.trim();
    let path = match dest.strip_prefix("file://") {
        Some(rest) => rest,
        None if dest.contains("://") || dest.starts_with("data:") => return None,
        None => dest,
    };
    let path = PathBuf::from(percent_decode(path));
    Some(match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path,
    })
}

/// Decodes `%XX` escapes (e.g. `%20`); anything malformed is kept as is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len() + 1
            && let Some(byte) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

fn decode(path: &Path, max_side: u32) -> Result<ColorImage, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "file not found".to_owned(),
            _ => e.to_string(),
        })?
        .len();
    if len > MAX_FILE_BYTES {
        return Err(format!("{} MB is too large to show", len / (1024 * 1024)));
    }
    if is_svg(path) {
        return decode_svg(path, max_side);
    }
    let image = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let image = if image.width() > max_side || image.height() > max_side {
        image.resize(max_side, max_side, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// SVGs go by extension (`.svg`, `.svgz`); the raster decoder guesses
/// from the bytes.
fn is_svg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
}

/// Renders an SVG at its own size (scaled down to `max_side`), with text
/// in the system's fonts. It can't reach other files: `<image href>`
/// paths aren't loaded (an SVG in a note mustn't read whatever the user can),
/// and embedded raster images aren't drawn. `.svgz` is inflated here, with
/// the same size cap as other files.
fn decode_svg(path: &Path, max_side: u32) -> Result<ColorImage, String> {
    use resvg::{tiny_skia, usvg};

    // Loading the system fonts is slow; once per run is enough.
    static FONTS: std::sync::OnceLock<Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    let fontdb = FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone();
    let mut data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut inflated = Vec::new();
        flate2::read::GzDecoder::new(data.as_slice())
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut inflated)
            .map_err(|e| format!("not a valid .svgz: {e}"))?;
        if inflated.len() as u64 > MAX_FILE_BYTES {
            return Err("the SVG is too large once unpacked".into());
        }
        data = inflated;
    }
    let options = usvg::Options {
        fontdb,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    let tree =
        usvg::Tree::from_data(&data, &options).map_err(|e| format!("not a valid SVG: {e}"))?;
    let size = tree.size();
    let scale = (max_side as f32 / size.width().max(size.height())).min(1.0);
    let (w, h) = (
        (size.width() * scale).ceil().max(1.0) as u32,
        (size.height() * scale).ceil().max(1.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("the SVG has no size")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(ColorImage::from_rgba_premultiplied(
        [w as usize, h as usize],
        pixmap.data(),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn resolves_relative_file_and_remote_destinations() {
        let base = Path::new("/notes");
        assert_eq!(
            resolve("img/a.png", Some(base)),
            Some(PathBuf::from("/notes/img/a.png"))
        );
        assert_eq!(
            resolve("/abs/b.png", Some(base)),
            Some(PathBuf::from("/abs/b.png"))
        );
        assert_eq!(
            resolve("file:///abs/c%20d.png", Some(base)),
            Some(PathBuf::from("/abs/c d.png"))
        );
        assert_eq!(resolve("https://example.com/x.png", Some(base)), None);
        assert_eq!(resolve("data:image/png;base64,AAAA", Some(base)), None);
        assert_eq!(percent_decode("100%"), "100%");
    }

    fn wait_for(cache: &ImageCache, dest: &str, base: &Path) -> ImageSlot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let slot = cache.get(dest, Some(base), 1);
            if !matches!(slot, ImageSlot::Loading) || Instant::now() > deadline {
                return slot;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_clone_is_the_same_cache() {
        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        let other = cache.clone();
        assert!(matches!(
            cache.get("note.png", Some(Path::new("/notes")), 1),
            ImageSlot::Loading
        ));
        assert_eq!(other.stored(), 1);
        // The worker may already have failed the missing file. Either way
        // the second view must not insert its own entry.
        assert!(matches!(
            other.get("note.png", Some(Path::new("/notes")), 1),
            ImageSlot::Loading | ImageSlot::Failed(_)
        ));
        assert_eq!(cache.stored(), 1);
    }

    #[test]
    fn loads_scales_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 0, 255]))
            .save(dir.path().join("small.png"))
            .unwrap();
        image::RgbaImage::new(MAX_DIMENSION * 2, 10)
            .save(dir.path().join("wide.png"))
            .unwrap();
        std::fs::write(dir.path().join("broken.png"), b"not a png").unwrap();

        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        match wait_for(&cache, "small.png", dir.path()) {
            ImageSlot::Ready { size, .. } => assert_eq!(size, vec2(40.0, 20.0)),
            _ => panic!("small.png didn't load"),
        }
        let max_side = ctx
            .input(|i| i.max_texture_side)
            .min(MAX_DIMENSION as usize);
        match wait_for(&cache, "wide.png", dir.path()) {
            ImageSlot::Ready { size, .. } => assert_eq!(size.x, max_side as f32),
            _ => panic!("wide.png didn't load"),
        }
        assert!(matches!(
            wait_for(&cache, "broken.png", dir.path()),
            ImageSlot::Failed(_)
        ));
        assert!(matches!(
            wait_for(&cache, "missing.png", dir.path()),
            ImageSlot::Failed(_)
        ));
        assert!(matches!(
            cache.get("https://x/y.png", Some(dir.path()), 1),
            ImageSlot::Remote
        ));
    }

    #[test]
    fn svgs_render_at_their_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.svg");
        std::fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="20" height="20" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let image = decode(&path, 4096).unwrap();
        assert_eq!(image.size, [40, 20]);
        assert_eq!(
            image.pixels[0],
            egui::Color32::from_rgb(255, 0, 0),
            "the rect is drawn"
        );
        assert_eq!(image.pixels[39].a(), 0, "the rest is transparent");
        // Scaled down to fit.
        assert_eq!(decode(&path, 10).unwrap().size, [10, 5]);
        // Not SVG at all.
        std::fs::write(&path, "<html>nope</html>").unwrap();
        assert!(decode(&path, 4096).unwrap_err().contains("SVG"));
    }

    #[test]
    fn an_svg_cant_pull_in_other_files() {
        // Review of #33: `<image href>` paths aren't read.
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.svg");
        std::fs::write(
            &other,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#00ff00"/></svg>"##,
        )
        .unwrap();
        let path = dir.path().join("outer.svg");
        for href in ["other.svg", &other.display().to_string(), "../other.svg"] {
            std::fs::write(
                &path,
                format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><image width="10" height="10" xlink:href="{href}"/></svg>"##
                ),
            )
            .unwrap();
            let image = decode(&path, 4096).unwrap();
            assert_eq!(image.pixels[0].a(), 0, "{href} was loaded");
        }
    }

    #[test]
    fn svgz_is_inflated() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.svgz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#0000ff"/></svg>"##,
        )
        .unwrap();
        std::fs::write(&path, gz.finish().unwrap()).unwrap();
        let image = decode(&path, 4096).unwrap();
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(0, 0, 255));
    }

    fn save_png(path: &Path, w: u32, h: u32) {
        image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn a_link_to_the_same_file_is_not_decoded_twice() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.png");
        save_png(&real, 4, 4);
        std::os::unix::fs::symlink("real.png", dir.path().join("alias.png")).unwrap();
        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        cache.pause(true);
        cache.set_active(1);
        assert!(matches!(
            cache.get("real.png", Some(dir.path()), 1),
            ImageSlot::Loading
        ));
        assert!(matches!(
            cache.get("alias.png", Some(dir.path()), 1),
            ImageSlot::Loading
        ));
        assert_eq!(cache.stored(), 1);
        assert_eq!(cache.queued().len(), 1);
    }

    #[test]
    fn a_replaced_file_and_a_file_that_appears_are_decoded_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic.png");
        save_png(&path, 40, 20);
        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        cache.set_active(1);
        match wait_for(&cache, "pic.png", dir.path()) {
            ImageSlot::Ready { size, .. } => assert_eq!(size, vec2(40.0, 20.0)),
            _ => panic!("pic.png didn't load"),
        }
        save_png(&path, 8, 8);
        match wait_for(&cache, "pic.png", dir.path()) {
            ImageSlot::Ready { size, .. } => assert_eq!(size, vec2(8.0, 8.0)),
            _ => panic!("the replaced file kept the old image"),
        }

        assert!(matches!(
            wait_for(&cache, "later.png", dir.path()),
            ImageSlot::Failed(_)
        ));
        save_png(&dir.path().join("later.png"), 4, 4);
        match wait_for(&cache, "later.png", dir.path()) {
            ImageSlot::Ready { size, .. } => assert_eq!(size, vec2(4.0, 4.0)),
            _ => panic!("a file that appeared stayed failed"),
        }
    }

    #[test]
    fn closing_a_tab_drops_its_image_and_a_full_cache_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        save_png(&dir.path().join("a.png"), 40, 20);
        save_png(&dir.path().join("b.png"), 40, 20);
        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        cache.set_active(1);
        // 40×20 is 3200 bytes. Two of them do not fit.
        cache.set_budget(4000);
        assert!(matches!(
            wait_for(&cache, "a.png", dir.path()),
            ImageSlot::Ready { .. }
        ));
        cache.release_user(1);
        assert_eq!(cache.stored(), 0, "closing the tab kept the texture");

        cache.set_active(1);
        assert!(matches!(
            wait_for(&cache, "a.png", dir.path()),
            ImageSlot::Ready { .. }
        ));
        assert!(matches!(
            wait_for(&cache, "b.png", dir.path()),
            ImageSlot::Ready { .. }
        ));
        assert_eq!(cache.stored(), 1, "the budget kept both textures");
        assert!(cache.resident_bytes() <= 4000);
        assert!(matches!(
            cache.get("b.png", Some(dir.path()), 1),
            ImageSlot::Ready { .. }
        ));
        assert!(
            !matches!(
                cache.get("a.png", Some(dir.path()), 1),
                ImageSlot::Ready { .. }
            ),
            "the image on screen was the one dropped"
        );
    }

    #[test]
    fn a_hidden_tab_does_not_stay_ahead_of_the_tab_on_screen() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cache = ImageCache::new(&ctx);
        cache.pause(true);
        cache.set_active(1);
        cache.get("a.png", Some(dir.path()), 1);
        cache.get("b.png", Some(dir.path()), 1);
        cache.set_active(2);
        cache.get("c.png", Some(dir.path()), 2);
        let queued = cache.queued();
        assert_eq!(queued.len(), 3);
        assert!(queued[0].ends_with("c.png"), "the tab on screen waited");
        cache.release_user(1);
        let queued = cache.queued();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].ends_with("c.png"));
        assert_eq!(cache.stored(), 1);
    }
}
