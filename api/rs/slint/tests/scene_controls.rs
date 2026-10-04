// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! What the control walk publishes as a control region.  A control region is a
//! hit-test target, so it has to be the things a user can point at: painting
//! something and being able to interact with it are separate questions.

mod common;

use i_slint_backend_scene::ControlRegion;
use i_slint_backend_scene::controls::encode_window_controls;
use slint::ComponentHandle;
use slint::platform::WindowAdapter;
use slint::platform::software_renderer::SoftwareRenderer;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 200;

/// Show `f`'s component on the shared window, lay it out, and walk it.
/// Returns the published control regions in tree order (back to front).
///
/// The platform has to be installed before the component creates its window,
/// which is why the component is created inside the closure.
fn controls(f: impl FnOnce()) -> Vec<ControlRegion> {
    let window = common::setup(WIDTH, HEIGHT);
    f();
    WindowAdapter::request_redraw(window.as_ref());
    window.draw_if_needed(|renderer: &SoftwareRenderer| {
        let size = window.size();
        let (width, height) = (size.width as usize, size.height as usize);
        let mut buffer = vec![common::TestPixel(false); width * height];
        renderer.render(buffer.as_mut_slice(), width);
    });
    encode_window_controls(WindowAdapter::window(window.as_ref()))
        .expect("walking a laid-out window")
        .controls
}

macro_rules! component {
    ($name:ident { $($body:tt)* }) => {
        slint::slint! {
            export component $name inherits Window { $($body)* }
        }
    };
}

component!(InvisibleTouchArea {
    TouchArea { }
});

component!(PlainRectangle {
    Rectangle { background: red; }
});

component!(RectangleBehindTouchArea {
    Rectangle { background: red; }
    TouchArea { }
});

component!(TextLabel {
    Text { text: "not a hit target"; }
});

component!(PositionedTouchArea {
    TouchArea { x: 10px; y: 20px; width: 80px; height: 40px; }
});

/// Show a freshly created component and return the control regions it
/// publishes.  Creating the component has to happen after the platform is
/// installed, hence the macro.
macro_rules! controls_of {
    ($component:ident) => {
        controls(|| {
            let ui = $component::new().unwrap();
            ui.show().unwrap();
        })
    };
}

/// A `TouchArea` without a background is never asked to draw anything, and it is
/// still the thing the user clicks, so it has to be published.
#[test]
fn invisible_touch_area_is_a_control() {
    assert_eq!(controls_of!(InvisibleTouchArea).len(), 1);
}

/// Drawing something does not make it interactive: a rectangle is painted but
/// must not become a hit target, or it would swallow the clicks meant for the
/// control on top of it.
#[test]
fn plain_rectangle_is_not_a_control() {
    assert!(controls_of!(PlainRectangle).is_empty());
}

/// With both present, only the interactive one is published.
#[test]
fn rectangle_and_touch_area_are_distinguished() {
    assert_eq!(controls_of!(RectangleBehindTouchArea).len(), 1);
}

/// Text is drawn, not pointed at.
#[test]
fn text_is_not_a_control() {
    assert!(controls_of!(TextLabel).is_empty());
}

/// A control is published with the rectangle it was laid out at, in window
/// coordinates, because that is the rectangle the UI thread hit-tests against.
#[test]
fn published_geometry_is_the_laid_out_rect() {
    let regions = controls_of!(PositionedTouchArea);
    assert_eq!(regions.len(), 1);
    let geometry = regions[0].geometry;
    assert_eq!((geometry.origin.x, geometry.origin.y), (10.0, 20.0));
    assert_eq!((geometry.size.width, geometry.size.height), (80.0, 40.0));
}
