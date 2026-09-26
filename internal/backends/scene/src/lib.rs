// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// SceneFrame / DrawCommand types shared by every 2-thread backend.
//
// A `SceneFrame` is a plain, `Send` descriptor of one rendered frame: a list
// of `DrawCommand`s (physical-pixel drawing primitives), the font payloads
// its glyph runs reference, and the control regions for hit-testing.  The
// UI thread serialises its item tree into one via the snapshot encoder
// (`snapshot`); the render thread replays the commands against whatever
// renderer owns the window surface.  The winit backend replays them onto
// FemtoVG/GL; a platform whose renderer can draw into the window buffer
// directly can present from its own render thread without this protocol.

use i_slint_core::graphics::Color;
use i_slint_core::graphics::euclid;
use i_slint_core::lengths::{LogicalRect, PhysicalBorderRadius, PhysicalPx};

pub mod snapshot;

/// Physical-pixel geometry aliases (documents that all command payloads are
/// in physical pixels, matching femtovg's coordinate space).
pub type PhysicalLength = euclid::Length<f32, PhysicalPx>;
pub type PhysicalPoint = euclid::Point2D<f32, PhysicalPx>;
pub type PhysicalRect = euclid::Rect<f32, PhysicalPx>;

/// A value to assign to a borrowed (lent-out) control property.  The render
/// thread converts it to the concrete Slint property type (upstream
/// semantics, including detaching a previous binding) on the mirror tree.
#[derive(Clone, Debug)]
pub enum ControlPropertyValue {
    /// For `Text.text`, `TextInput.text`, ...
    Text(String),
    /// RGBA, for `Rectangle.background`, `Text.color`, ...
    Color { r: u8, g: u8, b: u8, a: u8 },
    /// For `TouchArea.enabled` / `pressed` / `has-hover`, ...
    Bool(bool),
    /// For numeric properties (`opacity`, `width`, ...).
    Number(f32),
}

/// A serialisable paint description, equivalent to femtovg::Paint.
#[derive(Clone, Debug)]
pub enum PaintDesc {
    /// Solid color fill.
    Solid { r: u8, g: u8, b: u8, a: u8 },
    /// Linear gradient.
    LinearGradient { start_x: f32, start_y: f32, end_x: f32, end_y: f32, stops: Vec<GradientStop> },
    /// Radial gradient.
    RadialGradient { cx: f32, cy: f32, radius: f32, stops: Vec<GradientStop> },
    /// Texture-mapped paint (image blit).
    ImagePaint {
        texture_key: u64,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        tex_x: f32,
        tex_y: f32,
        tex_w: f32,
        tex_h: f32,
        flags: u32,
    },
}

/// A single gradient colour stop.
#[derive(Clone, Debug)]
pub struct GradientStop {
    pub offset: f32,
    pub color: [u8; 4], // RGBA
}

/// One segment of a vector path.
#[derive(Clone, Debug)]
pub enum PathEvent {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

/// A positioned glyph for cross-thread text rendering.
#[derive(Clone, Debug)]
pub struct PositionedGlyph {
    pub x: f32,
    pub y: f32,
    pub id: u16,
}

/// Line cap style for strokes.
#[derive(Clone, Copy, Debug)]
pub enum LineCapDesc {
    Butt,
    Round,
    Square,
}

/// Line join style for strokes.
#[derive(Clone, Copy, Debug)]
pub enum LineJoinDesc {
    Miter,
    Round,
    Bevel,
}

/// Mirrors a control's screen region for the render thread's coordinate table.
#[derive(Clone, Debug)]
pub struct ControlRegion {
    /// Opaque handle combining component pointer + item index.
    pub id: u64,
    pub geometry: LogicalRect,
}

/// Logical-coordinate entry in the shared coordinate table.  Published by the
/// render thread as it composites the retained scene; read by the UI thread
/// for hit-testing during event processing (the pointer position is only
/// available on the UI thread, so the map never blocks the render thread).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ControlCoord {
    /// Control's top-left corner in logical pixels.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Current pointer-interaction state, updated by the render thread when
    /// it owns the widget state (see the event-protocol design).
    pub hovered: bool,
    pub pressed: bool,
}

impl ControlCoord {
    /// True when the logical point is inside this control's rectangle.
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// Shared, thread-safe table of control coordinates.  The render thread writes
/// (publish on composite); the UI thread and workers read (hit-test).  Access
/// via the backend's `coordinate_map()` accessor.
pub type CoordinateMap = std::collections::HashMap<u64, ControlCoord>;

/// Complete serialisable scene for one frame.
#[derive(Clone, Debug)]
pub struct SceneFrame {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    /// Window background as RGBA bytes; cleared before the commands replay.
    /// `None` means the UI thread did not see a solid brush and the commands
    /// carry the full background instead.
    pub background: Option<[u8; 4]>,
    /// Deduplicated font payloads referenced by the glyph runs below. The
    /// `blob_id` is `Blob::id()` of the parley font data, stable across
    /// frames, so the render thread can cache font ids without re-hashing the
    /// font bytes.
    pub fonts: Vec<SceneFont>,
    pub commands: Vec<DrawCommand>,
    pub controls: Vec<ControlRegion>,
}

/// One unique font payload serialised for a frame.
#[derive(Clone, Debug)]
pub struct SceneFont {
    /// Stable `Blob::id()` of the parley font data.
    pub blob_id: u64,
    /// Index of the font in a collection, or 0 for a single font.
    pub font_index: u32,
    pub data: Vec<u8>,
}

/// A compositing layer drawn on top of the UI scene, submitted from any
/// thread without going through the UI thread.
///
/// Commands use the same physical-pixel coordinate space as
/// [`SceneFrame::commands`].  Submitting a new frame replaces the previous
/// overlay; an empty command list removes it.  Whenever such an overlay is
/// submitted (or the window is resized) while the UI thread is busy, the
/// render thread re-composites the last retained UI scene with the overlay
/// and presents it, so drawing does not stall behind the UI thread.
#[derive(Clone, Debug, Default)]
pub struct OverlayFrame {
    /// Font payloads referenced by the glyph runs in `commands`.
    pub fonts: Vec<SceneFont>,
    pub commands: Vec<DrawCommand>,
}

/// A single draw command in the serialised scene.
#[derive(Clone, Debug)]
pub enum DrawCommand {
    // -- Canvas state --
    Save,
    Restore,
    Translate(f32, f32),
    Rotate(f32),
    Scale(f32, f32),
    /// Directly set the global alpha (used after accumulation).
    SetGlobalAlpha(f32),
    /// Intersection clip rect (physical coordinates).
    CombineClip(PhysicalRect),

    // -- Primitives --
    /// Fill a rectangle with a solid or gradient paint (no border radius).
    FillRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        anti_alias: bool,
    },
    /// Fill a rounded rectangle (background).
    FillRoundedRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: PhysicalBorderRadius,
        anti_alias: bool,
    },
    /// Stroke a rounded rectangle (border stroke for buttons / text boxes).
    StrokeRoundedRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: PhysicalBorderRadius,
        line_width: f32,
        anti_alias: bool,
    },
    /// Stroke a path (border rectangle).
    StrokePath {
        path: Vec<PathEvent>,
        paint: PaintDesc,
        line_width: f32,
        line_cap: LineCapDesc,
        line_join: LineJoinDesc,
        miter_limit: f32,
        anti_alias: bool,
    },
    /// Fill a path (rounded rect background or explicit path).
    FillPath {
        path: Vec<PathEvent>,
        paint: PaintDesc,
        fill_rule: u8,
        anti_alias: bool,
    },

    // -- Text (glyph-based) --
    /// Draw a glyph run produced by sharedparley on the UI thread. The font
    /// payload is carried once per frame in `SceneFrame::fonts`, referenced
    /// here by its stable `Blob::id()`.
    DrawGlyphRun {
        font_blob_id: u64,
        font_index: u32,
        font_size: f32,
        normalized_coords: Vec<i16>,
        paint: PaintDesc,
        y_offset: f32,
        glyphs: Vec<PositionedGlyph>,
        is_stroke: bool,
    },
    /// Fill a rectangle (underline/strikethrough/cursor) during text drawing.
    FillTextRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: f32,
        border: Option<(PaintDesc, f32)>,
    },
    /// Draw a text string that the render thread shapes itself with its own
    /// parley context (system fonts, no UI-thread involvement).  `x`/`y` is
    /// the baseline origin in physical pixels; `font_size` is in physical
    /// pixels; `max_width` wraps the line at the given physical-pixel width
    /// (`None` = single line).  Meant for overlay drawing driven by any app
    /// thread: only the string and geometry travel over the wire.
    DrawText {
        x: f32,
        y: f32,
        text: String,
        font_size: f32,
        paint: PaintDesc,
        max_width: Option<f32>,
    },

    // -- Images / pixmaps --
    /// Upload a raw RGBA8 pixel buffer to the render thread's texture cache.
    UploadPixmap {
        key: u64,
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Draw a previously-uploaded pixmap at the current canvas position.
    BlitPixmap {
        key: u64,
        /// f32 x, y, w, h, tex_x, tex_y, tex_w, tex_h, flags
        params: [f32; 9],
    },

    // -- Layer (offscreen compositing for opacity / clip-with-radius / layer hint) --
    Layer {
        width: u32,
        height: u32,
        /// Blit origin in physical pixels.
        origin: PhysicalPoint,
        alpha_tint: f32,
        commands: Vec<DrawCommand>,
    },

    // -- Box shadow --
    /// Render a drop-shadow: the render thread creates a small texture, fills
    /// a rounded rect, blurs, and composites.  All params in physical pixels.
    DrawBoxShadow {
        color: Color,
        blur: f32,
        offset_x: f32,
        offset_y: f32,
        width: f32,
        height: f32,
        radius: PhysicalBorderRadius,
    },

    // -- Cached pixmap (from custom widget painting) --
    CachedPixmap {
        key: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}
