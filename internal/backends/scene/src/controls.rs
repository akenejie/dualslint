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

use std::pin::Pin;

use i_slint_core::item_rendering::{
    CachedRenderingData, ItemRenderer, RenderBorderRectangle, RenderImage, RenderRectangle,
    RenderText,
};
use i_slint_core::item_tree::{ControlKind, ControlSlot, ItemRc, ItemTreeVTable};
use i_slint_core::items::{
    Clip, ContextMenu, DragArea, Flickable, FocusScope, Layer, Opacity, TextInput, TouchArea,
    WindowMoveArea,
};
use i_slint_core::lengths::{
    LogicalBorderRadius, LogicalPoint, LogicalRect, LogicalSize, LogicalVector, ScaleFactor,
};
use i_slint_core::platform::PlatformError;
use i_slint_core::window::WindowInner;
use vtable::VRc;

use crate::{ControlRegion, PressBehavior, PressTarget};

/// Stable u64 id for the item a control region points at, used for control
/// regions and the map from the UI thread's id back to the render-side item.
///
/// The id is derived from the item's position in the tree rather than from the
/// tree's address, so the UI-side and render-side instances of the same item
/// compute the same id even though they are different objects.
pub(crate) fn item_rc_as_id(item_rc: &ItemRc) -> u64 {
    item_rc.stable_id()
}

/// Whether a pointer press on `item` should move the focus to it.
///
/// The focus is decided on the UI thread, which never sees the item's
/// properties, so the `FocusScope` property and the `TextInput` behaviour are
/// flattened into this one bit here, on the thread that owns the tree.
fn focus_on_click(item: &ItemRc) -> bool {
    if let Some(item) = item.downcast::<i_slint_core::items::FocusScope>() {
        let item = item.as_pin_ref();
        let enabled =
            i_slint_core::items::FocusScope::FIELD_OFFSETS.enabled().apply_pin(item).get();
        let focus_on_click =
            i_slint_core::items::FocusScope::FIELD_OFFSETS.focus_on_click().apply_pin(item).get();
        return enabled && focus_on_click;
    }
    // A TextInput takes the focus whenever it is pressed; there is no property
    // to opt out of it.
    item.downcast::<i_slint_core::items::TextInput>().is_some()
}

/// Whether Tab moves the focus through `item`.
///
/// Same flattening as [`focus_on_click`], for the traversal property instead:
/// the UI thread decides which control Tab goes to next, and it can only do
/// that with the set the render thread publishes here.
fn focus_on_tab(item: &ItemRc) -> bool {
    if let Some(item) = item.downcast::<i_slint_core::items::FocusScope>() {
        let item = item.as_pin_ref();
        let enabled =
            i_slint_core::items::FocusScope::FIELD_OFFSETS.enabled().apply_pin(item).get();
        let focus_on_tab = i_slint_core::items::FocusScope::FIELD_OFFSETS
            .focus_on_tab_navigation()
            .apply_pin(item)
            .get();
        return enabled && focus_on_tab;
    }
    // A TextInput joins the traversal whenever it is enabled.
    item.downcast::<i_slint_core::items::TextInput>()
        .is_some_and(|item| item.as_pin_ref().enabled())
}

/// What an item does with a pointer press, if anything.
///
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
    /// The items that can take a pointer press, in paint order.
    press_targets: Vec<PressTarget>,
    /// The id of the item this tree has the focus on, read when the walk starts.
    focused: Option<u64>,
    scale_factor: ScaleFactor,
}

impl<'a> ControlEncoder<'a> {
    pub fn new(window_inner: &'a WindowInner) -> Self {
        let scale_factor = ScaleFactor::new(window_inner.scale_factor());
        // A `.slint` file can move the focus on its own, so the answer is read
        // from the tree rather than taken from the UI thread's last decision.
        let focused = window_inner.focus_item.borrow().upgrade().map(|item| item.stable_id());
        Self {
            window_inner,
            controls: Vec::new(),
            item_refs: Vec::new(),
            press_targets: Vec::new(),
            focused,
            scale_factor,
        }
    }

    /// Publish this item as a control region: a target for hit-testing and for
    /// `RenderHost::apply_control_state`.  Geometry is in window-space logical
    /// pixels, taken from the laid-out item so that the published rectangle is
    /// the one the user can point at rather than the one that happened to be
    /// drawn.
    fn push_control(&mut self, item: &ItemRc, origin: LogicalPoint) {
        // A control with no area is still published: a focus-only `FocusScope`
        // is laid out at zero width on purpose -- it takes no clicks but Tab
        // reaches it, and the traversal is what the control table exists to
        // inform. The hit-test ignores it on its own: no point falls inside an
        // empty rectangle.
        let geometry = item.geometry();
        // A popup is a tree of its own that the window draws at an offset, so
        // the item's own position is relative to the popup and the published
        // rectangle carries the offset. Everything downstream hit-tests in the
        // coordinates of the window the pointer is in.
        let window_origin = origin + item.map_to_window(geometry.origin).to_vector();
        let id = item_rc_as_id(item);
        self.controls.push(ControlRegion {
            id,
            geometry: LogicalRect::new(window_origin, geometry.size),
            focus_on_click: focus_on_click(item),
            focus_on_tab: focus_on_tab(item),
            focus_draws: item.focus_draws(),
            ime_ui_owned: item.ime_ui_owned(),
            hover_draws: item.hover_draws(),
            press_draws: item.press_draws(),
        });
        self.item_refs.push((id, item.clone()));
    }

    /// Publish an item that can take a pointer press.
    ///
    /// The press is not necessarily taken by the topmost item under the
    /// pointer: an item that lets the press through hands it to the items
    /// below it.  So the items that take a press are published on their own,
    /// in paint order, and the UI thread walks them to find the one the press
    /// lands on.
    fn push_press_target(&mut self, item: &ItemRc, behavior: PressBehavior, origin: LogicalPoint) {
        let geometry = item.geometry();
        if geometry.size.is_empty() {
            return;
        }
        let window_origin = origin + item.map_to_window(geometry.origin).to_vector();
        self.press_targets.push(PressTarget {
            id: item_rc_as_id(item),
            geometry: LogicalRect::new(window_origin, geometry.size),
            behavior,
            focus_on_click: focus_on_click(item),
            pointer_reacts: item.pointer_reacts(),
            press_draws: item.press_draws(),
        });
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
        // The compiler lowered the control structure of this tree into slots:
        // which items can be pointed at, where the `visible` wrappers sit,
        // and where the repeated subtrees attach.  Enumerating the slots
        // replaces walking the tree item by item and re-discovering that
        // structure with a cast per item -- work nothing needs, since the
        // geometry and the enabled / visible values are all the runtime has
        // to answer anyway.
        let pinned = VRc::borrow_pin(component);
        let slots = pinned.as_ref().control_slots();
        self.walk_control_slots(component, &slots, origin);
    }

    fn walk_control_slots(
        &mut self,
        component: &i_slint_core::item_tree::ItemTreeRc,
        slots: &[ControlSlot],
        origin: LogicalPoint,
    ) {
        let mut index = 0;
        while index < slots.len() {
            let mut advance = 1;
            match slots[index] {
                // A `visible: false` wrapper: the wrapped subtree is off the
                // screen and holds no controls.  Its slots sit right after
                // this one, up to the flat index where the subtree ends.
                ControlSlot::Visibility { index, skip } => {
                    let item_rc = ItemRc::new(component.clone(), index);
                    let invisible = item_rc.downcast::<Clip>().is_some_and(|clip| {
                        let clip = clip.as_pin_ref();
                        clip.is_visibility_clip() && clip.clip()
                    });
                    if invisible {
                        // The wrapped subtree's slots sit right here in the
                        // list: the guard plus `skip` of them, all skipped.
                        advance = 1 + skip as usize;
                    }
                }
                ControlSlot::Repeater { index } => {
                    // Each instantiated row is a tree of its own, with a slot
                    // table of its own: enumerate them in place.
                    let pin = VRc::borrow_pin(component);
                    let range = pin.as_ref().get_subtree_range(index);
                    for row in range.start..range.end {
                        let mut row_tree: vtable::VWeak<ItemTreeVTable, vtable::Dyn> =
                            Default::default();
                        pin.as_ref().get_subtree(index, row, &mut row_tree);
                        if let Some(row_tree) = row_tree.upgrade() {
                            self.collect_interactive_controls(&row_tree, origin);
                        }
                    }
                }
                ControlSlot::Item { index, kind } => {
                    let item_rc = ItemRc::new(component.clone(), index);
                    match kind {
                        ControlKind::TouchArea => {
                            if let Some(item) = item_rc.downcast::<TouchArea>() {
                                self.push_control(&item_rc, origin);
                                let enabled = TouchArea::FIELD_OFFSETS
                                    .enabled()
                                    .apply_pin(item.as_pin_ref())
                                    .get();
                                if enabled {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                        ControlKind::TextInput => {
                            if let Some(item) = item_rc.downcast::<TextInput>() {
                                self.push_control(&item_rc, origin);
                                let enabled = TextInput::FIELD_OFFSETS
                                    .enabled()
                                    .apply_pin(item.as_pin_ref())
                                    .get();
                                if enabled {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                        ControlKind::FocusScope => {
                            if let Some(item) = item_rc.downcast::<FocusScope>() {
                                self.push_control(&item_rc, origin);
                                let item = item.as_pin_ref();
                                let enabled =
                                    FocusScope::FIELD_OFFSETS.enabled().apply_pin(item).get();
                                let focus_on_click = FocusScope::FIELD_OFFSETS
                                    .focus_on_click()
                                    .apply_pin(item)
                                    .get();
                                if enabled && focus_on_click {
                                    self.push_press_target(&item_rc, PressBehavior::Focus, origin);
                                }
                            }
                        }
                        ControlKind::Flickable => {
                            if let Some(item) = item_rc.downcast::<Flickable>() {
                                let item = item.as_pin_ref();
                                if item.interactive() && item.mouse_drag_pan_enabled() {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                        ControlKind::DragArea => {
                            if let Some(item) = item_rc.downcast::<DragArea>() {
                                let enabled = DragArea::FIELD_OFFSETS
                                    .enabled()
                                    .apply_pin(item.as_pin_ref())
                                    .get();
                                if enabled {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                        ControlKind::ContextMenu => {
                            if let Some(item) = item_rc.downcast::<ContextMenu>() {
                                let enabled = ContextMenu::FIELD_OFFSETS
                                    .enabled()
                                    .apply_pin(item.as_pin_ref())
                                    .get();
                                if enabled {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                        ControlKind::WindowMoveArea => {
                            if let Some(item) = item_rc.downcast::<WindowMoveArea>() {
                                let enabled = WindowMoveArea::FIELD_OFFSETS
                                    .enabled()
                                    .apply_pin(item.as_pin_ref())
                                    .get();
                                if enabled {
                                    self.push_press_target(&item_rc, PressBehavior::Takes, origin);
                                }
                            }
                        }
                    }
                }
            }
            index += advance;
        }
    }

    fn finish(self) -> ControlTable {
        ControlTable {
            controls: self.controls,
            item_refs: self.item_refs,
            press_targets: self.press_targets,
            focused: self.focused,
        }
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
        clip_item: Pin<&Clip>,
        _item_rc: &ItemRc,
        size: LogicalSize,
    ) -> i_slint_core::items::RenderingResult {
        // A visibility clip is what the compiler lowers `visible: false` into:
        // the item and everything below it is not on the screen at all, so
        // there is nothing here the user can point at, and walking the
        // subtree would read the geometry bindings of every item it holds --
        // for a spreadsheet's 2,600 invisible line edits, thousands of text
        // measurements for a control table that would list none of them.
        // The tree that draws skips the same subtree, so skipping here keeps
        // the walk's work in line with what is actually on screen.
        if clip_item.is_visibility_clip() && clip_item.clip() {
            return i_slint_core::items::RenderingResult::ContinueRenderingWithoutChildren;
        }
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
    /// The items that can take a pointer press, in paint order.
    pub press_targets: Vec<PressTarget>,
    /// The id of the item the tree has the focus on, if it is a published
    /// control. A `.slint` file can move the focus on its own, so the UI thread
    /// reads the answer here instead of trusting its own last decision.
    pub focused: Option<u64>,
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
        return Ok(ControlTable {
            controls: Vec::new(),
            item_refs: Vec::new(),
            press_targets: Vec::new(),
            focused: None,
        });
    }

    let mut encoder = ControlEncoder::new(window_inner);

    window_inner.draw_contents(|components, _post_render| {
        // Every tree the window draws is walked for controls, not just its own:
        // a menu is a popup tree of its own, and its items are controls the user
        // points at like any other. They come after the window's own items, so
        // the hit-test takes them first and the menu is on top where it is drawn.
        //
        // One walk does this. The draw-shaped walk the real renderer runs is of
        // no use here: every draw method on the encoder is empty, so the pass
        // would visit every item, evaluate its geometry for translate and clip
        // bookkeeping nothing consumes, and publish nothing. The collection
        // walk below finds the controls on its own.
        for (component, origin) in components {
            if let Some(component) = i_slint_core::item_tree::ItemTreeWeak::upgrade(component) {
                encoder.collect_interactive_controls(&component, *origin);
            }
        }
    });

    Ok(encoder.finish())
}
