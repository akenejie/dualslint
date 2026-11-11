// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! Property-level ownership: decide which thread owns each property.
//!
//! Drawing (render thread) vs OS communication (UI thread).
//! Classification is by property kind and is hardcoded here.
//! The compiler decides per-property ownership so the UI and render threads
//! hold only what they must; properties not affecting drawing live on the
//! UI side where OS communication happens.

use crate::object_tree::{Document, ElementRc};
use crate::object_tree::{PropertyOwnership, recurse_elem_including_sub_components_no_borrow};
use smol_str::SmolStr;
use std::collections::BTreeMap;

/// The classification, for the code generators: a write the generated code
/// makes has to know which thread the value belongs to.
pub fn classify_public(
    builtin_name: Option<&str>,
    prop: &str,
) -> crate::object_tree::PropertyOwnership {
    classify(builtin_name, prop)
}

fn classify(builtin_name: Option<&str>, prop: &str) -> PropertyOwnership {
    match prop {
        // OS communication / input routing / IME - UI owns
        "has-focus" => PropertyOwnership::Ui,
        "focus" | "clear-focus" => PropertyOwnership::Ui,
        "read-only" => PropertyOwnership::Ui,
        "input-type" | "input-hint" => PropertyOwnership::Ui,
        "action" => PropertyOwnership::Ui,
        "cursor-position" | "cursor-position-byte-offset" => PropertyOwnership::Ui,
        "selection-start" | "selection-end" => PropertyOwnership::Ui,
        "anchor-position" => PropertyOwnership::Ui,
        "text-cursor-width" => PropertyOwnership::Ui,
        "font-metrics" => PropertyOwnership::Ui,
        "focus-on-click" | "focus-on-tab-navigation" => PropertyOwnership::Ui,
        "mouse-cursor" => PropertyOwnership::Ui,
        "input-method-hints" => PropertyOwnership::Ui,
        "has-hover" | "pressed" => PropertyOwnership::Shared,
        "enabled" => PropertyOwnership::Shared,
        // Drawing/visual
        "text" => match builtin_name {
            Some("TextInput") => PropertyOwnership::Shared,
            _ => PropertyOwnership::Render,
        },
        "font-family"
        | "font-size"
        | "font-weight"
        | "font-italic"
        | "letter-spacing"
        | "word-spacing"
        | "horizontal-alignment"
        | "vertical-alignment"
        | "wrap"
        | "overflow"
        | "text-overflow"
        | "single-line"
        | "multiline"
        | "stroke-width"
        | "stroke-color" => PropertyOwnership::Render,
        "color" | "background" | "foreground" | "highlight-color" => PropertyOwnership::Render,
        "opacity" | "visible" | "clip" | "cache-rendering-hint" => PropertyOwnership::Render,
        "x" | "y" | "width" | "height" | "min-width" | "min-height" | "max-width"
        | "max-height" | "preferred-width" | "preferred-height" => PropertyOwnership::Render,
        "border-width" | "border-color" | "border-radius" => PropertyOwnership::Render,
        "drop-shadow-blur"
        | "drop-shadow-color"
        | "drop-shadow-offset-x"
        | "drop-shadow-offset-y" => PropertyOwnership::Render,
        "drop-shadow" | "box-shadow" => PropertyOwnership::Render,
        "source" | "image-fit" | "image-rendering" => PropertyOwnership::Render,
        "rotation-angle" => PropertyOwnership::Render,
        "accessible-role" | "accessible-label" | "accessible-description" => {
            PropertyOwnership::Shared
        }
        _ => PropertyOwnership::Render,
    }
}

fn is_focus_drawn(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    if e.change_callbacks.contains_key("has-focus") {
        return true;
    }
    for cb in [
        "focus-gained",
        "focus-lost",
        "focus-changed-event",
        "clicked",
        "pointer-event",
        "toggled",
        "pressed",
        "released",
        "accepted",
    ] {
        if e.binding(cb).is_some() {
            return true;
        }
    }
    let Some(b) = e.builtin_type() else {
        return false;
    };
    if !b.properties.contains_key("has-focus") {
        return false;
    }
    e.named_references.is_referenced("has-focus")
        || e.property_analysis.borrow().get("has-focus").is_some_and(|a| a.is_used())
        || b.name != "FocusScope"
}

fn is_ime_ui(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    let Some(b) = e.builtin_type() else {
        return false;
    };
    if b.name != "TextInput" {
        return false;
    }
    e.binding("input-method-hints").is_some()
        || e.named_references.is_referenced("input-method-hints")
        || e.property_analysis.borrow().get("input-method-hints").is_some_and(|a| a.is_used())
}

fn is_hover_drawn(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    let Some(b) = e.builtin_type() else {
        return false;
    };
    if b.name != "TouchArea" && b.name != "FocusScope" && b.name != "TextInput" {
        return false;
    }
    if e.named_references.is_referenced("has-hover") {
        return true;
    }
    e.property_analysis.borrow().get("has-hover").is_some_and(|a| a.is_used())
}

fn is_press_drawn(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    let Some(b) = e.builtin_type() else {
        return false;
    };
    if b.name != "TouchArea" && b.name != "FocusScope" && b.name != "TextInput" {
        return false;
    }
    if e.named_references.is_referenced("pressed") {
        return true;
    }
    e.property_analysis.borrow().get("pressed").is_some_and(|a| a.is_used())
}

pub fn property_ownership(doc: &Document) {
    let mut doc_focus_affects = false;
    let mut doc_ime_affects = false;
    doc.visit_all_used_components(|component| {
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            let e = elem.borrow();
            let bname = e.builtin_type().map(|b| b.name.clone());
            let mut map: BTreeMap<SmolStr, PropertyOwnership> = BTreeMap::new();
            for (prop, _) in e.property_declarations.iter() {
                map.insert(prop.clone(), classify(bname.as_deref(), prop));
            }
            for (prop, _) in e.bindings_including_synthetic() {
                map.entry(prop.clone()).or_insert_with(|| classify(bname.as_deref(), &prop));
            }
            drop(e);
            elem.borrow_mut().property_ownership = std::cell::RefCell::new(map);

            let focus_draws = is_focus_drawn(elem);
            let ime_ui = is_ime_ui(elem);
            let hover_draws = is_hover_drawn(elem);
            let press_draws = is_press_drawn(elem);
            let mut emut = elem.borrow_mut();
            emut.focus_draws = focus_draws;
            emut.ime_ui_owned = ime_ui;
            emut.hover_draws = hover_draws;
            emut.press_draws = press_draws;
            drop(emut);
            doc_focus_affects |= focus_draws;
            doc_ime_affects |= ime_ui;
        });
    });
    doc.focus_affects_rendering.set(Some(doc_focus_affects));
    doc.ime_affects_rendering.set(Some(doc_ime_affects));
}
