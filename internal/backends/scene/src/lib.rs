// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// The data a 2-thread backend's render side and the UI side agree on.
//
// The render thread draws the item tree itself, so nothing here describes a
// frame: what crosses the thread boundary is the control table (where each
// interactive item is on screen) and the property values the UI tree and the
// render tree exchange.  `controls` walks the tree to produce that table; the
// `ControlPropertyValue` and `CSlintControlPropertyValue` shapes below are the
// C ABI's spelling of one property value.

use i_slint_core::graphics::euclid;
use i_slint_core::lengths::{LogicalRect, PhysicalPx};

pub mod controls;

/// Physical-pixel geometry, the space a rendering backend draws in.
pub type PhysicalLength = euclid::Length<f32, PhysicalPx>;
pub type PhysicalPoint = euclid::Point2D<f32, PhysicalPx>;
pub type PhysicalRect = euclid::Rect<f32, PhysicalPx>;

/// A value to assign to a borrowed (lent-out) control property.  The render
/// thread converts it to the concrete Slint property type (upstream
/// semantics, including detaching a previous binding) on the mirror tree.
#[derive(Clone, Debug, PartialEq)]
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

/// Tag of [`CSlintControlPropertyValue`], so the other end of the C ABI knows
/// which field of the struct is live.  The values are part of the ABI, and the
/// C++ side lists the same order in `api/cpp/include/slint.h`.
pub const SLINT_CONTROL_PROPERTY_BOOL: u32 = 0;
/// Tag for the numeric case; see [`SLINT_CONTROL_PROPERTY_BOOL`].
pub const SLINT_CONTROL_PROPERTY_NUMBER: u32 = 1;
/// Tag for the text case; see [`SLINT_CONTROL_PROPERTY_BOOL`].
pub const SLINT_CONTROL_PROPERTY_TEXT: u32 = 2;
/// Tag for the color case; see [`SLINT_CONTROL_PROPERTY_BOOL`].
pub const SLINT_CONTROL_PROPERTY_COLOR: u32 = 3;

/// [`ControlPropertyValue`] in the shape a C caller can pass it: a fixed-size
/// tagged struct that crosses by value, so the only pointer in it is the one the
/// text case needs.
///
/// `bool_` carries the boolean case, `number` the numeric case, and `text` a
/// NUL-terminated UTF-8 string the text case; `color` is four bytes in r, g, b,
/// a order.  Only the field named by `tag` is read, so the others are left
/// whatever the caller's stack or register block happened to hold.
#[repr(C)]
pub struct CSlintControlPropertyValue {
    /// One of the `SLINT_CONTROL_PROPERTY_*` tags, naming the live field.
    tag: u32,
    /// The value for the numeric case.
    number: f32,
    /// The value for the boolean case.
    bool_: bool,
    /// The value for the color case, in r, g, b, a order.
    color: [u8; 4],
    /// The value for the text case; null stands for the empty string.
    text: *const std::os::raw::c_char,
}

impl CSlintControlPropertyValue {
    /// Resolve the tag to a value, or `None` when the tag names no known kind.
    ///
    /// A caller that sends an unknown tag has no value to assign, so the
    /// function that receives it refuses the assignment rather than guessing
    /// which field was meant.
    ///
    /// # Safety
    ///
    /// When `tag` is [`SLINT_CONTROL_PROPERTY_TEXT`], `text` must be null or
    /// point to a NUL-terminated string.
    pub unsafe fn to_value(&self) -> Option<ControlPropertyValue> {
        let [r, g, b, a] = self.color;
        Some(match self.tag {
            SLINT_CONTROL_PROPERTY_BOOL => ControlPropertyValue::Bool(self.bool_),
            SLINT_CONTROL_PROPERTY_NUMBER => ControlPropertyValue::Number(self.number),
            // SAFETY: the caller promised the string is NUL-terminated, and it
            // is copied here before the borrow ends.
            SLINT_CONTROL_PROPERTY_TEXT => ControlPropertyValue::Text(if self.text.is_null() {
                String::new()
            } else {
                // SAFETY: as above.
                unsafe { std::ffi::CStr::from_ptr(self.text) }.to_string_lossy().into_owned()
            }),
            SLINT_CONTROL_PROPERTY_COLOR => ControlPropertyValue::Color { r, g, b, a },
            _ => return None,
        })
    }

    /// Build the C shape from a [`ControlPropertyValue`], for handing a value
    /// back out to a C caller.
    ///
    /// `text` is only read for the text case and is stored as given rather than
    /// copied, because a `CString` has no address to hand out that stays put.
    /// The caller therefore owns the buffer and keeps it alive for as long as
    /// the returned struct can be read.
    pub fn from_value(value: &ControlPropertyValue, text: *const std::os::raw::c_char) -> Self {
        let (tag, number, bool_, color) = match value {
            ControlPropertyValue::Bool(b) => (SLINT_CONTROL_PROPERTY_BOOL, 0., *b, [0; 4]),
            ControlPropertyValue::Number(n) => (SLINT_CONTROL_PROPERTY_NUMBER, *n, false, [0; 4]),
            ControlPropertyValue::Color { r, g, b, a } => {
                (SLINT_CONTROL_PROPERTY_COLOR, 0., false, [*r, *g, *b, *a])
            }
            // The caller passes the pointer to the live buffer it already made
            // for the string; an empty text still needs a non-null pointer,
            // which is what an empty CString gives.
            ControlPropertyValue::Text(_) => (SLINT_CONTROL_PROPERTY_TEXT, 0., false, [0; 4]),
        };
        let text =
            if matches!(value, ControlPropertyValue::Text(_)) { text } else { std::ptr::null() };
        Self { tag, number, bool_, color, text }
    }
}

/// Read one property of a component that was handed to the render thread.
///
/// `component` is the same pointer the component factory returned, `item` is
/// the address of the item that carried the query, and `property` is the
/// name the render thread could not resolve against its own items. The value
/// goes into `out`; returning `false` says the component declares no such
/// property, which is not an error but the answer "look elsewhere".
///
/// # Safety
///
/// `component` and `item` must be the pointers the render thread passed in, and
/// `property` must be a NUL-terminated string. When the tag written into `out`
/// is [`SLINT_CONTROL_PROPERTY_TEXT`], `out.text` must be a NUL-terminated
/// string that stays valid until this call returns.
pub type SlintRenderThreadPropertyRead = unsafe extern "C" fn(
    component: *const std::os::raw::c_void,
    item: *const std::os::raw::c_void,
    property: *const std::os::raw::c_char,
    out: *mut CSlintControlPropertyValue,
) -> bool;

/// Assign one property of a component that was handed to the render thread; the
/// counterpart of [`SlintRenderThreadPropertyRead`], with the same contract
/// plus `value`, which the callee only reads.
///
/// # Safety
///
/// The pointers must be the ones the render thread passed in, `property` and,
/// when `value->tag` is [`SLINT_CONTROL_PROPERTY_TEXT`], `value->text` must be
/// NUL-terminated strings.
pub type SlintRenderThreadPropertyWrite = unsafe extern "C" fn(
    component: *const std::os::raw::c_void,
    item: *const std::os::raw::c_void,
    property: *const std::os::raw::c_char,
    value: *const CSlintControlPropertyValue,
) -> bool;

/// The pair of calls that answer for a component the render thread draws, in
/// the shape a C caller passes them.
///
/// It is the counterpart of the Rust `ComponentPropertyAccess`: a `.slint`
/// widget's property belongs to the component around its items rather than to
/// any item, so only the application that created the component can say what
/// it is. Both calls run on the render thread.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CSlintRenderThreadPropertyAccess {
    /// Reads a property; a null one is read as "this component declares
    /// nothing", leaving every query to the render thread's own items.
    pub read: Option<SlintRenderThreadPropertyRead>,
    /// Assigns a property; a null one refuses every assignment.
    pub write: Option<SlintRenderThreadPropertyWrite>,
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

    /// The control a completed click belongs to, or `None`.
    ///
    /// A click is a press and a release over the same control, so this reads
    /// the state as it is *before* the event is folded in: after [`Self::apply`]
    /// the press is gone either way, and a release that never had a press is
    /// not a click. Dragging off a control and releasing over nothing is a
    /// cancelled click, not a click on whatever happened to be underneath.
    ///
    /// Ask this before calling `apply`, so the press is still there.
    pub fn clicked(&self, phase: PointerPhase, target: Option<u64>) -> Option<u64> {
        if phase != PointerPhase::Released {
            return None;
        }
        target.filter(|id| self.press == Some(*id) && self.hover == Some(*id))
    }
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

    #[test]
    fn a_release_over_the_pressed_control_is_a_click() {
        let mut state = PointerState::default();
        let press = state.clicked(PointerPhase::Pressed, Some(1));
        state.apply(PointerPhase::Pressed, Some(1));
        assert_eq!(press, None, "a press is not a click");
        assert_eq!(state.clicked(PointerPhase::Moved, Some(1)), None, "nor is a move");
        assert_eq!(state.clicked(PointerPhase::Released, Some(1)), Some(1));
    }

    #[test]
    fn a_release_away_from_the_pressed_control_is_not_a_click() {
        let mut state = PointerState::default();
        state.apply(PointerPhase::Pressed, Some(1));
        // Dragged off the control before letting go.
        state.apply(PointerPhase::Moved, Some(2));
        assert_eq!(state.clicked(PointerPhase::Released, Some(2)), None, "dragged off");
        assert_eq!(state.clicked(PointerPhase::Released, None), None, "released outside");
    }

    #[test]
    fn a_release_without_a_press_is_not_a_click() {
        let mut state = PointerState::default();
        state.apply(PointerPhase::Moved, Some(1));
        assert_eq!(state.clicked(PointerPhase::Released, Some(1)), None);
    }
}

#[cfg(test)]
mod c_abi_tests {
    use super::{
        CSlintControlPropertyValue, ControlPropertyValue, SLINT_CONTROL_PROPERTY_BOOL,
        SLINT_CONTROL_PROPERTY_COLOR, SLINT_CONTROL_PROPERTY_NUMBER, SLINT_CONTROL_PROPERTY_TEXT,
    };
    use std::mem::{align_of, offset_of, size_of};

    /// The C++ side declares this struct field for field in
    /// `api/cpp/include/slint.h` and reads the values straight out of it.  A
    /// field that moves here without moving there is read at the wrong offset,
    /// which corrupts memory instead of failing to compile, so the layout is
    /// pinned rather than left to whatever the compiler feels like.
    #[test]
    fn the_property_value_layout_matches_the_c_declaration() {
        type V = CSlintControlPropertyValue;
        assert_eq!(size_of::<V>(), 24);
        assert_eq!(align_of::<V>(), 8);
        assert_eq!(offset_of!(V, tag), 0);
        assert_eq!(offset_of!(V, number), 4);
        // `bool` is one byte and `color` needs no alignment, so the color sits
        // right after the flag rather than at the next pointer boundary.
        assert_eq!(offset_of!(V, bool_), 8);
        assert_eq!(offset_of!(V, color), 9);
        // The pointer is what forces the tail padding up to an aligned offset.
        assert_eq!(offset_of!(V, text), 16);
    }

    /// The tags are ABI, and the C++ `PropertyValue::Kind` enum lists the same
    /// four cases in the same order, so a bool must not be read as a number.
    #[test]
    fn the_tags_are_the_ones_the_cpp_enum_lists() {
        assert_eq!(
            [
                SLINT_CONTROL_PROPERTY_BOOL,
                SLINT_CONTROL_PROPERTY_NUMBER,
                SLINT_CONTROL_PROPERTY_TEXT,
                SLINT_CONTROL_PROPERTY_COLOR,
            ],
            [0, 1, 2, 3]
        );
    }

    /// Only the field the tag names may reach the value, so a caller that leaves
    /// the other fields uninitialised still gets what it asked for.
    #[test]
    fn only_the_tagged_field_is_read() {
        let mut value: CSlintControlPropertyValue = unsafe { std::mem::zeroed() };
        value.tag = SLINT_CONTROL_PROPERTY_BOOL;
        value.bool_ = true;
        // Everything else is still zero here, which is the point: the number and
        // the string must not leak into a boolean assignment.
        // SAFETY: the tag is not the text case, so no string is read.
        assert!(matches!(unsafe { value.to_value() }, Some(ControlPropertyValue::Bool(true))));

        value.tag = SLINT_CONTROL_PROPERTY_NUMBER;
        value.number = 0.5;
        value.bool_ = false;
        // SAFETY: as above.
        assert!(matches!(
            unsafe { value.to_value() },
            Some(ControlPropertyValue::Number(n)) if n == 0.5
        ));
    }

    /// A null string is the empty string, so a caller that has no text to send
    /// does not have to allocate one.
    #[test]
    fn a_null_string_reads_as_empty() {
        let mut value: CSlintControlPropertyValue = unsafe { std::mem::zeroed() };
        value.tag = SLINT_CONTROL_PROPERTY_TEXT;
        // SAFETY: a null `text` is explicitly allowed and read as empty.
        assert!(matches!(
            unsafe { value.to_value() },
            Some(ControlPropertyValue::Text(t)) if t.is_empty()
        ));
    }

    /// An unknown tag names no field, so there is no value to assign.
    #[test]
    fn an_unknown_tag_is_refused() {
        let mut value: CSlintControlPropertyValue = unsafe { std::mem::zeroed() };
        value.tag = 99;
        value.bool_ = true;
        // SAFETY: the tag is not the text case, so no string is read.
        assert!(unsafe { value.to_value() }.is_none());
    }
}
