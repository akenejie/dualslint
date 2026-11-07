// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! Property-level ownership: decide which thread owns each property.
//!
//! Drawing (render thread) vs OS communication (UI thread).
//! Classification is by property kind and is hardcoded here.
//! The compiler decides per-property ownership so the UI and render threads
//! hold only what they must; properties not affecting drawing live on the
//! UI side where OS communication happens.

use crate::expression_tree::Expression;
use crate::object_tree::{Document, ElementRc};
use crate::object_tree::{PropertyOwnership, recurse_elem_including_sub_components_no_borrow};
use smol_str::SmolStr;
use std::cell::RefCell;
use std::collections::BTreeMap;

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
        "font-family" | "font-size" | "font-weight" | "font-italic" | "letter-spacing"
        | "word-spacing" | "horizontal-alignment" | "vertical-alignment" | "wrap"
        | "overflow" | "text-overflow" | "single-line" | "multiline" | "stroke-width"
        | "stroke-color" => PropertyOwnership::Render,
        "color" | "background" | "foreground" | "highlight-color" => PropertyOwnership::Render,
        "opacity" | "visible" | "clip" | "cache-rendering-hint" => PropertyOwnership::Render,
        "x" | "y" | "width" | "height" | "min-width" | "min-height" | "max-width"
        | "max-height" | "preferred-width" | "preferred-height" => PropertyOwnership::Render,
        "border-width" | "border-color" | "border-radius" => PropertyOwnership::Render,
        "drop-shadow-blur" | "drop-shadow-color" | "drop-shadow-offset-x"
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
    for cb in ["focus-gained", "focus-lost", "focus-changed-event", "clicked", "pointer-event", "toggled", "pressed", "released", "accepted"] {
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
        || e.property_analysis
            .borrow()
            .get("has-focus")
            .is_some_and(|a| a.is_used())
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
        || e.property_analysis
            .borrow()
            .get("input-method-hints")
            .is_some_and(|a| a.is_used())
}

fn is_pointer_drawn(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    let Some(b) = e.builtin_type() else {
        return false;
    };
    if b.name != "TouchArea" && b.name != "FocusScope" && b.name != "TextInput" {
        return false;
    }
    if e.named_references.is_referenced("has-hover") || e.named_references.is_referenced("pressed")
    {
        return true;
    }
    for (p, a) in e.property_analysis.borrow().iter() {
        if (p == "has-hover" || p == "pressed") && a.is_used() {
            return true;
        }
    }
    false
}

fn is_pointer_handled(elem: &ElementRc) -> bool {
    let e = elem.borrow();
    if e.change_callbacks.contains_key("clicked")
        || e.change_callbacks.contains_key("pointer-event")
        || e.change_callbacks.contains_key("toggled")
        || e.change_callbacks.contains_key("accepted")
        || e.binding("clicked").is_some()
        || e.binding("pointer-event").is_some()
        || e.binding("toggled").is_some()
        || e.binding("accepted").is_some()
        || e.binding("pressed").is_some()
        || e.binding("released").is_some()
    {
        return true;
    }
    let Some(b) = e.builtin_type() else {
        return false;
    };
    // A text input moves its caret with the pointer by itself, so the press
    // has to reach the tree that draws it.
    if b.name == "TextInput" {
        return true;
    }
    is_pointer_drawn(elem)
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
                map.entry(prop.clone())
                    .or_insert_with(|| classify(bname.as_deref(), &prop));
            }
            drop(e);
            elem.borrow_mut().property_ownership = std::cell::RefCell::new(map);
        });
    });

    promote_cross_ownership_reads(doc);

    doc.visit_all_used_components(|component| {
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            let focus_draws = is_focus_drawn(elem);
            let ime_ui = is_ime_ui(elem);
            let pointer_draws = is_pointer_drawn(elem);
            let pointer_handled = is_pointer_handled(elem);
            let mut emut = elem.borrow_mut();
            emut.focus_draws = focus_draws;
            emut.ime_ui_owned = ime_ui;
            emut.pointer_draws = pointer_draws;
            emut.pointer_reacts = pointer_handled;
            drop(emut);
            doc_focus_affects |= focus_draws;
            doc_ime_affects |= ime_ui;
        });
    });
    doc.focus_affects_rendering.set(Some(doc_focus_affects));
    doc.ime_affects_rendering.set(Some(doc_ime_affects));
}

/// A property the UI owns whose value a drawing-side binding reads must live
/// on both threads: the mirror evaluates the binding, and the value it reads
/// has to be the one the UI thread keeps current.  The compiler walks every
/// binding and promotes the referenced Ui-owned properties to `Shared`, so
/// the value crosses instead of the mirror reading a default that nothing
/// updates.
fn promote_cross_ownership_reads(doc: &Document) {
    let mut elements: Vec<ElementRc> = Vec::new();
    doc.visit_all_used_components(|component| {
        recurse_elem_including_sub_components_no_borrow(component, &(), &mut |elem, _| {
            elements.push(elem.clone());
        });
    });

    let mut changed = true;
    let mut rounds = 0;
    while changed && rounds < 8 {
        changed = false;
        rounds += 1;
        for elem in &elements {
            let bindings: Vec<(SmolStr, Expression)> = {
                let e = elem.borrow();
                e.bindings_including_synthetic()
                    .map(|(n, b)| (n.clone(), b.borrow().expression.clone()))
                    .collect()
            };
            for (prop, binding) in &bindings {
                let bound_ownership = elem
                    .borrow()
                    .property_ownership
                    .borrow()
                    .get(prop)
                    .copied()
                    .unwrap_or(PropertyOwnership::Render);
                if bound_ownership == PropertyOwnership::Ui {
                    continue;
                }
                let mut refs: Vec<(ElementRc, SmolStr)> = Vec::new();
                binding.visit_recursive(&mut |sub: &Expression| {
                    if let Expression::PropertyReference(nr) = sub {
                        refs.push((nr.element(), nr.name().clone()));
                    }
                });
                for (target, name) in refs {
                    let owner_map = target.borrow().property_ownership.borrow().clone();
                    if owner_map.get(&name).copied() == Some(PropertyOwnership::Ui) {
                        drop(owner_map);
                        target
                            .borrow()
                            .property_ownership
                            .borrow_mut()
                            .insert(name, PropertyOwnership::Shared);
                        changed = true;
                    }
                }
            }
        }
    }
}
