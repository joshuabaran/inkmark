use std::collections::HashMap;

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use egui::epaint::FontColorTransferFunction;
use egui::{Color32, ColorImage, Context, Rect, TextureHandle, TextureId, TextureOptions, pos2};

use crate::packer::ShelfPacker;

const PAGE_SIZE: u32 = 1024;
/// Gap between glyphs so a rounding slip never samples a neighbour.
const PADDING: u32 = 1;
/// Pages allowed before the atlas clears itself at the next frame boundary.
const MAX_PAGES: usize = 4;
/// Quads are pixel-snapped, so texels map 1:1 to screen pixels.
const TEXTURE_OPTIONS: TextureOptions = TextureOptions::NEAREST;

/// Where a rasterized glyph lives in the atlas, in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct AtlasGlyph {
    pub page: usize,
    pub uv: Rect,
    pub size: [u32; 2],
    /// Offset from the pen position to the bitmap's left edge.
    pub left: i32,
    /// Offset from the baseline up to the bitmap's top edge.
    pub top: i32,
    /// Color bitmaps (emoji) are drawn untinted.
    pub is_color: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AtlasStats {
    pub pages: usize,
    pub glyphs: usize,
    pub rasterized_last_frame: usize,
    pub resets: usize,
}

struct Page {
    packer: ShelfPacker,
    image: ColorImage,
    /// `[min_x, min_y, max_x, max_y]` of pixels not yet uploaded.
    dirty: Option<[u32; 4]>,
    texture: TextureHandle,
}

impl Page {
    fn new(ctx: &Context, index: usize) -> Self {
        let size = PAGE_SIZE as usize;
        let image = ColorImage::filled([size, size], Color32::TRANSPARENT);
        let texture = ctx.load_texture(
            format!("inkmark-glyphs-{index}"),
            image.clone(),
            TEXTURE_OPTIONS,
        );
        Self {
            packer: ShelfPacker::new(PAGE_SIZE),
            image,
            dirty: None,
            texture,
        }
    }

    fn mark_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let rect = [x, y, x + w, y + h];
        self.dirty = Some(match self.dirty {
            None => rect,
            Some(d) => [
                d[0].min(rect[0]),
                d[1].min(rect[1]),
                d[2].max(rect[2]),
                d[3].max(rect[3]),
            ],
        });
    }
}

/// Glyph cache backed by egui textures: swash rasterizes, we pack and upload.
///
/// Per frame: [`begin_frame`](Self::begin_frame), any number of
/// [`get`](Self::get), then [`upload`](Self::upload) before painting.
pub struct GlyphAtlas {
    ctx: Context,
    swash: SwashCache,
    glyphs: HashMap<CacheKey, Option<AtlasGlyph>>,
    pages: Vec<Page>,
    /// Coverage → texel color for mask glyphs.
    coverage_lut: [Color32; 256],
    reset_pending: bool,
    stats: AtlasStats,
}

impl GlyphAtlas {
    pub fn new(ctx: &Context) -> Self {
        let transfer = FontColorTransferFunction::DARK_MODE_DEFAULT;
        Self {
            ctx: ctx.clone(),
            swash: SwashCache::new(),
            glyphs: HashMap::new(),
            pages: Vec::new(),
            coverage_lut: std::array::from_fn(|c| transfer.color_from_coverage(c as f32 / 255.0)),
            reset_pending: false,
            stats: AtlasStats::default(),
        }
    }

    /// Clears the atlas if it overflowed last frame. Must run before any `get` in a frame,
    /// because clearing invalidates every `AtlasGlyph` handed out.
    pub fn begin_frame(&mut self) {
        self.stats.rasterized_last_frame = 0;
        if !self.reset_pending {
            return;
        }
        self.reset_pending = false;
        self.stats.resets += 1;
        self.glyphs.clear();
        self.pages.truncate(MAX_PAGES);
        for page in &mut self.pages {
            page.packer.clear();
            page.image.pixels.fill(Color32::TRANSPARENT);
            page.mark_dirty(0, 0, PAGE_SIZE, PAGE_SIZE);
        }
    }

    pub fn get(&mut self, font_system: &mut FontSystem, key: CacheKey) -> Option<AtlasGlyph> {
        if let Some(glyph) = self.glyphs.get(&key) {
            return *glyph;
        }
        let glyph = self.rasterize(font_system, key);
        self.glyphs.insert(key, glyph);
        glyph
    }

    pub fn texture_id(&self, page: usize) -> TextureId {
        self.pages[page].texture.id()
    }

    pub fn stats(&self) -> AtlasStats {
        AtlasStats {
            pages: self.pages.len(),
            glyphs: self.glyphs.len(),
            ..self.stats
        }
    }

    /// Sends pixels written since the last upload to the GPU, one region per page.
    pub fn upload(&mut self) {
        for page in &mut self.pages {
            let Some([x0, y0, x1, y1]) = page.dirty.take() else {
                continue;
            };
            let (pos, size) = (
                [x0 as usize, y0 as usize],
                [(x1 - x0) as usize, (y1 - y0) as usize],
            );
            let region = page.image.region_by_pixels(pos, size);
            page.texture.set_partial(pos, region, TEXTURE_OPTIONS);
        }
    }

    fn rasterize(&mut self, font_system: &mut FontSystem, key: CacheKey) -> Option<AtlasGlyph> {
        let image = self.swash.get_image_uncached(font_system, key)?;
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None;
        }
        let (page_index, [x, y]) = self.alloc(w + PADDING, h + PADDING)?;
        self.stats.rasterized_last_frame += 1;

        let lut = &self.coverage_lut;
        let page = &mut self.pages[page_index];
        let stride = PAGE_SIZE as usize;
        for row in 0..h as usize {
            let dst = (y as usize + row) * stride + x as usize;
            let dst = &mut page.image.pixels[dst..dst + w as usize];
            match image.content {
                SwashContent::Mask => {
                    let src = &image.data[row * w as usize..][..w as usize];
                    for (px, &c) in dst.iter_mut().zip(src) {
                        *px = lut[c as usize];
                    }
                }
                SwashContent::SubpixelMask => {
                    let src = &image.data[row * w as usize * 4..][..w as usize * 4];
                    for (px, rgba) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                        *px = lut[rgba[0].max(rgba[1]).max(rgba[2]) as usize];
                    }
                }
                SwashContent::Color => {
                    let src = &image.data[row * w as usize * 4..][..w as usize * 4];
                    for (px, rgba) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                        *px = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                    }
                }
            }
        }
        page.mark_dirty(x, y, w, h);

        let scale = 1.0 / PAGE_SIZE as f32;
        Some(AtlasGlyph {
            page: page_index,
            uv: Rect::from_min_max(
                pos2(x as f32 * scale, y as f32 * scale),
                pos2((x + w) as f32 * scale, (y + h) as f32 * scale),
            ),
            size: [w, h],
            left: image.placement.left,
            top: image.placement.top,
            is_color: image.content == SwashContent::Color,
        })
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(usize, [u32; 2])> {
        for (i, page) in self.pages.iter_mut().enumerate() {
            if let Some(pos) = page.packer.alloc(w, h) {
                return Some((i, pos));
            }
        }
        if w > PAGE_SIZE || h > PAGE_SIZE {
            return None;
        }
        // Over budget: keep this frame correct with an extra page, clear next frame.
        if self.pages.len() >= MAX_PAGES {
            self.reset_pending = true;
        }
        let index = self.pages.len();
        self.pages.push(Page::new(&self.ctx, index));
        let pos = self.pages[index].packer.alloc(w, h)?;
        Some((index, pos))
    }
}

#[cfg(test)]
mod tests {
    use cosmic_text::{CacheKeyFlags, fontdb};

    use super::*;

    #[test]
    fn overflow_clears_at_next_frame_boundary() {
        let ctx = Context::default();
        let mut font_system = FontSystem::new();
        let font_id = font_system
            .db()
            .faces()
            .map(|f| f.id)
            .next()
            .expect("tests need at least one system font");
        let mut atlas = GlyphAtlas::new(&ctx);
        atlas.begin_frame();

        // Huge glyphs fill pages fast; keep going until the atlas goes over budget.
        let mut glyph_id = 1;
        while atlas.stats().pages <= MAX_PAGES && glyph_id < 2000 {
            let (key, _, _) = CacheKey::new(
                font_id,
                glyph_id,
                300.0,
                (0.0, 0.0),
                fontdb::Weight::NORMAL,
                CacheKeyFlags::empty(),
            );
            atlas.get(&mut font_system, key);
            glyph_id += 1;
        }
        let before = atlas.stats();
        assert!(
            before.pages > MAX_PAGES,
            "font has too few glyphs to overflow"
        );

        atlas.upload();
        atlas.begin_frame();
        let after = atlas.stats();
        assert_eq!(after.glyphs, 0);
        assert_eq!(after.pages, MAX_PAGES);
        assert_eq!(after.resets, 1);
    }
}
