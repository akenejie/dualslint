// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Software-3.0

//! Decide which input-method values the UI thread owns rather than the drawing
//! thread.
//!
//! A control's properties are mostly the drawing thread's: it lays the control
//! out and paints it, so it holds the values that decide pixels.  A few are not.
//! The platform's input method is an input facility, and the values it is told
//! that nothing draws — the hints for what kind of text is expected, above all —
//! exist only to talk to it.  Those belong to the thread that owns input.  This
//! pass finds them, element by element, and records the answer so the code
//! generators can keep them off the drawing side.

use crate::object_tree::{Document, ElementRc, recurse_elem_including_sub_components_no_borrow};

/// Properties a `TextInput` offers to the input method that nothing draws.
///
/// Kept as a list rather than a check per element so that a later property with
/// the same character can be named here when it appears.
const IME_ONLY_PROPERTIES: [&str; 1] = ["input-method-hints"];

/// Store the answer on the document for the code generators.
///
/// Like the focus analysis it has to see the builtin base types (`TextInput`),
/// which `resolve_native_classes` later rewrites, so it runs before that pass.
pub fn ime_affects_rendering(doc: &Document) {
    let mut affects = false;
    doc.visit_all_used_components(|component| {
        if affects {
            return;
        }
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            affects |= element_uses_ime_only_property(elem);
        });
    });
    doc.ime_affects_rendering.set(Some(affects));
}

/// Whether this element carries an input-method-only property.
///
/// The answer is what the drawing thread needs to know: when it is `true` for
/// an element, that element has a value the input method is told and nothing
/// draws, so the value belongs to the UI thread rather than to the thread that
/// draws.  A `TextInput` without one of these properties is drawn from values
/// that are also drawn, and those stay where the drawing is.
fn element_uses_ime_only_property(elem: &ElementRc) -> bool {
    let elem = elem.borrow();

    let Some(builtin) = elem.builtin_type() else { return false };
    if builtin.name != "TextInput" {
        return false;
    }

    IME_ONLY_PROPERTIES.iter().any(|name| {
        // Set on the element, read by the application, or both: any of them is
        // the value existing, which is all that matters for who owns it.
        elem.binding(name).is_some()
            || elem.named_references.is_referenced(name)
            || elem.property_analysis.borrow().get(*name).is_some_and(|analysis| analysis.is_used())
    })
}
