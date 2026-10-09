// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Where an interpreter application's `invoke_*` lands when a backend draws a
//! tree of its own.
//!
//! A backend that took the window over builds a second tree from the factory
//! the component left behind, and from then on the instance the application
//! holds is not the one on screen. `invoke_*` has to land on the tree that is
//! drawn, carrying plain arguments there and their answer back, and a property
//! that can travel is read from there too, because that tree's bindings decide
//! what is seen. A call that carries something a thread cannot hold stays with
//! the caller.
//!
//! The adapter below is the seam the winit render thread implements: it says
//! which tree the window shows. It is the only way to have two trees of one
//! program in a test without a second graphics context.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use i_slint_core::api::{PhysicalSize, Window, WindowSize};
use i_slint_core::item_tree::ItemTreeRc;
use i_slint_core::platform::{Platform, PlatformError, WindowAdapter};
use i_slint_core::renderer::Renderer;
use i_slint_core::window::WindowInner;
use slint_interpreter::{Compiler, ComponentHandle, ComponentInstance, SharedString, Value};

const SOURCE: &str = r#"
    struct Pt { x: int }
    export component Rerouted inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> count: 1;
        in-out property <string> note: "";
        // A struct cannot travel, so a handover cannot carry this one.
        in-out property <Pt> point: { x: 0 };

        callback add(int, string);
        // A struct cannot travel with a call, so this one says what happens to
        // a call that carries something a thread cannot hold.
        callback set-struct(Pt);

        public function total() -> int { return count + 1; }

        add(amount, text) => {
            root.count += amount;
            root.note = text;
        }

        set-struct(point) => {
            root.count = point.x;
            root.note = "struct";
        }
    }

    // A timer is this tree's whole reason to be handed over: when a backend
    // takes the window, the timers this thread runs describe state nothing
    // draws, so they have to go on the shelf with the tree.
    export component Blink inherits Window {
        width: 320px;
        height: 240px;
        property <int> blinks: 0;
        Timer {
            interval: 1000ms;
            running: true;
            triggered => { root.blinks += 1; }
        }
    }

    // A timer plus a count that can travel: the count closes the gap at the
    // handover, and the drawing side's own timer is the one that keeps it
    // moving from then on.
    export component Tick inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> ticks: 0;
        Timer {
            interval: 1000ms;
            running: true;
            triggered => { root.ticks += 1; }
        }
    }
"#;

/// The window adapter of a backend that drew a tree of its own.
struct ShowingTree {
    inner: Rc<dyn WindowAdapter>,
    window: Window,
    shown: RefCell<Option<ItemTreeRc>>,
    calls: Cell<usize>,
}

impl WindowAdapter for ShowingTree {
    fn window(&self) -> &Window {
        &self.window
    }

    fn run_on_screen_tree(
        &self,
        local_tree: ItemTreeRc,
        task: Box<dyn FnOnce(&ItemTreeRc) + Send + 'static>,
    ) {
        self.calls.set(self.calls.get() + 1);
        match self.shown.borrow().clone() {
            Some(tree) => task(&tree),
            None => task(&local_tree),
        }
    }

    fn size(&self) -> PhysicalSize {
        self.inner.size()
    }

    fn renderer(&self) -> &dyn Renderer {
        self.inner.renderer()
    }

    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        self.inner.set_visible(visible)
    }

    fn set_size(&self, size: WindowSize) {
        self.inner.set_size(size)
    }

    fn request_redraw(&self) {
        self.inner.request_redraw()
    }
}

/// A platform whose windows report a tree of their own, standing in for the
/// render thread's.
struct PlatformShowingTree {
    inner: i_slint_backend_testing::TestingBackend,
    adapters: RefCell<Vec<Weak<ShowingTree>>>,
}

impl PlatformShowingTree {
    fn install() -> Rc<Self> {
        let platform = Rc::new(Self {
            inner: i_slint_backend_testing::TestingBackend::new(
                i_slint_backend_testing::TestingBackendOptions {
                    mock_time: true,
                    ..Default::default()
                },
            ),
            adapters: RefCell::new(Vec::new()),
        });
        i_slint_core::platform::set_platform(Box::new(PlatformHandle(platform.clone())))
            .expect("no platform is set on this thread yet");
        platform
    }

    fn adapter_of_last_window(self: &Rc<Self>) -> Rc<ShowingTree> {
        self.adapters
            .borrow()
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .next()
            .expect("a window asked the platform for an adapter")
    }
}

struct PlatformHandle(Rc<PlatformShowingTree>);

impl Platform for PlatformHandle {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let inner = self.0.inner.create_window_adapter()?;
        let adapter = Rc::new_cyclic(|weak: &Weak<ShowingTree>| ShowingTree {
            inner,
            window: Window::new(weak.clone()),
            shown: RefCell::new(None),
            calls: Cell::new(0),
        });
        self.0.adapters.borrow_mut().push(Rc::downgrade(&adapter));
        Ok(adapter)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        self.0.inner.duration_since_start()
    }
}

/// Build the component against the installed platform. It is `compile` from the
/// crate's other tests without installing a platform of its own.
fn create_instance() -> ComponentInstance {
    let result =
        spin_on::spin_on(Compiler::default().build_from_source(SOURCE.into(), Default::default()));
    assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
    result.component("Rerouted").expect("component should compile").create().unwrap()
}

/// Build the `Tick` component of [`SOURCE`], timers and all.
fn create_tick() -> ComponentInstance {
    let result =
        spin_on::spin_on(Compiler::default().build_from_source(SOURCE.into(), Default::default()));
    assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
    result.component("Tick").expect("component should compile").create().unwrap()
}

/// The tree behind an instance, which is the tree its window shows.
fn tree_of(instance: &ComponentInstance) -> ItemTreeRc {
    WindowInner::from_pub(instance.window())
        .try_component()
        .expect("a component is the tree of its window")
}

fn count_and_note(instance: &ComponentInstance) -> (Option<Value>, Option<Value>) {
    (instance.get_property("count").ok(), instance.get_property("note").ok())
}

#[test]
fn a_call_reaches_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    // Nothing has taken the window over, so the call lands on the caller's own
    // tree -- which is also what says the call goes through the hook at all.
    app.invoke("add", &[Value::from(2), Value::from(SharedString::from("first"))]).unwrap();
    assert_eq!(adapter.calls.get(), 1, "the call went through the hook");
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(3)), Some(Value::from(SharedString::from("first")))),
        "with no tree of its own, the window shows the caller's own tree"
    );

    // The handover: a second tree exists, and the window shows it from now on.
    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.invoke("add", &[Value::from(4), Value::from(SharedString::from("after"))]).unwrap();
    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(5)), Some(Value::from(SharedString::from("after")))),
        "the call landed on the tree that is drawn"
    );
}

/// What an application reads once a call has changed the tree that is drawn.
#[test]
fn a_read_comes_from_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.invoke("add", &[Value::from(4), Value::from(SharedString::from("after"))]).unwrap();
    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(5)), Some(Value::from(SharedString::from("after")))),
        "the call landed on the tree that is drawn"
    );
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(5)), Some(Value::from(SharedString::from("after")))),
        "and what the application reads is the state that tree holds"
    );
}

/// The number a function returns comes from the tree the window shows.
#[test]
fn a_functions_answer_comes_from_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    assert_eq!(app.invoke("total", &[]), Ok(Value::from(2)));

    let mirror = create_instance();
    mirror.set_property("count", Value::from(10)).unwrap();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    assert_eq!(
        app.invoke("total", &[]),
        Ok(Value::from(11)),
        "the answer comes from the tree that is drawn"
    );
    assert_eq!(
        app.get_property("count"),
        Ok(Value::from(10)),
        "and that is what the application reads"
    );
}

/// A call that carries a struct stays with the caller.
#[test]
fn a_call_that_cannot_cross_stays_with_the_caller() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    // Before a handover the window shows the caller's own tree, so a call that
    // cannot travel runs there all the same, and what the application reads is
    // the same tree.
    let mut point = slint_interpreter::Struct::default();
    point.set_field("x".into(), Value::from(7));
    app.invoke("set-struct", &[Value::Struct(point)]).unwrap();
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(7)), Some(Value::from(SharedString::from("struct")))),
        "a struct cannot travel, so the call ran on the caller's own tree"
    );

    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    // A struct cannot travel after the handover either: the call stays with the
    // caller, and the tree that is drawn is left as it was. What the application
    // reads afterwards is the tree the window draws, which the call never
    // reached.
    let mut point = slint_interpreter::Struct::default();
    point.set_field("x".into(), Value::from(9));
    app.invoke("set-struct", &[Value::Struct(point)]).unwrap();
    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(1)), Some(Value::from(SharedString::from("")))),
        "the tree that is drawn is left as it was"
    );
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(1)), Some(Value::from(SharedString::from("")))),
        "what the application reads is the tree the window draws, not the one the call wrote"
    );
}

/// A handover carries the state the application set onto the drawn tree.
#[test]
fn a_handover_carries_the_application_state_onto_the_drawn_tree() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    app.set_property("count", Value::from(7)).unwrap();
    app.set_property("note", Value::from(SharedString::from("the application"))).unwrap();
    let mut point = slint_interpreter::Struct::default();
    point.set_field("x".into(), Value::from(9));
    app.set_property("point", Value::Struct(point.clone())).unwrap();

    // The handover: a second tree exists and the window shows it from now on.
    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    i_slint_core::window::WindowInner::from_pub(app.window()).run_render_handover();

    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(7)), Some(Value::from(SharedString::from("the application")))),
        "the drawn tree was given the values the application set"
    );
    let Value::Struct(point) = mirror.get_property("point").unwrap() else {
        panic!("point is a struct")
    };
    assert_eq!(
        point.get_field("x"),
        Some(&Value::from(0)),
        "a struct cannot travel, so the drawn tree keeps the declared value"
    );
}

/// A value set after the handover reaches the drawn tree as well, so that the
/// bindings that decide what is seen see it.
#[test]
fn a_value_set_after_the_handover_reaches_the_drawn_tree() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    // The handover: a second tree exists and the window shows it from now on.
    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.set_property("count", Value::from(42)).unwrap();
    app.set_property("note", Value::from(SharedString::from("set later"))).unwrap();

    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(42)), Some(Value::from(SharedString::from("set later")))),
        "a value set after the handover reached the tree that is drawn"
    );
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(42)), Some(Value::from(SharedString::from("set later")))),
        "the value is on the caller's own tree too"
    );

    // A struct cannot travel, so setting one stays with the caller.
    let mut point = slint_interpreter::Struct::default();
    point.set_field("x".into(), Value::from(5));
    app.set_property("point", Value::Struct(point)).unwrap();
    let Value::Struct(point) = app.get_property("point").unwrap() else {
        panic!("point is a struct")
    };
    assert_eq!(point.get_field("x"), Some(&Value::from(5)), "the caller's tree holds the struct");
    let Value::Struct(point) = mirror.get_property("point").unwrap() else {
        panic!("point is a struct")
    };
    assert_eq!(
        point.get_field("x"),
        Some(&Value::from(0)),
        "a struct cannot travel, so the drawn tree keeps the declared value"
    );
}

/// A tree whose only reason to hand over is its timers sets a handover up all
/// the same, so the shelf closes on them the moment a backend takes the window.
#[test]
fn a_tree_that_only_ticks_sets_up_a_handover() {
    let platform = PlatformShowingTree::install();
    let app = {
        let result = spin_on::spin_on(
            Compiler::default().build_from_source(SOURCE.into(), Default::default()),
        );
        assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
        result.component("Blink").expect("component should compile").create().unwrap()
    };
    let adapter = platform.adapter_of_last_window();

    // Nothing a thread can carry, but the timers still need the shelf: without
    // a handover clause this tree would have been skipped entirely.
    let mirror = {
        let result = spin_on::spin_on(
            Compiler::default().build_from_source(SOURCE.into(), Default::default()),
        );
        assert!(!result.has_errors(), "{:?}", result.diagnostics().collect::<Vec<_>>());
        result.component("Blink").expect("component should compile").create().unwrap()
    };
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    WindowInner::from_pub(app.window()).run_render_handover();

    // The handover ran without carrying anything and put the tree on the
    // shelf. What the drawn tree does afterwards is its own business.
}

/// A tree that ticks and carries a count: the tick runs on the drawn tree once
/// the handover happened, and the count the application reads is the drawn
/// tree's.
#[test]
fn a_ticking_tree_hands_over_and_keeps_ticking_on_the_drawn_side() {
    let platform = PlatformShowingTree::install();
    let app = create_tick();
    let adapter = platform.adapter_of_last_window();

    // Before a backend takes the window over, the component's own timer runs
    // on this thread.
    i_slint_backend_testing::testing_backend::mock_elapsed_time(2000);
    i_slint_core::platform::update_timers_and_animations();
    let first = match app.get_property("ticks").unwrap() {
        Value::Number(n) => n as i64,
        other => panic!("ticks is no number: {other:?}"),
    };
    assert!(first >= 1, "the application's own timer counted before the handover");

    // Hand over: a second tree is drawn, and it gets the count so far.
    let mirror = create_tick();
    let after_handover = match app.get_property("ticks").unwrap() {
        Value::Number(n) => n as i64,
        other => panic!("ticks is no number: {other:?}"),
    };
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    WindowInner::from_pub(app.window()).run_render_handover();
    match mirror.get_property("ticks").unwrap() {
        Value::Number(n) if n as i64 == after_handover => {}
        other => panic!("the count did not travel with the handover: {other:?}"),
    }

    // More time passes: the drawn tree's own timer is the one that counts now,
    // the shelf having silenced the one this thread still calls ours.
    i_slint_backend_testing::testing_backend::mock_elapsed_time(3000);
    i_slint_core::platform::update_timers_and_animations();

    let mirror_now = match mirror.get_property("ticks").unwrap() {
        Value::Number(n) => n as i64,
        other => panic!("ticks is no number: {other:?}"),
    };
    assert!(mirror_now > after_handover, "the drawn tree keeps ticking");
    let app_now = match app.get_property("ticks").unwrap() {
        Value::Number(n) => n as i64,
        other => panic!("ticks is no number: {other:?}"),
    };
    assert_eq!(app_now, mirror_now, "what the application reads is the drawn tree's count");
}
