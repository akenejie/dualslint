// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// Snapshot encoder — the UI-thread side of the 2-thread split.
//
// Walks the Slint item tree via the `ItemRenderer` and `GlyphRenderer` trait
// methods, serialising every draw into a `Vec<DrawCommand>` that the render
// thread replays against a FemtoVG/GL canvas it owns entirely.
//
// All coordinates in the command stream are in **physical pixels**. The
// encoder converts logical inputs from the item tree, and the render thread
// replays them against a femtovg canvas that also works in physical pixels.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::pin::Pin;

use i_slint_core::Color as CoreColor;
use i_slint_core::graphics::{Brush, Image, ImageInner, IntRect, euclid};
use i_slint_core::item_rendering::{
    BorderRectLayout, CachedRenderingData, ItemRenderer, RenderBorderRectangle, RenderImage,
    RenderRectangle, RenderText,
};
use i_slint_core::items::{self, Clip, FillRule, ItemRc, Layer, Opacity, Path, RenderingResult};
use i_slint_core::lengths::{
    LogicalBorderRadius, LogicalPoint, LogicalRect, LogicalSize, LogicalVector, PhysicalPx,
    ScaleFactor,
};
use i_slint_core::platform::PlatformError;
use i_slint_core::textlayout::sharedparley::{self, GlyphRenderer, fontique, parley};
use i_slint_core::window::WindowInner;

use crate::render_thread::{
    ControlRegion, DrawCommand, GradientStop, LineCapDesc, LineJoinDesc, PaintDesc, PathEvent,
    PhysicalLength, PhysicalPoint, PhysicalRect, PositionedGlyph, SceneFont, SceneFrame,
};

// ---------------------------------------------------------------------------
// Brush → PaintDesc conversion
// ---------------------------------------------------------------------------

fn color_to_u8_array(c: CoreColor) -> [u8; 4] {
    [c.red(), c.green(), c.blue(), c.alpha()]
}

fn stops_to_gradient_stops(stops: &[i_slint_core::graphics::GradientStop]) -> Vec<GradientStop> {
    stops
        .iter()
        .map(|s| GradientStop { offset: s.position, color: color_to_u8_array(s.color) })
        .collect()
}

/// Resolve a `Brush` into a `PaintDesc` given the shape's physical-pixel
/// size.  Returns `None` when the brush is transparent.
fn brush_to_paint_desc(
    brush: &Brush,
    size: euclid::Size2D<f32, PhysicalPx>,
    scale_factor: ScaleFactor,
) -> Option<PaintDesc> {
    use i_slint_core::graphics::resolve_brush;

    let resolved = resolve_brush(brush, size, scale_factor)?;
    Some(match resolved {
        i_slint_core::graphics::ResolvedBrush::SolidColor(color) => {
            PaintDesc::Solid { r: color.red(), g: color.green(), b: color.blue(), a: color.alpha() }
        }
        i_slint_core::graphics::ResolvedBrush::LinearGradient(gradient) => {
            PaintDesc::LinearGradient {
                start_x: gradient.start.x,
                start_y: gradient.start.y,
                end_x: gradient.end.x,
                end_y: gradient.end.y,
                stops: stops_to_gradient_stops(&gradient.stops),
            }
        }
        i_slint_core::graphics::ResolvedBrush::RadialGradient(gradient) => {
            PaintDesc::RadialGradient {
                cx: gradient.center.x,
                cy: gradient.center.y,
                radius: gradient.radius.get(),
                stops: stops_to_gradient_stops(&gradient.stops),
            }
        }
        i_slint_core::graphics::ResolvedBrush::ConicGradient(gradient) => {
            // Conic gradients are not in PaintDesc yet; approximate as solid.
            let first = gradient.stops.first().map(|s| s.color);
            let c = first.unwrap_or_default();
            PaintDesc::Solid { r: c.red(), g: c.green(), b: c.blue(), a: c.alpha() }
        }
    })
}

// ---------------------------------------------------------------------------
// Path conversion helpers
// ---------------------------------------------------------------------------

fn femtovg_line_cap(cap: items::LineCap) -> LineCapDesc {
    match cap {
        items::LineCap::Round => LineCapDesc::Round,
        items::LineCap::Square => LineCapDesc::Square,
        items::LineCap::Butt | _ => LineCapDesc::Butt,
    }
}

fn femtovg_line_join(join: items::LineJoin) -> LineJoinDesc {
    match join {
        items::LineJoin::Round => LineJoinDesc::Round,
        items::LineJoin::Bevel => LineJoinDesc::Bevel,
        items::LineJoin::Miter | _ => LineJoinDesc::Miter,
    }
}

/// Unique u64 id from an ItemRc (component ptr + item index), used for
/// cache keys and control regions.
pub(crate) fn item_rc_as_id(item_rc: &ItemRc) -> u64 {
    let mut hasher = DefaultHasher::new();
    let ptr = &**item_rc.item_tree() as *const _ as usize;
    ptr.hash(&mut hasher);
    item_rc.index().hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// SnapshotEncoder
// ---------------------------------------------------------------------------

/// Internal state pushed/popped in lock-step with `DrawCommand::Save` /
/// `DrawCommand::Restore`.  Tracks the **logical** scissor and alpha so
/// that the `ItemRenderer` trait's logical-coordinate API is satisfied; the
/// encoder converts to physical coordinates when emitting commands.
#[derive(Clone)]
struct State {
    /// Current scissor rect in logical coordinates.
    scissor: LogicalRect,
    /// Accumulated opacity (product of all `apply_opacity` calls).
    global_alpha: f32,
}

/// The UI-thread snapshot encoder.
pub(crate) struct SnapshotEncoder {
    /// The commands being accumulated.
    pub(crate) commands: Vec<DrawCommand>,
    /// Control regions for this frame (one per visible item).
    pub(crate) controls: Vec<ControlRegion>,
    /// The render-side item behind each control id, so the render thread —
    /// which owns the mirror tree — can loan out its properties.  This is
    /// only meaningful on the render thread; the UI thread drops it before
    /// sending the `SceneFrame`.
    pub(crate) item_refs: Vec<(u64, ItemRc)>,
    /// Current transform state stack.
    state: Vec<State>,
    /// Physical pixel dimensions of the window.
    width: u32,
    height: u32,
    scale_factor: ScaleFactor,
    /// Monotonic key counter for texture upload commands.
    next_key: u64,
    /// Solid window background (RGBA), cleared by the render thread before
    /// the command replay. `None` when the window background is a gradient,
    /// in which case the encoder emits a full-viewport background rect.
    background: Option<[u8; 4]>,
    /// Separate text layout cache for the snapshot encoder (independent of
    /// the renderer's own cache so it works without a GL context).
    text_layout_cache: sharedparley::TextLayoutCache,
    /// Deduplicated font payloads referenced by this frame's glyph runs.
    fonts: Vec<SceneFont>,
    /// The window adapter for dependency tracking / window queries.
    window_adapter: i_slint_core::window::WindowAdapterRc,
}

impl SnapshotEncoder {
    pub(crate) fn new(
        width: u32,
        height: u32,
        scale_factor: ScaleFactor,
        window_adapter: i_slint_core::window::WindowAdapterRc,
    ) -> Self {
        let phys_size = euclid::Size2D::new(width as f32, height as f32);
        let logical_size: LogicalSize = (phys_size / scale_factor.get()).cast();
        Self {
            commands: Vec::new(),
            controls: Vec::new(),
            item_refs: Vec::new(),
            state: vec![State {
                scissor: LogicalRect::new(LogicalPoint::default(), logical_size),
                global_alpha: 1.0,
            }],
            width,
            height,
            scale_factor,
            next_key: 1,
            text_layout_cache: sharedparley::TextLayoutCache::default(),
            fonts: Vec::new(),
            window_adapter,
            background: None,
        }
    }

    /// Set the solid window background color that the render thread clears with.
    pub(crate) fn set_background(&mut self, color: CoreColor) {
        self.background = Some([color.red(), color.green(), color.blue(), color.alpha()]);
    }

/// Finish encoding and return the complete `SceneFrame` plus the render-side
/// item behind each control id.
pub(crate) fn finish(self) -> (SceneFrame, Vec<(u64, ItemRc)>) {
        let frame = SceneFrame {
            width: self.width,
            height: self.height,
            scale_factor: self.scale_factor.get(),
            background: self.background,
            fonts: self.fonts,
            commands: self.commands,
            controls: self.controls,
        };
        (frame, self.item_refs)
    }

    fn alloc_key(&mut self) -> u64 {
        let k = self.next_key;
        self.next_key = k.wrapping_add(1);
        k
    }

    /// Publish this item as a control region (a target for hit-testing and
    /// `RenderHost::apply_control_state`).  Geometry is in window-space logical
    /// pixels.
    fn push_control(&mut self, item: &ItemRc, size: LogicalSize) {
        if size.is_empty() {
            return;
        }
        let origin = item.geometry().origin;
        let window_origin = item.map_to_window(origin);
        self.controls.push(ControlRegion {
            id: item_rc_as_id(item),
            geometry: LogicalRect::new(window_origin, size),
        });
        self.item_refs.push((item_rc_as_id(item), item.clone()));
    }

    fn push(&mut self, cmd: DrawCommand) {
        self.commands.push(cmd);
    }

    fn phys_rect(&self, logical: LogicalSize) -> euclid::Size2D<f32, PhysicalPx> {
        logical * self.scale_factor
    }
}

// ---------------------------------------------------------------------------
// ItemRenderer implementation
// ---------------------------------------------------------------------------

impl ItemRenderer for SnapshotEncoder {
    fn draw_rectangle(
        &mut self,
        rect: Pin<&dyn RenderRectangle>,
        self_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        let brush = rect.background();
        let phys_size = size * self.scale_factor;
        let geometry = euclid::Rect::from_size(phys_size);
        if geometry.is_empty() {
            return;
        }
        let paint = match brush_to_paint_desc(&brush, phys_size, self.scale_factor) {
            Some(p) => p,
            None => return,
        };
        self.push(DrawCommand::FillRect { rect: geometry, paint, anti_alias: false });
        self.push_control(self_rc, size);
    }

    fn draw_border_rectangle(
        &mut self,
        rect: Pin<&dyn RenderBorderRectangle>,
        self_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        let Some(layout) = BorderRectLayout::new(rect, size, self.scale_factor) else {
            return;
        };

        // Background fill
        let bg_brush = rect.background();
        let bg_paint = brush_to_paint_desc(&bg_brush, layout.brush_size, self.scale_factor);

        if let Some(paint) = bg_paint {
            self.push(DrawCommand::FillRoundedRect {
                rect: layout.background_rect,
                paint,
                radius: layout.background_radius,
                anti_alias: true,
            });
        }

        // Border stroke (rounded path, mirroring upstream femtovg renderer)
        if layout.border_width.get() > 0.0 {
            let border_paint =
                brush_to_paint_desc(&layout.border_color, layout.brush_size, self.scale_factor);
            if let Some(paint) = border_paint {
                self.push(DrawCommand::StrokeRoundedRect {
                    rect: layout.border_rect,
                    paint,
                    radius: layout.border_radius,
                    line_width: layout.border_width.get(),
                    anti_alias: true,
                });
            }
        }
        self.push_control(self_rc, size);
    }

    fn draw_window_background(
        &mut self,
        rect: Pin<&dyn RenderRectangle>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        // Dependency tracking only — actual window background is drawn by
        // the backend before the item tree walk.
        let _ = rect.background();
    }

    fn draw_image(
        &mut self,
        image: Pin<&dyn RenderImage>,
        item_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        if size.width <= 0.0 || size.height <= 0.0 {
            return;
        }
        let source = image.source();
        let image_inner: &ImageInner = (&source).into();

        // Mirror the upstream femtovg renderer: compute the destination rect
        // that fits the source into the item, respecting the image-fit mode,
        // aspect ratio and alignment. The canvas transform is already translated
        // to the item origin, so the blit lands in the item's local space.
        let orig_size = source.size().cast::<f32>();
        let source_clip_rect = image.source_clip().unwrap_or(IntRect::from_size(orig_size.cast()));
        let fitted = i_slint_core::graphics::fit(
            image.image_fit(),
            size * self.scale_factor,
            source_clip_rect,
            self.scale_factor,
            image.alignment(),
            image.tiling(),
        );

        let target_size_for_scalable =
            if image_inner.is_svg() { Some(fitted.size.cast()) } else { None };

        let Some(pixel_buffer) = image_inner.render_to_buffer(target_size_for_scalable) else {
            return;
        };

        let (buf_w, buf_h, rgba) = match &pixel_buffer {
            i_slint_core::graphics::SharedImageBuffer::RGBA8(pixels) => {
                (pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            }
            i_slint_core::graphics::SharedImageBuffer::RGBA8Premultiplied(pixels) => {
                (pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            }
            i_slint_core::graphics::SharedImageBuffer::RGB8(pixels) => {
                let w = pixels.width();
                let h = pixels.height();
                let rgba: Vec<u8> = pixels
                    .as_bytes()
                    .chunks(3)
                    .flat_map(|rgb| IntoIterator::into_iter([rgb[0], rgb[1], rgb[2], 255]))
                    .collect();
                (w, h, rgba)
            }
        };

        let key = self.alloc_key();
        self.push(DrawCommand::UploadPixmap { key, pixels: rgba, width: buf_w, height: buf_h });

        self.push(DrawCommand::BlitPixmap {
            key,
            params: [
                fitted.offset.x,
                fitted.offset.y,
                fitted.size.width,
                fitted.size.height,
                0.0,
                1.0,
                buf_w as f32,
                buf_h as f32,
                0.0,
            ],
        });

        self.controls.push(ControlRegion {
            id: item_rc_as_id(item_rc),
            geometry: LogicalRect::new(LogicalPoint::default(), size),
        });
    }

    fn draw_text(
        &mut self,
        text: Pin<&dyn RenderText>,
        self_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        // `TextLayoutCache` is not `Clone` and `self` is borrowed mutably as
        // the `GlyphRenderer`, so temporarily move the layout cache out and
        // back around the call to satisfy the borrow checker.
        let layout_cache = std::mem::replace(
            &mut self.text_layout_cache,
            sharedparley::TextLayoutCache::default(),
        );
        sharedparley::draw_text(self, text, Some(self_rc), size, Some(&layout_cache));
        self.text_layout_cache = layout_cache;
        self.push_control(self_rc, size);
    }

    fn draw_text_input(
        &mut self,
        text_input: Pin<&items::TextInput>,
        self_rc: &ItemRc,
        size: LogicalSize,
    ) {
        let layout_cache = std::mem::replace(
            &mut self.text_layout_cache,
            sharedparley::TextLayoutCache::default(),
        );
        sharedparley::draw_text_input(self, text_input, self_rc, size, &layout_cache);
        self.text_layout_cache = layout_cache;
    }

    fn draw_path(&mut self, path: Pin<&Path>, _item_rc: &ItemRc, size: LogicalSize) {
        let (offset, path_events) = match path.fitted_path_events(_item_rc) {
            Some(offset_and_events) => offset_and_events,
            None => return,
        };

        let sf = self.scale_factor.get();

        // Convert lyon_path events into PathEvent list (physical coords)
        let mut events: Vec<PathEvent> = Vec::new();

        for ev in path_events.iter() {
            match ev {
                lyon_path::Event::Begin { at } => {
                    events.push(PathEvent::MoveTo(at.x * sf, at.y * sf));
                }
                lyon_path::Event::Line { from: _, to } => {
                    events.push(PathEvent::LineTo(to.x * sf, to.y * sf));
                }
                lyon_path::Event::Quadratic { from: _, ctrl, to } => {
                    events.push(PathEvent::QuadTo(ctrl.x * sf, ctrl.y * sf, to.x * sf, to.y * sf));
                }
                lyon_path::Event::Cubic { from: _, ctrl1, ctrl2, to } => {
                    events.push(PathEvent::CubicTo(
                        ctrl1.x * sf,
                        ctrl1.y * sf,
                        ctrl2.x * sf,
                        ctrl2.y * sf,
                        to.x * sf,
                        to.y * sf,
                    ));
                }
                lyon_path::Event::End { last: _, first: _, close } => {
                    if close {
                        events.push(PathEvent::Close);
                    }
                }
            }
        }

        let offset_phys = offset * sf;
        let fill_phys_size = size * self.scale_factor;

        // Apply the offset via translate
        if offset_phys.x != 0.0 || offset_phys.y != 0.0 {
            self.push(DrawCommand::Save);
            self.push(DrawCommand::Translate(offset_phys.x, offset_phys.y));
        }

        // Fill
        let fill_brush = path.fill();
        let fill_paint = brush_to_paint_desc(&fill_brush, fill_phys_size, self.scale_factor);
        let anti_alias = path.anti_alias();

        if let Some(paint) = fill_paint {
            self.push(DrawCommand::FillPath {
                path: events.clone(),
                paint,
                fill_rule: match path.fill_rule() {
                    FillRule::Evenodd => 1,
                    _ => 0,
                },
                anti_alias,
            });
        }

        // Stroke
        let stroke_brush = path.stroke();
        let stroke_paint = brush_to_paint_desc(&stroke_brush, fill_phys_size, self.scale_factor);
        if let Some(paint) = stroke_paint {
            self.push(DrawCommand::StrokePath {
                path: events,
                paint,
                line_width: path.stroke_width().get() * sf,
                line_cap: femtovg_line_cap(path.stroke_line_cap()),
                line_join: femtovg_line_join(path.stroke_line_join()),
                miter_limit: path.stroke_miter_limit(),
                anti_alias,
            });
        }

        if offset_phys.x != 0.0 || offset_phys.y != 0.0 {
            self.push(DrawCommand::Restore);
        }
    }

    fn draw_box_shadow(
        &mut self,
        box_shadow: Pin<&items::BoxShadow>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) {
        use i_slint_core::graphics::boxshadowcache::BoxShadowOptions;

        let Some(options) = BoxShadowOptions::new(_self_rc, box_shadow, self.scale_factor) else {
            return;
        };
        if options.inset {
            return;
        }

        let offset_x = (box_shadow.offset_x() * self.scale_factor).get();
        let offset_y = (box_shadow.offset_y() * self.scale_factor).get();
        if options.blur.get() == 0.0 && offset_x == 0.0 && offset_y == 0.0 {
            return;
        }

        self.push(DrawCommand::DrawBoxShadow {
            color: options.color,
            blur: options.blur.get(),
            offset_x,
            offset_y,
            width: options.width.get(),
            height: options.height.get(),
            radius: options.radius,
        });
    }

    fn visit_opacity(
        &mut self,
        opacity_item: Pin<&Opacity>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) -> RenderingResult {
        let opacity = opacity_item.opacity();
        // For simplicity, apply opacity directly (no offscreen layer).  This
        // is correct for non-overlapping children; overlapping opacity may
        // blend incorrectly.  A future improvement can emit DrawCommand::Layer.
        let alpha = {
            let st = self.state.last_mut().unwrap();
            st.global_alpha *= opacity;
            st.global_alpha
        };
        self.push(DrawCommand::SetGlobalAlpha(alpha));
        RenderingResult::ContinueRenderingChildren
    }

    fn visit_layer(
        &mut self,
        layer_item: Pin<&Layer>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) -> RenderingResult {
        if !layer_item.cache_rendering_hint() {
            return RenderingResult::ContinueRenderingChildren;
        }
        // Fall through — the render thread has its own layer cache.
        // Full snapshot-encoding of layers requires a two-pass walk;
        // for now, draw children directly.
        RenderingResult::ContinueRenderingChildren
    }

    fn visit_clip(
        &mut self,
        clip_item: Pin<&Clip>,
        _item_rc: &ItemRc,
        size: LogicalSize,
    ) -> RenderingResult {
        if !clip_item.clip() {
            return RenderingResult::ContinueRenderingChildren;
        }

        let (clip_rect, clip_radius) = i_slint_core::item_rendering::clip_content_box(
            size,
            clip_item.logical_border_radius(),
            clip_item.border_width(),
        );

        if !self.get_current_clip().intersects(&clip_rect) {
            return RenderingResult::ContinueRenderingWithoutChildren;
        }

        if !clip_radius.is_zero() {
            // Rounded clip — approximate as rectangular clip for now.
            self.combine_clip(clip_rect, clip_radius);
        } else {
            self.combine_clip(clip_rect, LogicalBorderRadius::default());
        }
        RenderingResult::ContinueRenderingChildren
    }

    fn combine_clip(&mut self, clip_rect: LogicalRect, _radius: LogicalBorderRadius) -> bool {
        let scissor = &mut self.state.last_mut().unwrap().scissor;
        match scissor.intersection(&clip_rect) {
            Some(r) => {
                *scissor = r;
                let phys_clip = r * self.scale_factor;
                self.push(DrawCommand::CombineClip(
                    euclid::Rect::from_size(euclid::size2(phys_clip.width(), phys_clip.height()))
                        .translate(euclid::vec2(phys_clip.origin.x, phys_clip.origin.y)),
                ));
                true
            }
            None => {
                *scissor = LogicalRect::default();
                false
            }
        }
    }

    fn get_current_clip(&self) -> LogicalRect {
        self.state.last().unwrap().scissor
    }

    fn translate(&mut self, distance: LogicalVector) {
        let phys = distance * self.scale_factor;
        self.push(DrawCommand::Translate(phys.x, phys.y));
        let scissor = &mut self.state.last_mut().unwrap().scissor;
        *scissor = scissor.translate(-distance);
    }

    fn rotate(&mut self, angle_in_degrees: f32) {
        let r = angle_in_degrees.to_radians();
        self.push(DrawCommand::Rotate(r));

        let scissor = &mut self.state.last_mut().unwrap().scissor;
        let (sin, cos) = (-r).sin_cos();
        let rot = |p: LogicalPoint| -> LogicalPoint {
            (p.x * cos - p.y * sin, p.x * sin + p.y * cos).into()
        };
        let corners = [
            rot(scissor.origin),
            rot(scissor.origin + euclid::vec2(scissor.width(), 0.)),
            rot(scissor.origin + euclid::vec2(0., scissor.height())),
            rot(scissor.origin + scissor.size),
        ];
        let origin: LogicalPoint = (
            corners.iter().fold(f32::MAX, |a, b| b.x.min(a)),
            corners.iter().fold(f32::MAX, |a, b| b.y.min(a)),
        )
            .into();
        let end: LogicalPoint = (
            corners.iter().fold(f32::MIN, |a, b| b.x.max(a)),
            corners.iter().fold(f32::MIN, |a, b| b.y.max(a)),
        )
            .into();
        *scissor = LogicalRect::new(origin, (end - origin).into());
    }

    fn scale(&mut self, scale_x: f32, scale_y: f32) {
        self.push(DrawCommand::Scale(scale_x, scale_y));
        let scissor = &mut self.state.last_mut().unwrap().scissor;
        scissor.origin.x /= scale_x;
        scissor.origin.y /= scale_y;
        scissor.size.width /= scale_x;
        scissor.size.height /= scale_y;
    }

    fn apply_opacity(&mut self, opacity: f32) {
        let alpha = {
            let st = self.state.last_mut().unwrap();
            st.global_alpha *= opacity;
            st.global_alpha
        };
        self.push(DrawCommand::SetGlobalAlpha(alpha));
    }

    fn global_alpha_transparent(&self) -> bool {
        self.state.last().unwrap().global_alpha == 0.0
    }

    fn save_state(&mut self) {
        self.push(DrawCommand::Save);
        self.state.push(self.state.last().unwrap().clone());
    }

    fn restore_state(&mut self) {
        self.push(DrawCommand::Restore);
        self.state.pop();
    }

    fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    fn draw_cached_pixmap(
        &mut self,
        item_cache: &ItemRc,
        update_fn: &dyn Fn(&mut dyn FnMut(u32, u32, &[u8])),
    ) {
        let mut pixels_data: Option<(u32, u32, Vec<u8>)> = None;
        update_fn(&mut |width: u32, height: u32, data: &[u8]| {
            pixels_data = Some((width, height, data.to_vec()));
        });
        let (w, h, pixels) = match pixels_data {
            Some(d) => d,
            None => return,
        };
        let key = self.alloc_key();
        self.push(DrawCommand::UploadPixmap { key, pixels, width: w, height: h });
        self.push(DrawCommand::BlitPixmap {
            key,
            params: [0.0, 0.0, w as f32, h as f32, 0.0, 1.0, w as f32, h as f32, 0.0],
        });
        let _ = item_cache;
    }

    fn draw_string(&mut self, string: &str, color: CoreColor) {
        sharedparley::draw_text(
            self,
            std::pin::pin!((i_slint_core::SharedString::from(string), Brush::from(color))),
            None,
            LogicalSize::new(
                self.width as f32 / self.scale_factor.get(),
                self.height as f32 / self.scale_factor.get(),
            ),
            None,
        );
    }

    fn draw_image_direct(&mut self, image: Image) {
        let target_size = LogicalSize::from_untyped(image.size().cast());
        if target_size.is_empty() {
            return;
        }
        let image_inner: &ImageInner = (&image).into();
        let target_size_for_scalable = if image_inner.is_svg() {
            let phys = target_size * self.scale_factor;
            Some(phys.cast())
        } else {
            None
        };
        let Some(pixel_buffer) = image_inner.render_to_buffer(target_size_for_scalable) else {
            return;
        };
        let (buf_w, buf_h, rgba) = match &pixel_buffer {
            i_slint_core::graphics::SharedImageBuffer::RGBA8(pixels) => {
                (pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            }
            i_slint_core::graphics::SharedImageBuffer::RGBA8Premultiplied(pixels) => {
                (pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            }
            i_slint_core::graphics::SharedImageBuffer::RGB8(pixels) => {
                let w = pixels.width();
                let h = pixels.height();
                let rgba: Vec<u8> = pixels
                    .as_bytes()
                    .chunks(3)
                    .flat_map(|rgb| IntoIterator::into_iter([rgb[0], rgb[1], rgb[2], 255]))
                    .collect();
                (w, h, rgba)
            }
        };
        let key = self.alloc_key();
        self.push(DrawCommand::UploadPixmap { key, pixels: rgba, width: buf_w, height: buf_h });
        let phys = target_size * self.scale_factor;
        self.push(DrawCommand::BlitPixmap {
            key,
            params: [0.0, 0.0, phys.width, phys.height, 0.0, 1.0, buf_w as f32, buf_h as f32, 0.0],
        });
    }

    fn window(&self) -> &i_slint_core::window::WindowInner {
        i_slint_core::window::WindowInner::from_pub(self.window_adapter.window())
    }

    fn as_any(&mut self) -> Option<&mut dyn core::any::Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// GlyphRenderer implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) enum GlyphBrush {
    Fill(PaintDesc),
    Stroke(PaintDesc),
}

impl GlyphRenderer for SnapshotEncoder {
    type PlatformBrush = GlyphBrush;

    fn platform_text_fill_brush(
        &mut self,
        brush: Brush,
        size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        let phys_size = size * self.scale_factor;
        brush_to_paint_desc(&brush, phys_size, self.scale_factor).map(GlyphBrush::Fill)
    }

    fn platform_brush_for_color(&mut self, color: &CoreColor) -> Option<Self::PlatformBrush> {
        if color.alpha() == 0 {
            None
        } else {
            Some(GlyphBrush::Fill(PaintDesc::Solid {
                r: color.red(),
                g: color.green(),
                b: color.blue(),
                a: color.alpha(),
            }))
        }
    }

    fn platform_text_stroke_brush(
        &mut self,
        brush: Brush,
        _physical_stroke_width: f32,
        size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        let phys_size = size * self.scale_factor;
        brush_to_paint_desc(&brush, phys_size, self.scale_factor).map(GlyphBrush::Stroke)
    }

    fn draw_glyph_run(
        &mut self,
        font: &parley::FontData,
        font_size: PhysicalLength,
        normalized_coords: &[i16],
        _synthesis: &fontique::Synthesis,
        brush: Self::PlatformBrush,
        y_offset: PhysicalLength,
        glyphs_it: &mut dyn Iterator<Item = parley::layout::Glyph>,
    ) {
        let blob_id = font.data.id();
        let font_index = font.index;
        if !self.fonts.iter().any(|f| f.blob_id == blob_id && f.font_index == font_index) {
            self.fonts.push(SceneFont { blob_id, font_index, data: font.data.data().to_vec() });
        }

        let paint_desc = match &brush {
            GlyphBrush::Fill(p) => p.clone(),
            GlyphBrush::Stroke(p) => p.clone(),
        };
        let is_stroke = matches!(brush, GlyphBrush::Stroke(_));

        let positioned: Vec<PositionedGlyph> =
            glyphs_it.map(|g| PositionedGlyph { x: g.x, y: g.y, id: g.id as u16 }).collect();

        self.push(DrawCommand::DrawGlyphRun {
            font_blob_id: blob_id,
            font_index,
            font_size: font_size.get(),
            normalized_coords: normalized_coords.to_vec(),
            paint: paint_desc,
            y_offset: y_offset.get(),
            glyphs: positioned,
            is_stroke,
        });
    }

    fn fill_rectangle(
        &mut self,
        physical_rect: sharedparley::PhysicalRect,
        brush: Self::PlatformBrush,
        radius: sharedparley::PhysicalLength,
        border: Option<sharedparley::RectangleBorder<Self::PlatformBrush>>,
    ) {
        let paint = match brush {
            GlyphBrush::Fill(p) => p,
            GlyphBrush::Stroke(p) => p,
        };
        let rect = PhysicalRect::new(
            PhysicalPoint::from_lengths(
                PhysicalLength::new(physical_rect.min_x()),
                PhysicalLength::new(physical_rect.min_y()),
            ),
            euclid::Size2D::from_lengths(
                PhysicalLength::new(physical_rect.width()),
                PhysicalLength::new(physical_rect.height()),
            ),
        );
        let border_desc = border.map(|b| {
            let bp = match b.brush {
                GlyphBrush::Fill(p) => p,
                GlyphBrush::Stroke(p) => p,
            };
            (bp, b.width.get())
        });
        self.push(DrawCommand::FillTextRect {
            rect,
            paint,
            radius: radius.get(),
            border: border_desc,
        });
    }
}

/// Serialise the given window's item tree into a `SceneFrame`, independent of
/// which thread / window adapter the component lives on.  This is the shared
/// encode path for both sides of the 2-thread split:
///
/// * the UI thread encodes its own component and submits the frame to the
///   render thread (`DualThreadRenderer::encode_scene`), and
/// * the render thread encodes the render-owned component (instantiated on
///   the render thread via `RenderHost::attach_component`) and composites the
///   frame directly — this is the authoritative draw path: text shaping runs
///   on the render thread through the render component's own font context.
///
/// The window background is handled as in the UI path: a solid color becomes
/// the clear colour, any other brush is serialised as a full-viewport rect.
pub(crate) fn encode_window_scene(
    window: &i_slint_core::api::Window,
) -> Result<SceneFrame, PlatformError> {
    encode_window_scene_full(window).map(|(frame, _)| frame)
}

/// Like [`encode_window_scene`] but also returns the render-side `ItemRc`
/// behind every control id.  Used by the render thread's mirror to loan out
/// control properties; the `ItemRc`s are not `Send` and must never leave the
/// thread that encoded them.
pub(crate) fn encode_window_scene_full(
    window: &i_slint_core::api::Window,
) -> Result<(SceneFrame, Vec<(u64, ItemRc)>), PlatformError> {
    let window_inner = WindowInner::from_pub(window);
    let scale_factor = ScaleFactor::new(window_inner.scale_factor());
    let window_adapter = window_inner.window_adapter();
    let size = window_adapter.size();
    if size.width == 0 || size.height == 0 {
        return Ok((SceneFrame {
            width: 0,
            height: 0,
            scale_factor: scale_factor.get(),
            background: None,
            fonts: Vec::new(),
            commands: Vec::new(),
            controls: Vec::new(),
        }, Vec::new()));
    }

    let mut encoder =
        SnapshotEncoder::new(size.width, size.height, scale_factor, window_adapter.clone());

    if let Some(window_item_rc) = window_inner.window_item_rc() {
        let window_item = window_item_rc.downcast::<i_slint_core::items::WindowItem>().unwrap();
        match window_item.as_pin_ref().background() {
            i_slint_core::graphics::Brush::SolidColor(color) => {
                encoder.set_background(color);
            }
            _ => {
                encoder.draw_rectangle(
                    window_item.as_pin_ref(),
                    &window_item_rc,
                    i_slint_core::lengths::logical_size_from_api(
                        window.size().to_logical(window_inner.scale_factor()),
                    ),
                    &window_item.as_pin_ref().cached_rendering_data,
                );
            }
        }
    }

    window_inner.draw_contents(|components, post_render| {
        for (component, origin) in components {
            if let Some(component) = i_slint_core::item_tree::ItemTreeWeak::upgrade(component) {
                i_slint_core::item_rendering::render_component_items(
                    &component,
                    &mut encoder,
                    *origin,
                    &window_adapter,
                );
            }
        }
        post_render(&mut encoder);
    });

    Ok(encoder.finish())
}
