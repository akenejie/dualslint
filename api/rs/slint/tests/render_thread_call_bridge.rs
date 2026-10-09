// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! What an application's call reaches when a backend draws a tree of its own.
//!
//! A component's tree cannot cross to the render thread, so a backend that took
//! the window over builds one of its own from the factory the component left
//! behind. From then on the component the application holds is not the one on
//! screen, and a call the application makes -- `invoke_*`, which is how
//! application code reaches a `.slint` callback or function -- has to land on the
//! tree that is drawn. These tests pin down that: the call reaches the tree the
//! window shows, the value it returns comes back from there, and the value an
//! application reads comes from there too, because the tree that is drawn is the
//! one whose bindings decide what is seen. A call that cannot travel is the
//! exception: it runs against the tree the caller holds, which nothing draws.
//!
//! A backend that draws a tree of its own says so by running the call against
//! that tree, which is what the adapter below does. It is the seam the winit
//! render thread implements, and it is the only way to have two trees of one
//! program in a test without a second graphics context.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use i_slint_core::api::PhysicalSize;
use i_slint_core::item_tree::ItemTreeRc;
use i_slint_core::platform::{Platform, PlatformError, WindowAdapter};
use i_slint_core::renderer::Renderer;
use i_slint_core::window::WindowInner;
use slint::{ComponentHandle, SharedString};

slint::slint! {
    export struct Pt {
        x: int,
    }

    export component BridgeApp inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> count: 1;
        in-out property <string> note: "";

        callback add(int, string);

        // A struct cannot travel with a call yet, so these two are here to say
        // what happens to a call that carries one.
        callback set-struct(Pt);

        public function total() -> int {
            return count + 1;
        }

        public function echo(point: Pt) -> int {
            return point.x;
        }

        add(amount, text) => {
            root.count += amount;
            root.note = text;
        }

        set-struct(point) => {
            root.count = point.x;
            root.note = "struct";
        }
    }

    // A timer is this component's whole reason to be hand a handover over:
    // when a backend takes the window, the timers this thread runs describe
    // state nothing draws, so they have to go on the shelf with the tree.
    export component Blinker inherits Window {
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
    export component TimerTick inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> ticks: 0;
        Timer {
            interval: 1000ms;
            running: true;
            triggered => { root.ticks += 1; }
        }
    }
}

/// The window adapter of a backend that drew a tree of its own.
///
/// Everything about the window is the testing backend's; the one thing this adds
/// is which tree the window shows, and that is the whole question here. Before a
/// handover is set up it is the tree the caller holds, so an application that
/// calls into its component while nothing has taken the window over still
/// reaches its own tree.
struct ShowingTree {
    inner: Rc<dyn WindowAdapter>,
    window: slint::Window,
    shown: RefCell<Option<ItemTreeRc>>,
    calls: Cell<usize>,
}

impl WindowAdapter for ShowingTree {
    fn window(&self) -> &slint::Window {
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

    fn set_size(&self, size: slint::WindowSize) {
        self.inner.set_size(size)
    }

    fn request_redraw(&self) {
        self.inner.request_redraw()
    }
}

/// A platform whose windows report a tree of their own, standing in for the
/// render thread's.
///
/// It is behind an `Rc` because `set_platform` takes a `'static` box while the
/// test needs a handle on the same platform to reach the adapters it handed out.
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

    /// The adapter the component's window asked for, which is the last one this
    /// platform handed out.
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
            window: slint::Window::new(weak.clone()),
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

/// The tree behind a component, which is the tree its window shows.
fn tree_of(component: &impl ComponentHandle) -> ItemTreeRc {
    WindowInner::from_pub(component.window())
        .try_component()
        .expect("a component is the tree of its window")
}

/// The second tree a handover would build: another instance of the same program,
/// which is what the factory the component left behind is for.
fn second_tree<T: ComponentHandle + 'static>(app: &T) -> T {
    let component = WindowInner::from_pub(app.window())
        .render_factory()
        .expect("generated code left a factory behind")()
    .downcast::<T>()
    .expect("the factory builds the same component");
    *component
}

#[test]
fn a_call_reaches_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // Nothing has taken the window over, so the call lands on the application's
    // own tree -- which is also what says the call goes through the hook at all.
    app.invoke_add(2, SharedString::from("before the handover"));
    assert_eq!(adapter.calls.get(), 1, "the call went through the hook");
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (3, "before the handover"),
        "with no tree of its own, the window shows the caller's own tree"
    );

    // The handover: a second tree exists, and the window shows it from now on.
    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.invoke_add(4, SharedString::from("after the handover"));
    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (5, "after the handover"),
        "the call landed on the tree that is drawn"
    );
}

#[test]
fn the_value_a_call_returns_comes_from_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // A function answers with a value, and the value is the tree's: `total`
    // reads `count` where it runs, so the two trees cannot be told apart by
    // anything but the number that comes back.
    assert_eq!(app.invoke_total(), 2, "the caller's own tree says count + 1");

    let mirror = second_tree(&app);
    mirror.set_count(41);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    assert_eq!(
        app.invoke_total(),
        42,
        "the answer came from the tree that is drawn, not from the caller's tree"
    );
    assert_eq!(app.get_count(), 41, "and what the application reads is the value that tree holds");
}

#[test]
fn a_call_that_cannot_travel_stays_with_the_caller() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // Before a handover the window shows the caller's own tree, so a call that
    // cannot travel runs there all the same, and what the application reads is
    // the same tree.
    app.invoke_set_struct(Pt { x: 9 });
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (9, "struct"),
        "the call ran against the tree the caller holds"
    );

    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    // A struct is not `Send` as far as the bridge is concerned, so a call that
    // carries one stays on the tree the caller holds. That is a limitation, not
    // a decision about which tree is right: the window shows the other tree, and
    // this call does not reach it. Until a struct of basic fields counts as
    // portable, that is what a caller has to expect of one. What the application
    // reads afterwards is the tree the window draws, which the call left alone.
    app.invoke_set_struct(Pt { x: 7 });
    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (1, ""),
        "and did not reach the tree that is drawn"
    );
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (1, ""),
        "so what the application reads is the tree the window draws, not the one the call wrote"
    );

    // The value comes back from the tree that ran the call, whichever that was.
    assert_eq!(app.invoke_echo(Pt { x: 5 }), 5);
}

/// What an application reads once a call has changed the tree that is drawn.
#[test]
fn a_read_comes_from_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.invoke_add(4, SharedString::from("after the handover"));

    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (5, "after the handover"),
        "the call landed on the tree that is drawn"
    );
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (5, "after the handover"),
        "and what the application reads is the state that tree holds"
    );
}

/// A handover carries the state the application set onto the drawn tree.
#[test]
fn a_handover_carries_the_application_state_onto_the_drawn_tree() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    app.set_count(7);
    app.set_note(SharedString::from("the application"));

    // The handover: a second tree exists, and the window shows it from now on.
    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    WindowInner::from_pub(app.window()).run_render_handover();

    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (7, "the application"),
        "the drawn tree was given the values the application set"
    );
}

/// A value the application assigns after the handover reaches the drawn tree.
#[test]
fn a_value_set_after_the_handover_reaches_the_drawn_tree() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // The handover: a second tree exists, and the window shows it from now on.
    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    app.set_count(42);
    app.set_note(SharedString::from("set later"));

    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (42, "set later"),
        "the tree that is drawn was given the value the application assigned"
    );
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (42, "set later"),
        "and the tree the application reads back says the same thing"
    );
}

/// A tree whose only reason to hand over is its timers sets a handover up all
/// the same, so the shelf closes on them the moment a backend takes the window.
#[test]
fn a_tree_that_only_ticks_sets_up_a_handover() {
    let platform = PlatformShowingTree::install();
    let app = Blinker::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // Nothing a thread can carry, but the timers still need the shelf: without
    // a handover clause this tree would have been skipped entirely.
    let mirror = second_tree(&app);
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
    let app = TimerTick::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    // Before a backend takes the window over, the component's own timer runs
    // on this thread.
    i_slint_backend_testing::testing_backend::mock_elapsed_time(2000);
    i_slint_core::platform::update_timers_and_animations();
    assert!(app.get_ticks() >= 1, "the application's own timer counted before the handover");

    // Hand over: a second tree is drawn, and it gets the count so far.
    let mirror = second_tree(&app);
    let after_handover = app.get_ticks();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    WindowInner::from_pub(app.window()).run_render_handover();
    assert_eq!(mirror.get_ticks(), after_handover, "the count traveled with the handover");

    // More time passes: the drawn tree's own timer is the one that counts now,
    // the shelf having silenced the one this thread still calls ours.
    i_slint_backend_testing::testing_backend::mock_elapsed_time(3000);
    i_slint_core::platform::update_timers_and_animations();

    let mirror_now = mirror.get_ticks();
    assert!(mirror_now > after_handover, "the drawn tree keeps ticking");
    assert_eq!(app.get_ticks(), mirror_now, "what the application reads is the drawn tree's count");
}
