// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Where an interpreter application's `invoke_*` lands when a backend draws a
//! tree of its own.
//!
//! A backend that took the window over builds a second tree from the factory
//! the component left behind, and from then on the instance the application
//! holds is not the one on screen. `invoke_*` has to land on the tree that is
//! drawn, carrying plain arguments there and their answer back, while a call
//! that carries something a thread cannot hold stays with the caller.
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
    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(3)), Some(Value::from(SharedString::from("first")))),
        "with no tree of its own, the window shows the caller's own tree"
    );
    assert_eq!(adapter.calls.get(), 1, "the call went through the hook");

    // The handover: a second tree exists, and the window shows it from now on.
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
        (Some(Value::from(3)), Some(Value::from(SharedString::from("first")))),
        "the tree nothing draws is left as it was"
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
    assert_eq!(app.get_property("count"), Ok(Value::from(1)), "the caller's own tree is untouched");
}

/// A call that carries a struct stays with the caller.
#[test]
fn a_call_that_cannot_cross_stays_with_the_caller() {
    let platform = PlatformShowingTree::install();
    let app = create_instance();
    let adapter = platform.adapter_of_last_window();

    let mirror = create_instance();
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));

    let mut point = slint_interpreter::Struct::default();
    point.set_field("x".into(), Value::from(7));
    app.invoke("set-struct", &[Value::Struct(point)]).unwrap();

    assert_eq!(
        count_and_note(&app),
        (Some(Value::from(7)), Some(Value::from(SharedString::from("struct")))),
        "a struct cannot travel, so the call ran on the caller's own tree"
    );
    assert_eq!(
        count_and_note(&mirror),
        (Some(Value::from(1)), Some(Value::from(SharedString::from("")))),
        "the tree that is drawn is left as it was"
    );
}
