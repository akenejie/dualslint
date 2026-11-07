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
pub mod property_ownership;

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
    /// For integer properties (`TextInput.cursor-position-byte-offset`, ...).
    Int(i32),
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
/// Tag for the integer case; see [`SLINT_CONTROL_PROPERTY_BOOL`].
pub const SLINT_CONTROL_PROPERTY_INT: u32 = 4;

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
            SLINT_CONTROL_PROPERTY_INT => ControlPropertyValue::Int(self.number as i32),
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
            ControlPropertyValue::Int(i) => (SLINT_CONTROL_PROPERTY_INT, *i as f32, false, [0; 4]),
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
/// string that stays valid until the next read or write on the same thread:
/// the value is copied after this call returns, so a buffer this call owns and
/// destroys on its way out is gone by then.
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
    /// Whether a pointer press on this control moves the focus to it.  The
    /// focus is decided on the UI thread, which does not see the property, so
    /// the render thread — which does — tells it here.
    pub focus_on_click: bool,
    /// Whether this control joins the Tab traversal.  The UI thread owns which
    /// control has the focus, so it also owns what Tab moves through, and the
    /// render thread — which sees the property — publishes the set here.
    pub focus_on_tab: bool,
    /// Whether moving the focus onto this control can change what is drawn.
    ///
    /// The compiler worked this out per element from the `.slint` source and
    /// stored it in the item tree; the render thread is only publishing it.  A
    /// control whose focus state nothing reads and that does not paint it has
    /// no reason to be told about a focus change, so the UI thread keeps that
    /// control's focus to itself.
    pub focus_draws: bool,
    /// Whether this control has an input-method value the UI thread owns.
    ///
    /// The compiler worked this out per element and stored it in the item tree;
    /// the render thread is only publishing it.  A control with a value the
    /// platform's input method is told and nothing draws has it read from the
    /// UI thread's own tree rather than being handed it from here.
    pub ime_ui_owned: bool,
    pub hover_draws: bool,
    pub press_draws: bool,
}

/// What an item does with a pointer press.
///
/// The tree hands a press to the item under the pointer, and to the items
/// below it when that item lets the press through.  The UI thread owns the
/// pointer, so it has to know which item the press lands on; the render thread
/// owns the items, so it publishes what they do with a press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressBehavior {
    /// The item takes the press: the event ends there.
    Takes,
    /// The item takes the press only by taking the focus.  A press that does
    /// not move the focus falls through to the items below it.
    Focus,
}

/// An item that can take a pointer press.
#[derive(Clone, Copy, Debug)]
pub struct PressTarget {
    /// The id of the item, the same id the control regions use.
    pub id: u64,
    /// The item's rectangle in window-space logical pixels.
    pub geometry: LogicalRect,
    /// What the item does with the press.
    pub behavior: PressBehavior,
    /// Whether the item takes the focus when it is pressed.
    pub focus_on_click: bool,
    /// Whether a pointer event on this item, or on something inside it, can
    /// change what is on screen.
    ///
    /// The compiler worked this out per element and the render thread only
    /// publishes it.  A press on an item that changes nothing on screen needs
    /// not be sent on at all.
    pub pointer_reacts: bool,
    /// Whether this item's pressed state shows on screen: the pressed-state
    /// update is only sent for items whose visuals react to it.  The compiler
    /// worked this out per element, like `pointer_reacts`.
    pub press_draws: bool,
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
    /// Whether a pointer press here moves the focus, a property of the control
    /// that only the render thread can read.
    pub focus_on_click: bool,
    /// Whether Tab moves the focus through this control, a property of the
    /// control that only the render thread can read.
    pub focus_on_tab: bool,
    /// Whether moving the focus onto this control can change what is drawn.
    ///
    /// Published so the UI thread — which owns the focus — can tell whether a
    /// focus change is something the drawing thread has to hear about.
    pub focus_draws: bool,
    /// Whether this control has an input-method value the UI thread owns, so the
    /// UI thread reads it from its own tree rather than from the drawing side.
    pub ime_ui_owned: bool,
    pub hover_draws: bool,
    pub press_draws: bool,
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

/// Which measurement of a text control's caret to take.
///
/// The caret rectangle is drawn by the render thread, but its position is also
/// what the platform's input method is told, so the measuring is offered to any
/// thread that owns input rather than kept for the draw.  The offsets are in
/// bytes and are clamped to the text by the render thread, so a query from a UI
/// thread that does not know the text length cannot land out of bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorQuery {
    /// The caret's own rectangle, at the text control's current cursor.
    CursorRect,
    /// The caret rectangle at a given byte offset, used to describe the other
    /// end of a selection.
    RectAtByteOffset {
        /// The offset within the text, in bytes.
        byte_offset: usize,
    },
    /// The byte offset nearest a point in window coordinates, the reverse of
    /// [`Self::RectAtByteOffset`], for turning a pointer into a caret position.
    ByteOffsetAt {
        /// The x coordinate in window logical pixels.
        x: f32,
        /// The y coordinate in window logical pixels.
        y: f32,
    },
}

/// The answer to a [`CursorQuery`].
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct CursorGeometry {
    /// For the rectangle queries: origin and size, in window logical pixels.
    pub rect: Option<LogicalRect>,
    /// For [`CursorQuery::ByteOffsetAt`]: the offset within the text, in bytes.
    pub byte_offset: Option<usize>,
}

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
    /// The items that can take a pointer press, in paint order.
    ///
    /// Back to front, like the controls: the last one drawn is the first a
    /// press reaches.
    press_targets: Vec<PressTarget>,
    /// The item the tree held the focus on when this table was written.
    ///
    /// The UI thread owns which control keys go to, but a `.slint` file can move
    /// the drawing tree's focus on its own, so the answer is published back
    /// rather than remembered from the last click. Read it with
    /// [`Self::focused`].
    focused: Option<u64>,
}

impl PublishedControls {
    pub fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
        self.press_targets.clear();
        self.focused = None;
    }

    /// The control the drawing tree had the focus on at the last publish.
    pub fn focused(&self) -> Option<u64> {
        self.focused
    }

    /// Record which control the drawing tree has the focus on.
    pub fn set_focused(&mut self, focused: Option<u64>) {
        self.focused = focused;
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

    /// The items that can take a pointer press, in paint order: back to front.
    pub fn press_targets(&self) -> &[PressTarget] {
        &self.press_targets
    }

    /// Record the items that can take a pointer press.
    pub fn set_press_targets(&mut self, press_targets: Vec<PressTarget>) {
        self.press_targets = press_targets;
    }

    /// The item a press at the logical point lands on, front to back.
    ///
    /// The tree hands the press to the item under the pointer and, when that
    /// item lets the press through, to the items below it.  A focus scope is
    /// the one item that lets it through: it takes the press only by taking the
    /// focus, and only while neither focus answer names it.  The two answers
    /// are separate because a control whose focus state draws nothing has only
    /// the UI thread's.
    pub fn press_target_at(
        &self,
        x: f32,
        y: f32,
        tree_focused: Option<u64>,
        ui_focused: Option<u64>,
    ) -> Option<&PressTarget> {
        self.press_targets.iter().rev().find(|target| {
            let g = &target.geometry;
            let inside = x >= g.origin.x
                && x < g.origin.x + g.size.width
                && y >= g.origin.y
                && y < g.origin.y + g.size.height;
            inside
                && match target.behavior {
                    PressBehavior::Takes => true,
                    PressBehavior::Focus => {
                        tree_focused != Some(target.id) && ui_focused != Some(target.id)
                    }
                }
        })
    }

    /// The controls Tab moves through, in paint order.
    ///
    /// The UI thread owns the focus, so it also owns what Tab goes to next, and
    /// that order is the reverse of how the controls are painted: the last one
    /// drawn is the first a Tab reaches, the same way it is on top for a
    /// pointer.
    pub fn focus_order(&self) -> Vec<u64> {
        self.order
            .iter()
            .rev()
            .filter(|id| self.by_id.get(id).is_some_and(|c| c.focus_on_tab))
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod c_abi_tests {
    use super::{
        CSlintControlPropertyValue, ControlPropertyValue, SLINT_CONTROL_PROPERTY_BOOL,
        SLINT_CONTROL_PROPERTY_COLOR, SLINT_CONTROL_PROPERTY_INT, SLINT_CONTROL_PROPERTY_NUMBER,
        SLINT_CONTROL_PROPERTY_TEXT,
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
    /// five cases in the same order, so a bool must not be read as a number, nor
    /// an integer as one.
    #[test]
    fn the_tags_are_the_ones_the_cpp_enum_lists() {
        assert_eq!(
            [
                SLINT_CONTROL_PROPERTY_BOOL,
                SLINT_CONTROL_PROPERTY_NUMBER,
                SLINT_CONTROL_PROPERTY_TEXT,
                SLINT_CONTROL_PROPERTY_COLOR,
                SLINT_CONTROL_PROPERTY_INT,
            ],
            [0, 1, 2, 3, 4]
        );
    }

    /// An integer travels in the number field and comes back whole, since the
    /// int range is inside the exactly-representable range of the float it rides
    /// in.
    #[test]
    fn an_integer_survives_the_round_trip() {
        let value = CSlintControlPropertyValue::from_value(
            &ControlPropertyValue::Int(-1234),
            std::ptr::null(),
        );
        // SAFETY: the tag this wrote is the integer case, which reads no string.
        assert!(matches!(unsafe { value.to_value() }, Some(ControlPropertyValue::Int(-1234))));
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
