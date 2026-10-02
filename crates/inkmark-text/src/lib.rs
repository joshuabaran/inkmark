//! cosmic-text layout, swash glyph atlas, Mesh building, HeightCache.

mod atlas;
mod geometry;
mod heights;
mod packer;
mod renderer;

pub use atlas::{AtlasGlyph, AtlasStats, GlyphAtlas};
pub use geometry::{ClusterSpan, LineGeometry, Row};
pub use heights::{HeightCache, ScrollAnchor};
pub use renderer::{
    FontStyle, Fonts, GlyphMeshes, RichLine, SharedFonts, TextConfig, TextRenderer,
};
