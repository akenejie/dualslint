// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! Property ownership table shared by compiler and runtime.
//! Classifies each property as Render/Ui/Shared by property kind (hardcoded).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyOwnership {
    Render,
    Ui,
    Shared,
}

impl PropertyOwnership {
    pub fn render_holds(self) -> bool {
        matches!(self, Self::Render | Self::Shared)
    }
    pub fn ui_holds(self) -> bool {
        matches!(self, Self::Ui | Self::Shared)
    }
}

pub fn classify(builtin_name: Option<&str>, prop: &str) -> PropertyOwnership {
    match prop {
        "has-focus"
        | "focus"
        | "clear-focus"
        | "read-only"
        | "input-type"
        | "input-hint"
        | "action"
        | "cursor-position"
        | "cursor-position-byte-offset"
        | "selection-start"
        | "selection-end"
        | "anchor-position"
        | "text-cursor-width"
        | "font-metrics"
        | "focus-on-click"
        | "focus-on-tab-navigation"
        | "mouse-cursor"
        | "input-method-hints" => PropertyOwnership::Ui,
        "has-hover" | "pressed" | "enabled" => PropertyOwnership::Shared,
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
        | "stroke-color"
        | "color"
        | "background"
        | "foreground"
        | "highlight-color"
        | "opacity"
        | "visible"
        | "clip"
        | "cache-rendering-hint"
        | "x"
        | "y"
        | "width"
        | "height"
        | "min-width"
        | "min-height"
        | "max-width"
        | "max-height"
        | "preferred-width"
        | "preferred-height"
        | "border-width"
        | "border-color"
        | "border-radius"
        | "drop-shadow-blur"
        | "drop-shadow-color"
        | "drop-shadow-offset-x"
        | "drop-shadow-offset-y"
        | "drop-shadow"
        | "box-shadow"
        | "source"
        | "image-fit"
        | "image-rendering"
        | "rotation-angle" => PropertyOwnership::Render,
        "accessible-role" | "accessible-description" | "accessible-label" => {
            PropertyOwnership::Shared
        }
        _ => PropertyOwnership::Render,
    }
}
