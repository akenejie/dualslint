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

/// Bring up the testing backend and show a freshly built instance at `size`.
fn show(size: LogicalSize) -> ComponentInstance {
    i_slint_backend_testing::init_no_event_loop();
    build(size)
}

/// Compile [`SOURCE`] and show it at `size`.
///
/// The instance is returned rather than the window because the window is a
/// borrow of it, and a walk visits a tree that is only alive as long as the
/// component is.  No layout pass is run here on purpose: the walk does it,
/// which is what the second test is about.
fn build(size: LogicalSize) -> ComponentInstance {
    build_source(SOURCE, size)
}

fn build_source(source: &str, size: LogicalSize) -> ComponentInstance {
    let result =
        spin_on::spin_on(Compiler::default().build_from_source(source.into(), Default::default()));
    assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
    let instance: ComponentInstance =
        result.component("Controls").expect("compiles").create().expect("instantiable");
    instance.window().set_size(size);
    let _ = instance.window().show();
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

fn control_ids(window: &Window) -> Vec<u64> {
    regions(window).into_iter().map(|r| r.0).collect()
}

/// The whole point of a stable id is that two trees built from the same
/// component definition — the one the application holds and the one a backend
/// builds to draw — name the same control the same way, so the UI thread can
/// hand a click to its own tree by id.  Two instances of the same definition
/// are exactly that situation, without the render thread.
#[test]
fn separate_instances_agree_on_control_ids() {
    i_slint_backend_testing::init_no_event_loop();
    let a = build(LogicalSize::new(400.0, 300.0));
    let b = build(LogicalSize::new(400.0, 300.0));

    assert_eq!(
        control_ids(a.window()),
        control_ids(b.window()),
        "two trees from one definition name the same controls the same way"
    );
}

/// The id published for a control names an item of the tree it was walked from,
/// and `find_item_by_stable_id` recovers that item.  That is the lookup the UI
/// thread needs to hand a hit to its own tree by the id the render thread
/// published.
#[test]
fn a_published_id_finds_its_item() {
    let instance = show(LogicalSize::new(400.0, 300.0));
    let window = instance.window();
    let tree = i_slint_core::window::WindowInner::from_pub(window)
        .try_component()
        .expect("the window holds the component");
    let ids = control_ids(window);
    assert!(!ids.is_empty(), "there is something to find");
    for id in ids {
        let item = i_slint_core::item_tree::find_item_by_stable_id(&tree, id)
            .unwrap_or_else(|| panic!("id {id} names an item"));
        assert_eq!(item.stable_id(), id, "the lookup returns the item the id names");
    }
    assert!(
        i_slint_core::item_tree::find_item_by_stable_id(&tree, 0xdead_beef_dead_beef).is_none(),
        "an unknown id names nothing"
    );
}

/// A `for` loop expands to several instances of one definition, and the id has
/// to tell them apart even though they share element ids.
#[test]
fn repeated_controls_get_distinct_ids() {
    let instance = show(LogicalSize::new(400.0, 300.0));
    let ids = control_ids(instance.window());
    let unique: std::collections::HashSet<_> = ids.iter().collect();

    assert_eq!(unique.len(), ids.len(), "every control has its own id: {ids:?}");
}

/// The focus is decided on the UI thread, which never sees the items, so the
/// walk flattens each control's "does a press take the focus" into one bit the
/// UI can read.  A `TextInput` takes it whenever it is pressed.
#[test]
fn a_text_input_is_published_as_taking_the_focus_on_click() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                TextInput { text: "hello"; width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let controls = encode_window_controls(instance.window()).expect("walks").controls;

    assert!(!controls.is_empty(), "the tree offers controls to point at");
    assert!(
        controls.iter().all(|c| c.focus_on_click),
        "every control here is a text input, so every press takes the focus: {controls:?}"
    );
}

/// A `TouchArea` can be pointed at but is not something you can give the focus
/// to, and it must not be published as if it were.
#[test]
fn a_touch_area_is_not_published_as_taking_the_focus_on_click() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                TouchArea { width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let controls = encode_window_controls(instance.window()).expect("walks").controls;

    assert!(!controls.is_empty(), "the tree offers controls to point at");
    assert!(
        controls.iter().all(|c| !c.focus_on_click),
        "a touch area takes no focus on press: {controls:?}"
    );
}

/// A `FocusScope` says so itself with `focus-on-click`, and the walk passes the
/// property through rather than guessing from the item's type.
#[test]
fn a_focus_scope_publishes_its_focus_on_click_property() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                FocusScope { width: 100px; height: 20px; }
                FocusScope { focus-on-click: false; width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let controls = encode_window_controls(instance.window()).expect("walks").controls;
    let flags: Vec<bool> = controls.iter().map(|c| c.focus_on_click).collect();

    assert!(flags.iter().any(|f| *f), "the default focus scope takes the focus: {flags:?}");
    assert!(flags.iter().any(|f| !*f), "the opt-out focus scope does not: {flags:?}");
}

/// Tab moves the focus through the controls that join the traversal, and the
/// walk publishes that set so the UI thread can build the order itself.
#[test]
fn a_focus_scope_publishes_its_focus_on_tab_navigation_property() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                FocusScope { width: 100px; height: 20px; }
                FocusScope { focus-on-tab-navigation: false; width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let controls = encode_window_controls(instance.window()).expect("walks").controls;
    let flags: Vec<bool> = controls.iter().map(|c| c.focus_on_tab).collect();

    assert!(flags.iter().any(|f| *f), "the default focus scope is tabbable: {flags:?}");
    assert!(flags.iter().any(|f| !*f), "the opt-out focus scope is not: {flags:?}");
}

/// The press does not necessarily land on the topmost item: the tree hands it
/// down to the items below until one takes it.  So what the UI thread needs is
/// published per item — which items take a press, and which take it only by
/// taking the focus — rather than inferred from the controls.
#[test]
fn the_items_that_take_a_press_are_published_in_paint_order() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                TouchArea { width: 100px; height: 20px; }
                Flickable { width: 100px; height: 20px; interactive: true; }
                FocusScope { width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let targets = encode_window_controls(instance.window()).expect("walks").press_targets;
    let behaviors: Vec<i_slint_backend_scene::PressBehavior> =
        targets.iter().map(|t| t.behavior).collect();

    assert_eq!(
        behaviors,
        vec![
            i_slint_backend_scene::PressBehavior::Takes,
            i_slint_backend_scene::PressBehavior::Takes,
            i_slint_backend_scene::PressBehavior::Focus,
        ],
        "a touch area and a flickable take the press, a focus scope takes it with the focus"
    );
    assert!(
        targets.iter().all(|t| !t.geometry.size.is_empty()),
        "only items with something to point at are published: {targets:?}"
    );
}

/// An item that is disabled lets the press through, so it is not where the
/// press lands and publishing it would make the UI thread stop on it.
#[test]
fn a_disabled_item_takes_no_press() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                TouchArea { enabled: false; width: 100px; height: 20px; }
                TouchArea { width: 100px; height: 20px; }
                Flickable { width: 100px; height: 20px; interactive: false; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let targets = encode_window_controls(instance.window()).expect("walks").press_targets;

    assert_eq!(targets.len(), 1, "only the enabled touch area takes a press: {targets:?}");
}

/// What the press would change is the compiler's answer, carried through the
/// tree to the render thread and published with the item.  The UI thread reads
/// it to skip a press that cannot change a pixel.
#[test]
fn a_press_target_carries_whether_a_press_can_change_the_screen() {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(
        r#"
        export component Controls {
            VerticalLayout {
                TouchArea { width: 100px; height: 20px; clicked => { self.width += 1px; } }
                TouchArea { width: 100px; height: 20px; }
            }
        }
        "#,
        LogicalSize::new(400.0, 300.0),
    );
    let targets = encode_window_controls(instance.window()).expect("walks").press_targets;
    let reacts: Vec<bool> = targets.iter().map(|t| t.pointer_reacts).collect();

    assert_eq!(
        reacts,
        vec![true, false],
        "the touch area with a click handler reacts, the bare one does not"
    );
}

/// Two overlapping touch areas and a focus scope in front of both: enough to
/// tell front-to-back order apart from the order the tree paints them in.
const OVERLAPPING: &str = r#"
    export component Controls {
        back := TouchArea { x: 0; y: 0; width: 100px; height: 100px; }
        mid := TouchArea { x: 50px; y: 50px; width: 100px; height: 100px; }
        front := FocusScope { x: 60px; y: 60px; width: 100px; height: 100px; }
    }
"#;

fn published(source: &str) -> i_slint_backend_scene::PublishedControls {
    i_slint_backend_testing::init_no_event_loop();
    let instance = build_source(source, LogicalSize::new(400.0, 300.0));
    let table = encode_window_controls(instance.window()).expect("walks");
    let mut published = i_slint_backend_scene::PublishedControls::default();
    published.set_press_targets(table.press_targets);
    published
}

/// A press goes to the frontmost item that takes it.  Three items overlap at
/// `(70, 70)`; the focus scope is drawn last, so it is where the press lands.
#[test]
fn a_press_lands_on_the_frontmost_item_that_takes_it() {
    let table = published(OVERLAPPING);
    let targets = table.press_targets();
    assert_eq!(targets.len(), 3, "all three items are published: {targets:?}");
    let front = targets[2].id;

    let landed = table.press_target_at(70.0, 70.0, None, None);
    assert_eq!(landed.map(|t| t.id), Some(front), "the frontmost item takes the press");
}

/// A focus scope that already has the focus has nothing to do with the press,
/// so it lets it through to the items behind it.  The tree and the UI thread
/// keep separate focus answers, and either one saying "already focused" is
/// enough.
#[test]
fn a_focus_scope_that_has_the_focus_lets_the_press_through() {
    let table = published(OVERLAPPING);
    let targets = table.press_targets();
    let (mid, front) = (targets[1].id, targets[2].id);

    assert_eq!(
        table.press_target_at(70.0, 70.0, Some(front), None).map(|t| t.id),
        Some(mid),
        "the tree already focused the scope, so the press falls through"
    );
    assert_eq!(
        table.press_target_at(70.0, 70.0, None, Some(front)).map(|t| t.id),
        Some(mid),
        "the UI thread already focused the scope, so the press falls through"
    );
    assert_eq!(
        table.press_target_at(70.0, 70.0, Some(mid), None).map(|t| t.id),
        Some(front),
        "a different item having the focus does not make the scope let the press through"
    );
}

/// A touch area takes the press outright, so what is behind it never sees one,
/// whatever the focus is.
#[test]
fn a_touch_area_takes_the_press_whatever_the_focus_is() {
    let table = published(OVERLAPPING);
    let targets = table.press_targets();
    let mid = targets[1].id;

    assert_eq!(
        table.press_target_at(55.0, 55.0, Some(mid), Some(mid)).map(|t| t.id),
        Some(mid),
        "the press lands on the touch area even though it already has the focus"
    );
    assert!(
        table.press_target_at(250.0, 250.0, None, None).is_none(),
        "nothing is under the pointer there, so nothing takes the press"
    );
}
