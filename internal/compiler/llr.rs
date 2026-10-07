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
