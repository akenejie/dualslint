// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! What a component hands the render thread in place of its tree.
//!
//! An item tree holds `Rc`s and a `RefCell`, so it cannot be shared with the
//! thread that draws the window. What the render thread gets instead is the
//! generated constructor: a `Send + Sync` closure that builds a second tree from
//! the same program, over there, with that thread's own context. These tests pin
//! down what that handover rests on -- that generated code leaves the factory
//! behind on the window, that a window nothing generated code ran for has none,
//! and that a factory survives the trip to another thread and builds a
//! component that works over there.
//!
//! The render thread builds its tree behind a platform of its own, which is why
//! the worker below seeds one; without it a component cannot be created on any
//! thread but the one that already has a context.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use i_slint_core::platform::Platform;
use i_slint_core::window::WindowInner;
use slint::private_unstable_api::register_render_factory;
use slint::{ComponentHandle, SharedString};

slint::slint! {
    export component HandoverApp inherits Window {
        width: 320px;
        height: 240px;
        in property <string> label: "app copy";
        in property <int> count: 1;
    }
}

/// A window that no generated code ran for, which is what a `ComponentFactory`
/// that builds the tree itself leaves behind: something to draw into, and no
/// factory for a backend that would have to build a tree of its own.
///
/// The adapter is handed back with the window because the window only holds a
/// weak reference to it.
fn bare_window() -> (slint::Window, Rc<dyn i_slint_core::window::WindowAdapter>) {
    let adapter =
        testing_backend().create_window_adapter().expect("the testing backend makes windows");
    let window = slint::Window::new(Rc::downgrade(&adapter));
    (window, adapter)
}

fn testing_backend() -> i_slint_backend_testing::TestingBackend {
    i_slint_backend_testing::TestingBackend::new(i_slint_backend_testing::TestingBackendOptions {
        mock_time: true,
        ..Default::default()
    })
}

/// Give this thread a platform of its own, the way the render thread does
/// before it runs a factory.
fn seed_worker_platform() {
    i_slint_core::platform::set_platform(Box::new(testing_backend()))
        .expect("the worker thread has no platform yet");
}

#[test]
fn generated_code_leaves_a_factory_on_its_window() {
    i_slint_backend_testing::init_no_event_loop();

    let (bare, _adapter) = bare_window();
    assert!(
        WindowInner::from_pub(&bare).render_factory().is_none(),
        "a window nothing generated code ran for has no factory, which is the case \
         the backend has to report rather than draw quietly"
    );

    let app = HandoverApp::new().expect("the testing backend makes windows");
    assert!(
        WindowInner::from_pub(app.window()).render_factory().is_some(),
        "the render thread finds the factory through the window it will draw into"
    );
}

#[test]
fn the_factory_runs_on_another_thread_and_builds_a_working_component() {
    i_slint_backend_testing::init_no_event_loop();

    let app = HandoverApp::new().expect("the testing backend makes windows");
    let factory = WindowInner::from_pub(app.window())
        .render_factory()
        .expect("generated code left one behind");

    // The window the application holds keeps its own tree, so nothing the render
    // thread builds can reach back into it: the tree is what may not cross, and
    // the program is what crosses.
    app.set_label(SharedString::from("written by the app"));
    app.set_count(7);

    let mirror = std::thread::spawn(move || {
        seed_worker_platform();
        let component = factory()
            .downcast::<HandoverApp>()
            .expect("the factory built the component it was registered for");
        // What the mirror tree says has to be its own, not the app's: same
        // program, different tree.
        (component.get_label().to_string(), component.get_count())
    })
    .join()
    .expect("the factory ran on the worker thread");

    assert_eq!(
        mirror,
        ("app copy".to_string(), 1),
        "a tree built over there starts from the program's own values"
    );
    assert_eq!(
        (app.get_label().to_string(), app.get_count()),
        ("written by the app".to_string(), 7),
        "the application kept writing to its own copy"
    );
}

#[test]
fn the_newest_factory_is_the_one_the_render_thread_would_find() {
    i_slint_backend_testing::init_no_event_loop();

    let (window, _adapter) = bare_window();
    register_render_factory(&window, || {
        HandoverApp::new().expect("a window of the testing backend is available")
    });

    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    register_render_factory(&window, move || {
        *counter.lock().unwrap() += 1;
        HandoverApp::new().expect("a window of the testing backend is available")
    });

    // The render thread may reach the window long after the component behind it
    // was rebuilt, so the registration has to answer for the window as it stands
    // rather than for the component that registered first.
    let factory = WindowInner::from_pub(&window).render_factory().expect("still registered");
    drop(factory());
    assert_eq!(*calls.lock().unwrap(), 1, "the render thread runs the newest factory");
}

#[test]
fn each_window_carries_a_factory_that_builds_its_own_tree() {
    i_slint_backend_testing::init_no_event_loop();

    let first = HandoverApp::new().expect("the testing backend makes windows");
    let second = HandoverApp::new().expect("the testing backend makes windows");
    first.set_count(3);
    second.set_count(5);

    // A window is drawn by whichever thread holds a factory for it, so two
    // windows of one application mean two registrations. The render thread draws
    // one of them and reports that it draws one at a time; what it must never do
    // is reach a window through another window's factory.
    let factory_of = |window: &slint::Window| {
        WindowInner::from_pub(window).render_factory().expect("generated code left one behind")
    };
    let (first_factory, second_factory) = (factory_of(first.window()), factory_of(second.window()));

    std::thread::spawn(move || {
        seed_worker_platform();
        let from_first = first_factory().downcast::<HandoverApp>().expect("a HandoverApp");
        let from_second = second_factory().downcast::<HandoverApp>().expect("a HandoverApp");
        assert_eq!(from_first.get_count(), 1);
        assert_eq!(from_second.get_count(), 1);
    })
    .join()
    .expect("both factories ran on the worker thread");

    assert_eq!((first.get_count(), second.get_count()), (3, 5), "neither window lost its values");
}
