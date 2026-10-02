//! Images for the live view, decoded off the UI thread and kept as textures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use egui::{ColorImage, TextureHandle, TextureOptions, Vec2, vec2};

/// Larger images are scaled down to this many pixels on their longest side
/// (or the GPU's texture limit, if smaller).
const MAX_DIMENSION: u32 = 4096;
/// Files bigger than this aren't decoded.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

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

pub(crate) struct ImageCache {
    states: Arc<Mutex<HashMap<PathBuf, State>>>,
    jobs: Sender<PathBuf>,
}

impl ImageCache {
    pub fn new(ctx: &egui::Context) -> Self {
        let states: Arc<Mutex<HashMap<PathBuf, State>>> = Arc::default();
        let (jobs, rx) = mpsc::channel::<PathBuf>();
        let worker_states = Arc::clone(&states);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("inkmark-images".into())
            .spawn(move || {
                while let Ok(path) = rx.recv() {
                    let max_side = ctx.input(|i| i.max_texture_side) as u32;
                    // A decoder bug on a bad file must not take the thread down.
                    let decoded =
                        std::panic::catch_unwind(|| decode(&path, MAX_DIMENSION.min(max_side)))
                            .unwrap_or_else(|_| {
                                Err("the image decoder crashed on this file".into())
                            });
                    let state = match decoded {
                        Ok(image) => {
                            let size = vec2(image.width() as f32, image.height() as f32);
                            let name = path.display().to_string();
                            State::Ready(
                                ctx.load_texture(name, image, TextureOptions::LINEAR),
                                size,
                            )
                        }
                        Err(e) => State::Failed(e),
                    };
                    worker_states
                        .lock()
                        .expect("image cache poisoned")
                        .insert(path, state);
                    ctx.request_repaint();
                }
            })
            .expect("spawn image thread");
        Self { states, jobs }
    }

    /// The image at `dest` (as written in the Markdown), relative to `base`
    /// (the document's folder). The first request starts loading it.
    pub fn get(&self, dest: &str, base: Option<&Path>) -> ImageSlot {
        let Some(path) = resolve(dest, base) else {
            return ImageSlot::Remote;
        };
        let mut states = self.states.lock().expect("image cache poisoned");
        match states.get(&path) {
            Some(State::Loading) => ImageSlot::Loading,
            Some(State::Ready(texture, size)) => ImageSlot::Ready {
                texture: texture.clone(),
                size: *size,
            },
            Some(State::Failed(e)) => ImageSlot::Failed(e.clone()),
            None => {
                states.insert(path.clone(), State::Loading);
                let _ = self.jobs.send(path);
                ImageSlot::Loading
            }
        }
    }
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
            let slot = cache.get(dest, Some(base));
            if !matches!(slot, ImageSlot::Loading) || Instant::now() > deadline {
                return slot;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
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
            cache.get("https://x/y.png", Some(dir.path())),
            ImageSlot::Remote
        ));
    }
}
