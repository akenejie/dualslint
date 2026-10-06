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
//! window shows, the value it returns comes back from there, and the caller's own
//! tree is left alone.
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
fn second_tree(app: &BridgeApp) -> BridgeApp {
    let component = WindowInner::from_pub(app.window())
        .render_factory()
        .expect("generated code left a factory behind")()
    .downcast::<BridgeApp>()
    .expect("the factory builds a BridgeApp");
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
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (3, "before the handover"),
        "with no tree of its own, the window shows the caller's own tree"
    );
    assert_eq!(adapter.calls.get(), 1, "the call went through the hook");

    // The handover: a second tree exists, and the window shows it from now on.
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
        (3, "before the handover"),
        "the tree nothing draws is left as it was"
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
    assert_eq!(app.get_count(), 1, "the caller's own tree is untouched");
}

#[test]
fn a_call_that_cannot_travel_stays_with_the_caller() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    // A struct is not `Send` as far as the bridge is concerned, so a call that
    // carries one stays on the tree the caller holds. That is a limitation, not
    // a decision about which tree is right: the window shows the other tree, and
    // this call does not reach it. Until a struct of basic fields counts as
    // portable, that is what a caller has to expect of one.
    app.invoke_set_struct(Pt { x: 9 });
    assert_eq!(
        (app.get_count(), app.get_note().as_str()),
        (9, "struct"),
        "the call ran against the tree the caller holds"
    );
    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (1, ""),
        "and did not reach the tree that is drawn"
    );

    // The value comes back from the tree that ran the call, whichever that was.
    assert_eq!(app.invoke_echo(Pt { x: 5 }), 5);
}
