// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Decide, property by property, which thread holds the value.
//!
//! The tree on screen belongs to the drawing thread, so a property belongs to
//! it by default: the drawing thread lays the control out and paints it, and
//! the values that decide pixels live there.  A few properties are not: the
//! values the input side reads and nothing paints — the hints the platform's
//! input method is told, above all — exist only to talk to the input side,
//! and they belong to the thread that owns input.  A value both sides read
//! belongs to both.
//!
//! The decision is made here, from the elements, before either thread sees
//! them.  The code generators then place each value on the side that owns it,
//! and the published control metadata tells the UI thread what a control needs
//! instead of leaving it to guess.

use crate::object_tree::{
    Document, Element, ElementRc, PropertyOwnership,
    recurse_elem_including_sub_components_no_borrow,
};
use smol_str::SmolStr;
use std::collections::BTreeMap;

/// What the drawing side does with a property's value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drawing {
    /// The drawing side holds the value when the application reads it, when the
    /// item paints it, or when a callback on it runs in the tree.
    WhenUsed,
}

/// What the input side does with a property's value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    /// The input side reads or writes the value whatever the application does.
    Always,
    /// The input side reads or writes the value once the application set it.
    ///
    /// A property the application sets holds nothing until it does, so there
    /// is nothing for the input side to read before that.
    WhenSet,
}

/// The properties the two threads exchange over the control protocol, and what
/// each side does with them.
///
/// The list is the set of values a control's behaviour is made of: its focus
/// state, the values the input method is told, and the pointer state.  A
/// property that is not on the list is the drawing side's, because it lives in
/// the tree and the tree is what is drawn.
const RUNTIME_PROPERTIES: &[(&str, Drawing, Input)] = &[
    // The focus state: the input side asks whether a control has it, and the
    // drawing side paints it when the application or the item itself reads it.
    ("has-focus", Drawing::WhenUsed, Input::Always),
    // The hints the platform's input method is told.  Nothing paints them,
    // and they hold nothing until the application sets them.
    ("input-method-hints", Drawing::WhenUsed, Input::WhenSet),
    // The pointer state: the input side resolves it from where the pointer is,
    // and the drawing side reads it when the application binds it.
    ("pressed", Drawing::WhenUsed, Input::Always),
    ("has-hover", Drawing::WhenUsed, Input::Always),
    ("mouse-x", Drawing::WhenUsed, Input::Always),
    ("mouse-y", Drawing::WhenUsed, Input::Always),
    ("pressed-x", Drawing::WhenUsed, Input::Always),
    ("pressed-y", Drawing::WhenUsed, Input::Always),
    // The cursor the window shows while the pointer is over the control.  The
    // application sets it, and the input side is the one that shows it.
    ("mouse-cursor", Drawing::WhenUsed, Input::WhenSet),
];

/// The pointer properties, as the set the pointer answers are derived from.
///
/// These are the values a pointer leaves on a control: where it is and what it
/// does there.  The cursor is not among them: the application sets it rather
/// than reads it, so it says nothing about whether the application cares
/// about the pointer.
const POINTER_PROPERTIES: [&str; 6] =
    ["pressed", "has-hover", "mouse-x", "mouse-y", "pressed-x", "pressed-y"];

/// The callbacks an application can bind that a pointer event runs.
///
/// A handler runs in the tree, which is the drawing thread's, so a bound one
/// means a pointer event on this element has to reach that thread.
const POINTER_CALLBACKS: [&str; 5] =
    ["clicked", "double-clicked", "moved", "pointer-event", "scroll-event"];

/// The focus callbacks an application can bind.
///
/// Their handlers run on every focus change and may change whatever they like,
/// so any of them makes the focus state something the drawing side holds.
const FOCUS_CALLBACKS: [&str; 3] = ["focus-gained", "focus-lost", "focus-changed-event"];

/// Store the answer on every element, and derive from it the answers the
/// runtime asks about a control.
///
/// The walk has to see the builtin base types (`FocusScope`, `TextInput`, …),
/// which `resolve_native_classes` later replaces by the smallest class that
/// still carries the used members, so this runs before that pass.
pub fn property_ownership(doc: &Document) {
    let mut focus_affects = false;
    let mut ime_affects = false;

    doc.visit_all_used_components(|component| {
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            let ownership = element_property_ownership(elem);
            let answers = derived_answers(elem, &ownership);

            focus_affects |= answers.focus_draws;
            ime_affects |= answers.ime_ui_owned;

            let mut elem = elem.borrow_mut();
            elem.property_ownership = ownership.into();
            elem.focus_draws = answers.focus_draws;
            elem.ime_ui_owned = answers.ime_ui_owned;
            elem.pointer_reacts = answers.pointer_reacts;
        });
    });

    // The document-wide answers are what the code generators pass on to a
    // window that asks about them.  The per-control answers are what the
    // runtime uses; these stay as the window's fallback.
    doc.focus_affects_rendering.set(Some(focus_affects));
    doc.ime_affects_rendering.set(Some(ime_affects));
}

/// Which thread holds the value of each of this element's properties.
fn element_property_ownership(elem: &ElementRc) -> BTreeMap<SmolStr, PropertyOwnership> {
    let mut ownership = BTreeMap::new();
    let elem = elem.borrow();

    let Some(builtin) = elem.builtin_type() else { return ownership };

    // A focus callback the application bound runs on every focus change, in
    // the tree, so the drawing side holds the focus state even when nothing
    // reads it.
    let focus_callback_bound = FOCUS_CALLBACKS.iter().any(|name| elem.binding(name).is_some())
        || elem.change_callbacks.contains_key("has-focus");

    for (name, drawing, input) in RUNTIME_PROPERTIES {
        if !builtin.properties.contains_key(*name) {
            continue;
        }
        let drawing_reacts = *name == "has-focus" && focus_callback_bound;
        ownership.insert(
            (*name).into(),
            ownership_of(&elem, &builtin.name, name, *drawing, *input, drawing_reacts),
        );
    }

    ownership
}

/// The owner of one property's value.
fn ownership_of(
    elem: &Element,
    item_name: &str,
    name: &str,
    drawing: Drawing,
    input: Input,
    drawing_reacts: bool,
) -> PropertyOwnership {
    let drawing_needs = match drawing {
        Drawing::WhenUsed => {
            drawing_reacts || application_uses(elem, name) || item_paints(item_name, name)
        }
    };

    let input_needs = match input {
        Input::Always => true,
        Input::WhenSet => application_sets(elem, name) || application_uses(elem, name),
    };

    match (drawing_needs, input_needs) {
        (true, true) => PropertyOwnership::Shared,
        (true, false) => PropertyOwnership::Render,
        (false, true) => PropertyOwnership::Ui,
        // Nothing on either side asked for it, so it stays where the tree is.
        (false, false) => PropertyOwnership::Render,
    }
}

/// Whether the application reads or otherwise reacts to this element's
/// property.
///
/// The application is the `.slint` code: a binding that reads the property, an
/// expression that names it by id, a change callback on it.  All of it runs in
/// the tree, which is the drawing thread's.
fn application_uses(elem: &Element, name: &str) -> bool {
    elem.named_references.is_referenced(name)
        || elem.property_analysis.borrow().get(name).is_some_and(|a| a.is_used())
        || elem.change_callbacks.contains_key(name)
}

/// Whether the application gave this element's property a value.
///
/// A property the application sets holds nothing until it does, so there is
/// nothing for the input side to read before that.
fn application_sets(elem: &Element, name: &str) -> bool {
    elem.binding(name).is_some()
        || elem.property_analysis.borrow().get(name).is_some_and(|a| a.is_set)
}

/// Whether the item draws the property's value.
///
/// An item that carries focus paints its own focus state, the way a text
/// cursor blinks while it is focused; a `FocusScope` only forwards the focus
/// to the item that has it and paints nothing of its own.
fn item_paints(item_name: &str, name: &str) -> bool {
    match name {
        "has-focus" => item_name != "FocusScope",
        _ => false,
    }
}

/// The answers the runtime asks about a control, derived from the ownership of
/// its properties.
struct DerivedAnswers {
    /// Whether moving the focus onto this element can change what is drawn.
    focus_draws: bool,
    /// Whether this element has an input-method value the UI thread owns.
    ime_ui_owned: bool,
    /// Whether the application reacts to a pointer event on this element.
    pointer_reacts: bool,
}

/// Derive the answers the runtime asks about a control from the ownership of
/// its properties.
fn derived_answers(
    elem: &ElementRc,
    ownership: &BTreeMap<SmolStr, PropertyOwnership>,
) -> DerivedAnswers {
    let elem_ref = elem.borrow();

    // A pointer callback bound by the application runs in the tree, which is
    // the drawing side's, so a pointer event on this element has to reach it.
    let pointer_callback_bound =
        POINTER_CALLBACKS.iter().any(|name| elem_ref.binding(name).is_some());

    let focus_draws = ownership.get("has-focus").is_some_and(|owner| owner.render_holds());

    let ime_ui_owned = ownership
        .get("input-method-hints")
        .is_some_and(|owner| matches!(owner, PropertyOwnership::Ui));

    // The pointer state the application reads belongs to both threads, so the
    // drawing side has to be told what the pointer is doing.  A handler the
    // application bound says the same from the other end: the event has to
    // reach the code that answers it.
    let pointer_state_read = POINTER_PROPERTIES
        .iter()
        .any(|name| matches!(ownership.get(*name), Some(PropertyOwnership::Shared)));

    let pointer_reacts = pointer_callback_bound || pointer_state_read;

    DerivedAnswers { focus_draws, ime_ui_owned, pointer_reacts }
}
