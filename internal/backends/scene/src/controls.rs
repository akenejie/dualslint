// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// The control geometry encoder — the render thread's view of a window.
//
// A render thread that owns the item tree draws through the tree's own
// renderer, so it never needs the commands a draw would produce: what it owes
// the UI thread is the geometry of the things a pointer can hit, because the
// pointer position only exists on the UI thread and that is where hit-testing
// happens.  This module walks the item tree for exactly that.
//
// The walk goes through the `ItemRenderer` trait, which is also what lays the
// tree out: a container only computes its geometry while it renders, so a walk
// that skipped the draw pass would report the geometry of the previous frame.
// The draw methods below are therefore empty rather than absent — they are how
// the walk visits an item at all.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::pin::Pin;

use i_slint_core::item_rendering::{
    CachedRenderingData, ItemRenderer, RenderBorderRectangle, RenderImage, RenderRectangle,
    RenderText,
};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::{Clip, Layer, Opacity};
use i_slint_core::lengths::{
    LogicalBorderRadius, LogicalPoint, LogicalRect, LogicalSize, LogicalVector, ScaleFactor,
};
use i_slint_core::platform::PlatformError;
use i_slint_core::window::WindowInner;

use crate::ControlRegion;

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
// ControlEncoder
// ---------------------------------------------------------------------------

/// Walks a window's item tree and records where its controls ended up.
pub struct ControlEncoder<'a> {
    /// The window being walked.  The runtime asks a renderer for it, and here
    /// it is the answer to every query a drawing renderer would make.
    window_inner: &'a WindowInner,
    /// Control regions for this walk, one per interactive item.
    controls: Vec<ControlRegion>,
    /// The render-side item behind each control id, so the render thread —
    /// which owns the tree — can loan out its properties.  The `ItemRc`s are
    /// not `Send` and stay on the thread that encoded them.
    item_refs: Vec<(u64, ItemRc)>,
    scale_factor: ScaleFactor,
}

impl<'a> ControlEncoder<'a> {
    pub fn new(window_inner: &'a WindowInner) -> Self {
        let scale_factor = ScaleFactor::new(window_inner.scale_factor());
        Self { window_inner, controls: Vec::new(), item_refs: Vec::new(), scale_factor }
    }

    /// Publish this item as a control region: a target for hit-testing and for
    /// `RenderHost::apply_control_state`.  Geometry is in window-space logical
    /// pixels, taken from the laid-out item so that the published rectangle is
    /// the one the user can point at rather than the one that happened to be
    /// drawn.
    fn push_control(&mut self, item: &ItemRc, origin: LogicalPoint) {
        let geometry = item.geometry();
        if geometry.size.is_empty() {
            return;
        }
        // A popup is a tree of its own that the window draws at an offset, so
        // the item's own position is relative to the popup and the published
        // rectangle carries the offset. Everything downstream hit-tests in the
        // coordinates of the window the pointer is in.
        let window_origin = origin + item.map_to_window(geometry.origin).to_vector();
        let id = item_rc_as_id(item);
        self.controls
            .push(ControlRegion { id, geometry: LogicalRect::new(window_origin, geometry.size) });
        self.item_refs.push((id, item.clone()));
    }

    /// Publish the interactive items of `component` as control regions.
    ///
    /// Whether an item is painted and whether it can be pointed at are two
    /// different questions: a `TouchArea` without a background is never asked to
    /// draw anything yet is still a hit target, and a plain rectangle is drawn
    /// but not interactive.  So the controls are found by walking the tree
    /// rather than by watching the draw calls, and the walk runs back to front
    /// so that a later entry paints over an earlier one — which lets the
    /// hit-test take the last match as the topmost control.
    fn collect_interactive_controls(
        &mut self,
        component: &i_slint_core::item_tree::ItemTreeRc,
        origin: LogicalPoint,
    ) {
        i_slint_core::item_tree::visit_items(
            component,
            i_slint_core::item_tree::TraversalOrder::BackToFront,
            |component, _item, index, _state| {
                let item_rc = ItemRc::new(component.clone(), index);
                // A control is something the user can act on: point at it, type
                // into it, or tab to it.  A plain rectangle is painted but not
                // acted on, and it has no business in a control id space.  What
                // is in the space is what the property protocol has to be able
                // to name, so the two lists are kept together.
                let interactive = item_rc.downcast::<i_slint_core::items::TouchArea>().is_some()
                    || item_rc.downcast::<i_slint_core::items::FocusScope>().is_some()
                    || item_rc.downcast::<i_slint_core::items::TextInput>().is_some();
                if interactive {
                    self.push_control(&item_rc, origin);
                }
                i_slint_core::item_tree::ItemVisitorResult::Continue(())
            },
            (),
        );
    }

    fn finish(self) -> ControlTable {
        ControlTable { controls: self.controls, item_refs: self.item_refs }
    }
}

// ---------------------------------------------------------------------------
// ItemRenderer implementation
// ---------------------------------------------------------------------------
//
// Every draw method is empty on purpose; see the module comment.  The ones that
// are not empty are the clip and opacity bookkeeping the walk itself needs.

impl ItemRenderer for ControlEncoder<'_> {
    fn draw_rectangle(
        &mut self,
        _rect: Pin<&dyn RenderRectangle>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
    }

    fn draw_border_rectangle(
        &mut self,
        _rect: Pin<&dyn RenderBorderRectangle>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
    }

    fn draw_window_background(
        &mut self,
        _rect: Pin<&dyn RenderRectangle>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
    }

    fn draw_image(
        &mut self,
        _image: Pin<&dyn RenderImage>,
        _item_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
    }

    fn draw_text(
        &mut self,
        _text: Pin<&dyn RenderText>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
    }

    fn draw_text_input(
        &mut self,
        _text_input: Pin<&i_slint_core::items::TextInput>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) {
    }

    fn draw_path(
        &mut self,
        _path: Pin<&i_slint_core::items::Path>,
        _item_rc: &ItemRc,
        _size: LogicalSize,
    ) {
    }

    fn draw_box_shadow(
        &mut self,
        _box_shadow: Pin<&i_slint_core::items::BoxShadow>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) {
    }

    fn visit_opacity(
        &mut self,
        opacity_item: Pin<&Opacity>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) -> i_slint_core::items::RenderingResult {
        self.apply_opacity(opacity_item.opacity());
        i_slint_core::items::RenderingResult::ContinueRenderingChildren
    }

    fn visit_layer(
        &mut self,
        _layer_item: Pin<&Layer>,
        _self_rc: &ItemRc,
        _size: LogicalSize,
    ) -> i_slint_core::items::RenderingResult {
        // A layer is drawn into a texture and composited, which is the
        // renderer's own drawing; with nothing to draw there is nothing to
        // visit, and the layer's children are not part of the window geometry.
        i_slint_core::items::RenderingResult::ContinueRenderingChildren
    }

    fn visit_clip(
        &mut self,
        _clip_item: Pin<&Clip>,
        _item_rc: &ItemRc,
        size: LogicalSize,
    ) -> i_slint_core::items::RenderingResult {
        // The clip does not hide anything here: a control behind one is still
        // a control, and deciding that belongs to the draw pass that would
        // have clipped it.
        let _ = size;
        i_slint_core::items::RenderingResult::ContinueRenderingChildren
    }

    fn combine_clip(&mut self, _rect: LogicalRect, _radius: LogicalBorderRadius) -> bool {
        true
    }

    fn get_current_clip(&self) -> LogicalRect {
        LogicalRect::new(
            i_slint_core::lengths::LogicalPoint::default(),
            LogicalSize::new(f32::MAX, f32::MAX),
        )
    }

    fn translate(&mut self, _distance: LogicalVector) {}

    fn rotate(&mut self, _angle_in_degrees: f32) {}

    fn scale(&mut self, _scale_x: f32, _scale_y: f32) {}

    fn apply_opacity(&mut self, _opacity: f32) {}

    fn global_alpha_transparent(&self) -> bool {
        false
    }

    fn save_state(&mut self) {}

    fn restore_state(&mut self) {}

    fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    fn draw_cached_pixmap(
        &mut self,
        _item_cache: &ItemRc,
        _update_fn: &dyn Fn(&mut dyn FnMut(u32, u32, &[u8])),
    ) {
    }

    fn draw_string(&mut self, _string: &str, _color: i_slint_core::Color) {}

    fn draw_image_direct(&mut self, _image: i_slint_core::graphics::Image) {}

    fn window(&self) -> &WindowInner {
        self.window_inner
    }

    fn as_any(&mut self) -> Option<&mut dyn core::any::Any> {
        None
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The controls a window offers to a pointer, and the items behind them.
pub struct ControlTable {
    /// The interactive items, in back-to-front order, so that the last match for
    /// a point is the topmost control.
    pub controls: Vec<ControlRegion>,
    /// The `ItemRc` behind each id in `controls`. Not `Send`, so this stays on
    /// the thread that encoded it.
    pub item_refs: Vec<(u64, ItemRc)>,
}

/// The control geometry of a window, and the render-side `ItemRc` behind every
/// control id.
///
/// The UI thread hit-tests a pointer against this, and the render thread is the
/// only one that can say what a control is or lend out one of its properties.
pub fn encode_window_controls(
    window: &i_slint_core::api::Window,
) -> Result<ControlTable, PlatformError> {
    let window_inner = WindowInner::from_pub(window);
    let window_adapter = window_inner.window_adapter();
    let size = window_adapter.size();
    if size.width == 0 || size.height == 0 {
        return Ok(ControlTable { controls: Vec::new(), item_refs: Vec::new() });
    }

    let mut encoder = ControlEncoder::new(window_inner);

    window_inner.draw_contents(|components, post_render| {
        // Every tree the window draws is walked for controls, not just its own:
        // a menu is a popup tree of its own, and its items are controls the user
        // points at like any other. They come after the window's own items, so
        // the hit-test takes them first and the menu is on top where it is drawn.
        for (component, origin) in components {
            if let Some(component) = i_slint_core::item_tree::ItemTreeWeak::upgrade(component) {
                i_slint_core::item_rendering::render_component_items(
                    &component,
                    &mut encoder,
                    *origin,
                    &window_adapter,
                );
                // After that tree's draw pass, so that the interactive items are
                // visited whether or not they contributed a draw command.
                encoder.collect_interactive_controls(&component, *origin);
            }
        }
        post_render(&mut encoder);
    });

    Ok(encoder.finish())
}
