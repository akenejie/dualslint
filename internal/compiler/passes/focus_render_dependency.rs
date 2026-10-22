// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Decide whether moving keyboard focus can change what a window draws.
//!
//! The drawing thread holds the focus item once the UI thread has named it, but
//! it only needs to hear about a focus change when something on screen reacts to
//! it. That is visible in the elements: a binding reading `has-focus`, a bound
//! focus callback, or an item that paints its focus state itself. Nothing else
//! on screen depends on it, and a window without those can leave the screen
//! alone when focus moves.

use crate::object_tree::{Document, ElementRc, recurse_elem_including_sub_components_no_borrow};

/// The focus callbacks an application can bind on a focus-carrying element.
/// Their handlers run on every focus change and may change whatever they like,
/// so any of them makes focus drawing-relevant.
const FOCUS_CALLBACKS: [&str; 3] = ["focus-gained", "focus-lost", "focus-changed-event"];

/// Store the answer on the document for the code generators.
///
/// The walk has to see the builtin base types (`FocusScope`, `TextInput`, …),
/// which `resolve_native_classes` later replaces by the smallest class that
/// still carries the used members, so this runs before that pass.
pub fn focus_affects_rendering(doc: &Document) {
    let mut affects = false;
    doc.visit_all_used_components(|component| {
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            let draws = element_draws_on_focus_change(elem);
            // Record it on the element too: a control whose focus state draws
            // nothing is one the UI thread should not ask the drawing thread to
            // redraw for, and the published geometry carries this bit to it.
            elem.borrow_mut().focus_draws = draws;
            affects |= draws;
        });
    });
    doc.focus_affects_rendering.set(Some(affects));
}

/// Whether a focus change touching this element can show up on screen.
fn element_draws_on_focus_change(elem: &ElementRc) -> bool {
    let elem = elem.borrow();

    if FOCUS_CALLBACKS.iter().any(|name| elem.binding(name).is_some())
        || elem.change_callbacks.contains_key("has-focus")
    {
        return true;
    }

    // Only focus-carrying elements declare `has-focus`; an element reading one
    // of another registers the read under that other element, which the walk
    // over all elements covers in turn.
    let Some(builtin) = elem.builtin_type() else { return false };
    if !builtin.properties.contains_key("has-focus") {
        return false;
    }

    // Something reads this element's focus state, ...
    elem.named_references.is_referenced("has-focus")
        || elem
            .property_analysis
            .borrow()
            .get("has-focus")
            .is_some_and(|analysis| analysis.is_used())
        // ... or the item paints the state itself, as a text cursor does while it
        // blinks. A FocusScope only forwards focus: it draws nothing of its own.
        || builtin.name != "FocusScope"
}
