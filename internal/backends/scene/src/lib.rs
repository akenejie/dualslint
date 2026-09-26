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

/// The control geometry and interaction state that the render thread published
/// for the frame it last composited.
///
/// The ids are kept in paint order as well as by id, because a map has no
/// order and an overlapping set of controls needs one: the control drawn last is
/// the one on top, so it is the one a pointer in the overlap belongs to.
#[derive(Default)]
pub struct PublishedControls {
    /// Geometry and state by control id.
    pub by_id: CoordinateMap,
    order: Vec<u64>,
}

impl PublishedControls {
    pub fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
    }

    /// Record a control, appending it to the paint order.
    pub fn insert(&mut self, id: u64, coord: ControlCoord) {
        if self.by_id.insert(id, coord).is_none() {
            self.order.push(id);
        }
    }

    /// The topmost control containing the logical point, if any.
    pub fn control_at(&self, x: f32, y: f32) -> Option<u64> {
        self.order
            .iter()
            .rev()
            .find(|id| self.by_id.get(id).is_some_and(|c| c.contains(x, y)))
            .copied()
    }

    /// The ids in paint order, back to front.
    pub fn ids(&self) -> &[u64] {
        &self.order
    }
}

/// What the operating system reported about the pointer.
///
/// The UI thread is the one that receives these; the render thread never sees
/// them, only the state that follows from them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerPhase {
    Moved,
    Pressed,
    Released,
    /// The pointer left the window, so nothing is hovered any more.
    Exited,
}

/// The pointer state the UI thread believes the render-owned controls are in.
///
/// The UI thread keeps this because the pointer position only exists there, and
/// the control geometry only exists on the render thread: resolving the two is
/// the UI thread's job, applying the result is the render thread's.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerState {
    /// The control the pointer is over.
    pub hover: Option<u64>,
    /// The control holding the press.
    pub press: Option<u64>,
}

/// One control's resulting state, to be applied on the render thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ControlStateUpdate {
    pub id: u64,
    pub hovered: bool,
    pub pressed: bool,
}

impl PointerState {
    /// Fold one OS pointer event into this state and report the controls whose
    /// state it changed, as `(id, hovered, pressed)` triples.
    ///
    /// `target` is the control under the pointer according to the geometry the
    /// render thread last published, or `None` when the pointer is over nothing
    /// (including after [`PointerPhase::Exited`]).
    ///
    /// A press stays with the control it started on: dragging out of a button
    /// must not move the press to whatever is under the pointer, and dragging
    /// back must not start one.  Only the phase decides that, never the
    /// position, so the outcome does not depend on which pointer moves happened
    /// to be delivered.
    pub fn apply(&mut self, phase: PointerPhase, target: Option<u64>) -> Vec<ControlStateUpdate> {
        let previous_hover = self.hover;
        let previous_press = self.press;
        self.hover = match phase {
            PointerPhase::Exited => None,
            _ => target,
        };
        self.press = match phase {
            PointerPhase::Pressed => target,
            PointerPhase::Released | PointerPhase::Exited => None,
            PointerPhase::Moved => previous_press,
        };

        // Every control this event could have changed the state of is reported,
        // each once and with both of its flags, so the render thread never has
        // to merge a partial update with what it already applied.  A control
        // whose flags did not change is left out: a pointer move that stays
        // inside one control is the common case, and it should cost nothing.
        let affected = [previous_hover, previous_press, self.hover, self.press];
        let mut updates: Vec<ControlStateUpdate> = Vec::new();
        for (index, id) in affected.into_iter().enumerate() {
            let Some(id) = id else { continue };
            if affected[..index].contains(&Some(id)) {
                continue;
            }
            let was = (previous_hover == Some(id), previous_press == Some(id));
            let now = (self.hover == Some(id), self.press == Some(id));
            if was == now {
                continue;
            }
            updates.push(ControlStateUpdate { id, hovered: now.0, pressed: now.1 });
        }
        updates
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `(id, hovered, pressed)` triples a sequence of pointer events
    /// produces, as a compact comparable form.
    fn run(events: &[(PointerPhase, Option<u64>)]) -> Vec<(u64, bool, bool)> {
        let mut state = PointerState::default();
        let mut all = Vec::new();
        for (phase, target) in events {
            all.extend(
                state.apply(*phase, *target).into_iter().map(|u| (u.id, u.hovered, u.pressed)),
            );
        }
        all
    }

    #[test]
    fn moving_onto_a_control_hovers_it() {
        assert_eq!(run(&[(PointerPhase::Moved, Some(1))]), [(1, true, false)]);
    }

    #[test]
    fn moving_between_controls_releases_the_old_one() {
        assert_eq!(
            run(&[(PointerPhase::Moved, Some(1)), (PointerPhase::Moved, Some(2))]),
            [(1, true, false), (1, false, false), (2, true, false)],
            "the control left goes un-hovered and the one entered becomes hovered",
        );
    }

    #[test]
    fn moving_onto_nothing_clears_the_hover() {
        assert_eq!(
            run(&[(PointerPhase::Moved, Some(1)), (PointerPhase::Moved, None)]),
            [(1, true, false), (1, false, false)],
        );
    }

    #[test]
    fn pressing_then_releasing_on_the_same_control() {
        assert_eq!(
            run(&[(PointerPhase::Pressed, Some(1)), (PointerPhase::Released, Some(1))]),
            [(1, true, true), (1, true, false)],
            "a press reports hover and press together, a release only drops the press",
        );
    }

    #[test]
    fn dragging_out_of_a_button_keeps_it_pressed() {
        assert_eq!(
            run(&[
                (PointerPhase::Pressed, Some(1)),
                (PointerPhase::Moved, None),
                (PointerPhase::Released, None),
            ]),
            [(1, true, true), (1, false, true), (1, false, false)],
            "the press follows the button, not the pointer",
        );
    }

    #[test]
    fn dragging_onto_another_control_does_not_press_it() {
        assert_eq!(
            run(&[(PointerPhase::Pressed, Some(1)), (PointerPhase::Moved, Some(2))]),
            [(1, true, true), (1, false, true), (2, true, false)],
            "the second control is hovered but not pressed",
        );
    }

    #[test]
    fn pressing_on_nothing_presses_nothing() {
        assert!(run(&[(PointerPhase::Pressed, None), (PointerPhase::Released, None)]).is_empty());
    }

    #[test]
    fn exiting_clears_hover_and_press() {
        assert_eq!(
            run(&[(PointerPhase::Pressed, Some(1)), (PointerPhase::Exited, None)]),
            [(1, true, true), (1, false, false)],
        );
    }

    #[test]
    fn a_control_is_reported_once_per_event_with_both_flags() {
        let mut state = PointerState::default();
        // A press on an already-hovered control: the control is both the
        // previous hover and the target, and must still be named once.
        state.apply(PointerPhase::Moved, Some(7));
        let updates = state.apply(PointerPhase::Pressed, Some(7));
        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].id, updates[0].hovered, updates[0].pressed), (7, true, true));
    }

    #[test]
    fn a_move_onto_the_same_control_reports_nothing() {
        let mut state = PointerState::default();
        state.apply(PointerPhase::Moved, Some(1));
        assert!(state.apply(PointerPhase::Moved, Some(1)).is_empty());
    }

    #[test]
    fn a_move_while_pressed_reports_nothing_when_nothing_changed() {
        let mut state = PointerState::default();
        state.apply(PointerPhase::Pressed, Some(1));
        // Moving around inside the pressed button is the common case during a
        // drag and must stay free.
        assert!(state.apply(PointerPhase::Moved, Some(1)).is_empty());
    }

    #[test]
    fn the_outcome_does_not_depend_on_which_moves_were_delivered() {
        // Only the phase decides where the press lives, so a coalesced or
        // dropped move cannot move a press to another control.
        let mut coalesced = PointerState::default();
        coalesced.apply(PointerPhase::Pressed, Some(1));
        coalesced.apply(PointerPhase::Moved, Some(2));
        assert_eq!(coalesced.press, Some(1));

        let mut stepwise = PointerState::default();
        stepwise.apply(PointerPhase::Pressed, Some(1));
        stepwise.apply(PointerPhase::Moved, Some(2));
        stepwise.apply(PointerPhase::Moved, Some(3));
        assert_eq!(stepwise.press, Some(1));
    }

    #[test]
    fn the_topmost_control_wins_an_overlap() {
        let mut published = PublishedControls::default();
        // Published back to front, the way the encoder walks the tree.
        published.insert(
            1,
            ControlCoord {
                x: 0.,
                y: 0.,
                width: 100.,
                height: 100.,
                hovered: false,
                pressed: false,
            },
        );
        published.insert(
            2,
            ControlCoord {
                x: 10.,
                y: 10.,
                width: 20.,
                height: 20.,
                hovered: false,
                pressed: false,
            },
        );
        assert_eq!(published.control_at(5., 5.), Some(1));
        assert_eq!(published.control_at(15., 15.), Some(2), "the later control is on top");
        assert_eq!(published.control_at(500., 500.), None);
    }
}
