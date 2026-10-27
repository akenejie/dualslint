// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore optim
//! The Low Level Representation module

pub mod debug_info;
mod expression;
pub use expression::*;
mod item_tree;
pub use item_tree::*;
pub mod lower_expression;
pub mod lower_layout_expression;
pub mod lower_to_item_tree;
pub mod pretty_print;

/// Whether a value of this type can cross to the thread that draws the tree on
/// screen and come back.
///
/// A callback or function of a window-rooted component runs against the tree
/// the window shows, which a backend may draw on another thread, so its
/// arguments travel there and its return value comes back. That requires the
/// arguments to be `Send` and the return to have a default for the case where
/// the call did not return. Plain values qualify; a model or a callback does
/// not, because they are references into the tree the caller holds.
pub fn is_thread_portable_type(ty: &crate::langtype::Type) -> bool {
    use crate::langtype::Type;
    matches!(
        ty,
        Type::Void
            | Type::Int32
            | Type::Float32
            | Type::Bool
            | Type::String
            | Type::Color
            | Type::Keys
            | Type::Percent
            | Type::Angle
            | Type::Duration
            | Type::PhysicalLength
            | Type::LogicalLength
            | Type::Rem
    )
}

/// Whether a value of this type can cross to the thread that draws, by value.
///
/// `is_thread_portable_type` answers for the types a binding expression
/// produces; this one answers for property types too, which is what the
/// setter carry needs: a model property whose rows are plain records can
/// have those rows copied across, while one whose rows hold a nested model
/// cannot, because the row then carries a reference into the tree the
/// caller holds.
pub fn is_send_type(ty: &crate::langtype::Type) -> bool {
    use crate::langtype::Type;
    match ty {
        Type::Struct(s) => s.fields.values().all(is_send_field),
        other => is_thread_portable_type(other),
    }
}

/// Whether a value of this type can sit in a record that crosses to the thread
/// that draws. A struct field of array type is a model handle -- a reference
/// into the tree that owns it, not a copyable value -- so an array field keeps
/// its record home, however plain the element type looks.
fn is_send_field(ty: &crate::langtype::Type) -> bool {
    use crate::langtype::Type;
    match ty {
        Type::Array(_) => false,
        Type::Struct(s) => s.fields.values().all(is_send_field),
        other => is_thread_portable_type(other),
    }
}

/// The optimization passes over the LLR
pub mod optim_passes {
    pub mod count_property_use;
    mod inline_expressions;
    mod remove_unused;

    pub fn run_passes(root: &mut super::CompilationUnit) {
        count_property_use::count_property_use(root);
        inline_expressions::inline_simple_expressions(root);
        remove_unused::remove_unused(root);
    }
}
