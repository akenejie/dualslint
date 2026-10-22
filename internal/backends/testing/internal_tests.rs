// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! This module contains helper functions that are used for our internal tests within Slint

use crate::TestingWindow;
use i_slint_core::SharedString;
use i_slint_core::api::ComponentHandle;
pub use i_slint_core::input::BackendMouseEvent;
pub use i_slint_core::input::TouchPhase;
use i_slint_core::item_tree::ItemTreeVTable;
pub use i_slint_core::lengths::LogicalPoint;
pub use i_slint_core::platform::InternalEvent;
use i_slint_core::platform::WindowEvent;
pub use i_slint_core::window::PopupWindowLocation;
pub use i_slint_core::window::WindowInner;

/// Simulate a mouse click at `(x, y)` and release after a while at the same position
pub fn send_mouse_click<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    x: f32,
    y: f32,
) {
    crate::testing_backend::send_mouse_click(
        x,
        y,
        &WindowInner::from_pub(component.window()).window_adapter(),
    );
}

/// Simulate a press and release at `(x, y)`, carrying the UI thread's focus
/// choice on the press. Backends that decide the focus use this seam; here it
/// stands in for one, without a second tree.
pub fn send_mouse_click_with_ui_focus<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    x: f32,
    y: f32,
    ui_focused: Option<u64>,
) {
    crate::testing_backend::send_mouse_click_with_ui_focus(
        x,
        y,
        ui_focused,
        &WindowInner::from_pub(component.window()).window_adapter(),
    );
}

/// Simulate entering a sequence of ascii characters key by key.
pub fn send_keyboard_string_sequence<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    sequence: &str,
) {
    crate::testing_backend::send_keyboard_string_sequence(
        &SharedString::from(sequence),
        &WindowInner::from_pub(component.window()).window_adapter(),
    );
}

/// Simulate entering a keyboard shortcut or other "nested" character sequence
pub fn send_key_combo<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    keys: impl IntoIterator<Item = impl Into<char>>,
) {
    let keys: Vec<SharedString> = keys.into_iter().map(|k| k.into().into()).collect();
    send_key_combo_with_text(component, &keys);
}

/// Simulate entering a keyboard shortcut where each key is a text string.
///
/// Unlike [`send_key_combo`], each key can be an arbitrary string,
/// which supports multi-codepoint grapheme clusters (e.g. NFD-encoded Unicode).
pub fn send_key_combo_with_text<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    keys: &[SharedString],
) {
    for key in keys {
        send_keyboard_key_text(component, key, true);
    }
    for key in keys.iter().rev() {
        send_keyboard_key_text(component, key, false);
    }
}

/// Simulate a single key event with the given text (pressed or released).
///
/// Unlike [`send_keyboard_char`], the text is dispatched as a single event,
/// which supports multi-codepoint grapheme clusters.
pub fn send_keyboard_key_text<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    text: &SharedString,
    pressed: bool,
) {
    crate::testing_backend::send_keyboard_key_text(
        text,
        pressed,
        &WindowInner::from_pub(component.window()).window_adapter(),
    )
}

/// Simulate entering a sequence of ascii characters key by (pressed or released).
pub fn send_keyboard_char<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    ch: char,
    pressed: bool,
) {
    send_keyboard_key_text(component, &SharedString::from(ch), pressed)
}

/// Applies the specified scale factor to the window that's associated with the given component.
/// This overrides the value provided by the windowing system.
pub fn set_window_scale_factor<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    factor: f32,
) {
    component.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: factor });
}

/// Override the locale used for decimal separator detection in the given component's window.
pub fn set_locale<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    locale: &str,
) {
    let inner = WindowInner::from_pub(component.window());
    inner.context().set_locale(locale);
}

/// Send a platform pinch gesture event to the component's window.
///
/// `delta` is the incremental scale change (e.g. 0.0 for start, 0.5 for 50% increase).
/// The ScaleRotateGestureHandler accumulates deltas: `scale *= (1.0 + delta)`.
pub fn send_pinch_gesture<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    delta: f32,
    center_x: f32,
    center_y: f32,
    phase: i_slint_core::input::TouchPhase,
) {
    component.window().dispatch_event(WindowEvent::internal(
        i_slint_core::input::BackendMouseEvent::PinchGesture {
            position: i_slint_core::lengths::logical_point_from_api(
                i_slint_core::api::LogicalPosition::new(center_x, center_y),
            ),
            delta,
            phase,
        },
    ));
}

/// Send a rotation gesture event to the component's window.
///
/// `delta` is the incremental rotation in degrees using the Slint convention:
/// positive = clockwise. The handler accumulates deltas into its `rotation` property.
pub fn send_rotation_gesture<
    X: vtable::HasStaticVTable<ItemTreeVTable>,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    delta: f32,
    center_x: f32,
    center_y: f32,
    phase: i_slint_core::input::TouchPhase,
) {
    component.window().dispatch_event(WindowEvent::internal(
        i_slint_core::input::BackendMouseEvent::RotationGesture {
            position: i_slint_core::lengths::logical_point_from_api(
                i_slint_core::api::LogicalPosition::new(center_x, center_y),
            ),
            delta,
            phase,
        },
    ));
}

pub fn access_testing_window<R>(
    window: &i_slint_core::api::Window,
    callback: impl FnOnce(&TestingWindow) -> R,
) -> R {
    i_slint_core::window::WindowInner::from_pub(window)
        .window_adapter()
        .internal(i_slint_core::InternalToken)
        .and_then(|wa| (wa as &dyn core::any::Any).downcast_ref::<TestingWindow>())
        .map(callback)
        .expect("access_testing_window called without testing backend/adapter")
}

/// Runs a future to completion by polling the future and updating the mock time until the future is ready
pub fn block_on<R>(future: impl Future<Output = R>) -> R {
    let mut pinned = core::pin::pin!(future);
    let mut ctx = core::task::Context::from_waker(core::task::Waker::noop());
    loop {
        if let core::task::Poll::Ready(r) = pinned.as_mut().poll(&mut ctx) {
            return r;
        }
        let duration = i_slint_core::platform::duration_until_next_timer_update()
            .unwrap_or(core::time::Duration::from_secs(1));
        crate::testing_backend::mock_elapsed_time(duration.as_millis() as u64);
    }
}

/// The stable id of the item at `(x, y)` in the window's own tree, as the UI
/// thread would name it after a hit-test.
///
/// The id is structural, so a component and the tree a backend draws from it
/// answer the same id with their own copy of the same item. Tests use it to name
/// a control the way the UI thread does when it routes a key.
pub fn stable_id_at<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    x: f32,
    y: f32,
) -> Option<u64> {
    let inner = WindowInner::from_pub(component.window());
    let tree = inner.try_component()?;
    let point = i_slint_core::lengths::LogicalPoint::new(x, y);
    // Every ancestor contains the point, so the control is the smallest one
    // that does: the walk visits parents before children, so the last match is
    // the deepest item, which is the one a hit-test would resolve to.
    let mut found = None;
    i_slint_core::item_tree::visit_items(
        &tree,
        i_slint_core::item_tree::TraversalOrder::FrontToBack,
        |component, _item, index, _state| {
            let item = i_slint_core::item_tree::ItemRc::new(component.clone(), index);
            let geometry = item.geometry();
            if geometry.contains(point) && !geometry.size.is_empty() {
                found = Some(item.stable_id());
            }
            i_slint_core::item_tree::ItemVisitorResult::Continue(())
        },
        (),
    );
    found
}

/// Route keys to the item with this stable id, or let the tree route from its
/// focused item when `None`.
pub fn set_key_target<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    id: Option<u64>,
) {
    WindowInner::from_pub(component.window()).set_key_target(id);
}

/// Carry the UI thread's focus answer for the duration of one event, the way a
/// backend does when it sends a press whose `has-focus` only the UI thread knows.
pub fn set_ui_focused_id<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    id: Option<u64>,
) {
    WindowInner::from_pub(component.window()).set_ui_focused_id(id);
}

/// The item the tree itself has the focus on, as a stable id. Used to say what
/// the core-focused path does when a backend is not deciding the focus.
pub fn focused_item_id<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
) -> Option<u64> {
    WindowInner::from_pub(component.window())
        .focus_item
        .borrow()
        .upgrade()
        .map(|item| item.stable_id())
}

/// The stable id of the `FocusScope` at `(x, y)` in the window's tree, so a test
/// can name the control a press would focus without going through the backend.
pub fn focus_scope_id_at<
    X: vtable::HasStaticVTable<ItemTreeVTable> + 'static,
    Component: Into<vtable::VRc<ItemTreeVTable, X>> + ComponentHandle,
>(
    component: &Component,
    x: f32,
    y: f32,
) -> Option<u64> {
    let inner = WindowInner::from_pub(component.window());
    let tree = inner.try_component()?;
    let point = i_slint_core::lengths::LogicalPoint::new(x, y);
    let mut found = None;
    i_slint_core::item_tree::visit_items(
        &tree,
        i_slint_core::item_tree::TraversalOrder::FrontToBack,
        |component, _item, index, _state| {
            let item = i_slint_core::item_tree::ItemRc::new(component.clone(), index);
            if item.downcast::<i_slint_core::items::FocusScope>().is_some() {
                let geometry = item.geometry();
                if geometry.contains(point) && !geometry.size.is_empty() {
                    found = Some(item.stable_id());
                }
            }
            i_slint_core::item_tree::ItemVisitorResult::Continue(())
        },
        (),
    );
    found
}
