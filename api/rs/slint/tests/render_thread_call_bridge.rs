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

    // A focus scope the window can be clicked on: whether the core moves the
    // focus itself is the whole question here.
    export component FocusApp inherits Window {
        width: 320px;
        height: 240px;
        out property <bool> scope-focused: scope.has-focus;
        scope := FocusScope {
            width: 100%;
            height: 100%;
        }
    }

    // Two focus scopes that each count the keys they are handed, so a key's
    // route can be told apart by which counter it moved.
    export component TwoScopes inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> left-keys: 0;
        in-out property <int> right-keys: 0;
        left := FocusScope {
            x: 0px;
            width: 160px;
            height: 240px;
            key-pressed(event) => { root.left-keys += 1; EventResult.accept }
        }
        right := FocusScope {
            x: 160px;
            width: 160px;
            height: 240px;
            key-pressed(event) => { root.right-keys += 1; EventResult.accept }
        }
    }

    // A focus scope whose focus state is read by nothing, so the compiler drops
    // it from the drawing side. A backend that decides the focus keeps it on
    // the UI thread, and the press still has to see that the scope is focused:
    // a focused scope ignores the press instead of consuming it. A `TouchArea`
    // behind it counts the presses that reach the tree, because a consumed press
    // never gets there.
    export component QuietFocusApp inherits Window {
        width: 320px;
        height: 240px;
        in-out property <int> reached: 0;
        area := TouchArea {
            width: 100%;
            height: 100%;
            clicked => { root.reached += 1; }
        }
        scope := FocusScope {
            width: 100%;
            height: 100%;
            focus-on-click: true;
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
    /// Whether the focus is decided on the UI thread, the way the render
    /// thread's window does it.  Off by default, which is the single-threaded
    /// case where the core focuses a clicked `FocusScope` itself.
    focus_decided_by_ui: Cell<bool>,
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

    fn focus_is_decided_by_ui(&self) -> bool {
        self.focus_decided_by_ui.get()
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
            focus_decided_by_ui: Cell::new(false),
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

/// A handler the application registered runs on the thread the application
/// lives on, even when the call that reaches it comes from the tree the window
/// draws.
#[test]
fn a_host_handler_is_reached_from_the_tree_the_window_shows() {
    let platform = PlatformShowingTree::install();
    let app = BridgeApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();

    let seen = Rc::new(RefCell::new(Vec::<(i32, String)>::new()));
    app.on_add({
        let seen = seen.clone();
        move |amount, text| seen.borrow_mut().push((amount, text.to_string()))
    });

    // The handover: a second tree exists, and the window shows it from now on.
    let mirror = second_tree(&app);
    *adapter.shown.borrow_mut() = Some(tree_of(&mirror));
    WindowInner::from_pub(app.window()).run_render_handover();

    // The call goes where the window draws, and the drawn tree reaches back to
    // the handler the application registered.
    app.invoke_add(3, SharedString::from("from the drawn tree"));

    assert_eq!(
        seen.borrow().as_slice(),
        [(3, "from the drawn tree".to_string())],
        "the drawn tree reached the application's own handler"
    );
    assert_eq!(
        (mirror.get_count(), mirror.get_note().as_str()),
        (1, ""),
        "and the `.slint` body stayed out of the way, as a host handler replaces it"
    );
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

/// With no second thread, the core focuses a clicked `FocusScope` itself, which
/// is the behaviour every single-threaded backend relies on.
#[test]
fn the_core_focuses_a_clicked_focus_scope_when_it_decides_the_focus() {
    let platform = PlatformShowingTree::install();
    let app = FocusApp::new().expect("the platform makes windows");
    let _ = platform;

    assert!(!app.get_scope_focused(), "nothing is focused to start with");
    i_slint_backend_testing::send_mouse_click(&app, 160.0, 120.0);

    assert!(app.get_scope_focused(), "the clicked focus scope took the focus");
}

/// A window that draws a tree of its own hands the focus decision to the UI
/// thread, so the core must not move the focus to the clicked `FocusScope`.  The
/// UI thread does that itself, with the id the render thread published.
#[test]
fn a_window_that_draws_its_own_tree_leaves_the_focus_to_the_ui_thread() {
    let platform = PlatformShowingTree::install();
    let app = FocusApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();
    adapter.focus_decided_by_ui.set(true);

    i_slint_backend_testing::send_mouse_click(&app, 160.0, 120.0);

    assert!(!app.get_scope_focused(), "the core did not take the focus the UI thread is to decide");
}

/// Keys go to the control the UI thread names, not to whatever the tree renders
/// as focused.
///
/// This is the seam the UI-owned focus needs: the focus a control draws from and
/// the control a key is routed to are two different answers, and only the UI
/// thread holds the second. A key with no target still follows the focus, so the
/// single-threaded path is unchanged.
#[test]
fn a_named_control_receives_the_key_whatever_the_focus_is() {
    let platform = PlatformShowingTree::install();
    let app = TwoScopes::new().expect("the platform makes windows");
    let _ = platform;

    let left = i_slint_backend_testing::stable_id_at(&app, 80.0, 120.0)
        .expect("the left scope covers the left half");
    let right = i_slint_backend_testing::stable_id_at(&app, 240.0, 120.0)
        .expect("the right scope covers the right half");
    assert_ne!(left, right, "the two scopes are different items");

    // Nothing focused yet, so a key follows nothing and reaches no scope.
    i_slint_backend_testing::send_keyboard_string_sequence(&app, "a");
    assert_eq!((app.get_left_keys(), app.get_right_keys()), (0, 0));

    // Naming the right scope routes the key there, without moving any focus.
    i_slint_backend_testing::set_key_target(&app, Some(right));
    i_slint_backend_testing::send_keyboard_string_sequence(&app, "b");
    assert_eq!(
        (app.get_left_keys(), app.get_right_keys()),
        (0, 1),
        "the named control is the one the key reached"
    );

    // Naming the left scope routes the next key the other way.
    i_slint_backend_testing::set_key_target(&app, Some(left));
    i_slint_backend_testing::send_keyboard_string_sequence(&app, "c");
    assert_eq!((app.get_left_keys(), app.get_right_keys()), (1, 1));

    // Clearing the target hands routing back to the tree's own focus.
    i_slint_backend_testing::set_key_target(&app, None);
    i_slint_backend_testing::send_keyboard_string_sequence(&app, "d");
    assert_eq!(
        (app.get_left_keys(), app.get_right_keys()),
        (1, 1),
        "with no target and no focused scope, the key reaches neither"
    );
}

/// The focus a control draws nothing for stays on the UI thread, so the tree
/// never sees it -- and the press that caused it has to be told, or a focused
/// `FocusScope` would consume a press it should ignore.
#[test]
fn a_press_sees_the_focus_the_ui_thread_kept_to_itself() {
    let platform = PlatformShowingTree::install();
    let app = QuietFocusApp::new().expect("the platform makes windows");
    let adapter = platform.adapter_of_last_window();
    adapter.focus_decided_by_ui.set(true);

    let scope = i_slint_backend_testing::focus_scope_id_at(&app, 160.0, 120.0)
        .expect("the scope covers the window");

    // First press: the UI thread does not yet name the scope, so the scope
    // accepts the press (it takes the focus on click) and the event is consumed
    // before the `TouchArea` behind it -- exactly as the core does when it
    // decides the focus itself.
    i_slint_backend_testing::send_mouse_click_with_ui_focus(&app, 160.0, 120.0, None);
    assert_eq!(app.get_reached(), 0, "the press that took the focus was consumed");

    // Second press: the UI thread already has the focus on the scope, and that
    // travels with the press. A focused `FocusScope` ignores the press instead
    // of consuming it, so it falls through to the `TouchArea` and the click is
    // counted -- the behaviour a shared backend has, reproduced with the focus
    // the UI thread kept to itself.
    i_slint_backend_testing::send_mouse_click_with_ui_focus(&app, 160.0, 120.0, Some(scope));
    assert_eq!(
        app.get_reached(),
        1,
        "a press on the focused scope falls through to what is behind"
    );

    // The tree's own focus is never set for a control that draws no focus, so
    // the carried id is the only thing that could have said it was focused.
    assert_eq!(
        i_slint_backend_testing::focused_item_id(&app),
        None,
        "the drawing side was left out of a focus it does not draw"
    );
}
