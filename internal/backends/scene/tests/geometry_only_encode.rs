// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// The control walk draws nothing, so it has no other way to give a container
// its geometry than to lay the tree out the way a drawing renderer would.  If
// it skipped that, it would publish where the previous frame put the controls.
// These tests pin the two consequences: the regions match the laid-out items,
// and a resize reaches them.

use i_slint_backend_scene::ControlRegion;
use i_slint_backend_scene::controls::encode_window_controls;
use i_slint_core::api::{LogicalSize, Window};
use slint_interpreter::{Compiler, ComponentHandle, ComponentInstance};

/// The things that make the walk interesting: a container that lays its
/// children out, a widget whose text has to be measured, and a control that
/// sticks out of the clip around it.
const SOURCE: &str = r#"
import { Button, LineEdit, VerticalBox } from "std-widgets.slint";

export component Controls {
    in property <int> count: 3;
    VerticalBox {
        padding: 12px;
        spacing: 8px;
        line-edit := LineEdit {
            text: "hello";
            width: 200px;
        }
        button := Button {
            text: "Add";
        }
        for i in count: Button {
            text: "row \{i}";
        }
        clip := Rectangle {
            width: 40px;
            height: 20px;
            LineEdit {
                text: "clipped";
                width: 200px;
            }
        }
    }
}
"#;

/// The control regions of a window, in a form that can be compared.
fn regions(window: &Window) -> Vec<(u64, i32, i32, u32, u32)> {
    encode_window_controls(window).expect("walks").controls.iter().map(region).collect()
}

fn region(c: &ControlRegion) -> (u64, i32, i32, u32, u32) {
    (
        c.id,
        c.geometry.origin.x as i32,
        c.geometry.origin.y as i32,
        c.geometry.size.width as u32,
        c.geometry.size.height as u32,
    )
}

/// Compile [`SOURCE`] and show it at `size`.
///
/// The instance is returned rather than the window because the window is a
/// borrow of it, and a walk visits a tree that is only alive as long as the
/// component is.  No layout pass is run here on purpose: the walk does it,
/// which is what the second test is about.
fn show(size: LogicalSize) -> ComponentInstance {
    i_slint_backend_testing::init_no_event_loop();
    let result =
        spin_on::spin_on(Compiler::default().build_from_source(SOURCE.into(), Default::default()));
    assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
    let instance: ComponentInstance =
        result.component("Controls").expect("compiles").create().expect("instantiable");
    instance.window().set_size(size);
    instance.window().show();
    instance
}

/// The first control is the `LineEdit` the component declared, and its width is
/// the one it asked for, because the container it sits in was laid out first.
/// A walk that did not lay the tree out would report the window's width here.
#[test]
fn the_walk_publishes_the_laid_out_geometry() {
    let instance = show(LogicalSize::new(400.0, 300.0));
    let regions = regions(instance.window());

    assert!(!regions.is_empty(), "the tree offers controls to point at");
    assert!(
        regions[0].1 >= 12 && regions[0].2 >= 12,
        "the first control sits inside the box's padding, at {:?}",
        regions[0]
    );
    // The declared 200px, less whatever the widget keeps for itself.  A walk
    // that had not laid the tree out would report the window's width instead.
    assert!(
        (150..=200).contains(&regions[0].3),
        "the first control kept the width it declared, at {:?}",
        regions[0]
    );
}

/// A resize changes the published geometry.  A pass that never called the
/// layout would leave the previous frame's numbers behind.
#[test]
fn a_resize_reaches_the_published_geometry() {
    let instance = show(LogicalSize::new(400.0, 300.0));
    let before = regions(instance.window());

    instance.window().set_size(LogicalSize::new(300.0, 200.0));
    let after = regions(instance.window());

    assert_ne!(before, after, "a resize reaches the published geometry");
}
