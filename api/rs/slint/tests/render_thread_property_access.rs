// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only

//! Lending the render thread the properties of the component it draws.
//!
//! A widget from a `.slint` file is a group of items plus the bindings between
//! them, so what a caller usually wants to know -- whether a `CheckBox` is
//! `checked` -- is a property of that group and of no item in it. The render
//! thread resolves what its own items can answer and asks the application for
//! the rest, over the same pair of calls whether the application is written in
//! Rust or reaches it through the C ABI.

use i_slint_backend_scene::{CSlintRenderThreadPropertyAccess, ControlPropertyValue};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::window::WindowInner;
use slint::render_thread::ComponentPropertyAccess;

/// The component the tests hand over stands for one an application created; its
/// address is what the render thread passes to the pair of calls, so a callee
/// can tell which component it is answering for.
struct AppComponent {
    checked: std::cell::Cell<bool>,
}

thread_local! {
    /// The text a write call was handed, so a test can look at the string that
    /// crossed the ABI without the callee having to borrow anything.
    static WRITTEN_TEXT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };

    /// Where the component keeps its `label`, standing in for the place a real
    /// application keeps it. A read hands out a pointer to this rather than to a
    /// buffer of its own, because the value is only copied after the call has
    /// returned and a buffer the call owned would be gone by then.
    static LABEL: std::cell::RefCell<std::ffi::CString> =
        std::cell::RefCell::new(std::ffi::CString::default());
}

unsafe extern "C" fn read_from_c(
    component: *const std::os::raw::c_void,
    item: *const std::os::raw::c_void,
    property: *const std::os::raw::c_char,
    out: *mut i_slint_backend_scene::CSlintControlPropertyValue,
) -> bool {
    // SAFETY: the render thread passes the address of the component it is
    // drawing, and the tests own every pointer in this call.
    let Some(component) = (unsafe { (component as *const AppComponent).as_ref() }) else {
        return false;
    };
    assert!(!item.is_null(), "the item pointer is the address of an item in the tree");
    // SAFETY: `property` is the NUL-terminated name the render thread holds.
    let Ok(property) = (unsafe { std::ffi::CStr::from_ptr(property) }).to_str() else {
        return false;
    };
    let value = match property {
        "checked" => ControlPropertyValue::Bool(component.checked.get()),
        "label" => ControlPropertyValue::Text(
            LABEL.with_borrow(|label| label.to_string_lossy().into_owned()),
        ),
        // A name the component does not declare, so that the render thread
        // goes on to ask its own items.
        _ => return false,
    };
    let text = LABEL.with_borrow_mut(|label| {
        if label.as_bytes().is_empty() {
            *label = std::ffi::CString::new("from the component").unwrap();
        }
        label.as_ptr()
    });
    // SAFETY: `out` is the writable slot the render thread passed, and the
    // string it points at lives in the component rather than in this call, which
    // is what the contract asks for.
    unsafe {
        out.write(i_slint_backend_scene::CSlintControlPropertyValue::from_value(&value, text))
    };
    true
}

unsafe extern "C" fn write_to_c(
    component: *const std::os::raw::c_void,
    item: *const std::os::raw::c_void,
    property: *const std::os::raw::c_char,
    value: *const i_slint_backend_scene::CSlintControlPropertyValue,
) -> bool {
    // SAFETY: the render thread passes the address of the component it is
    // drawing, and the tests own every pointer in this call.
    let Some(component) = (unsafe { (component as *const AppComponent).as_ref() }) else {
        return false;
    };
    assert!(!item.is_null(), "the item pointer is the address of an item in the tree");
    // SAFETY: as in the read; the name and the value are the render thread's.
    let Ok(property) = (unsafe { std::ffi::CStr::from_ptr(property) }).to_str() else {
        return false;
    };
    let Some(value) = (unsafe { (*value).to_value() }) else { return false };
    match (property, value) {
        ("checked", ControlPropertyValue::Bool(v)) => {
            component.checked.set(v);
            true
        }
        // The other direction a string has to survive: what the bridge handed
        // over, read back as text.
        ("label", ControlPropertyValue::Text(v)) => {
            WRITTEN_TEXT.with_borrow_mut(|slot| *slot = Some(v));
            true
        }
        _ => false,
    }
}

slint::slint! {
    export component Probe inherits Window {
        width: 100px;
        height: 100px;
    }
}

/// The item the calls are handed: the root of a real tree on a real window,
/// which is what the render thread would pass for a control it could not resolve
/// a name against.
fn a_real_item() -> (Probe, ItemRc) {
    i_slint_backend_testing::init_no_event_loop();
    let probe = Probe::new().unwrap();
    probe.show().unwrap();
    let inner = WindowInner::from_pub(probe.window());
    inner.ensure_tree_instantiated();
    let item = ItemRc::new_root(inner.component());
    (probe, item)
}

fn a_component() -> AppComponent {
    AppComponent { checked: std::cell::Cell::new(false) }
}

#[test]
fn the_c_pair_answers_for_the_component_it_was_written_against() {
    let (_probe, item) = a_real_item();
    let component = a_component();
    // SAFETY: both calls are the ones the tests define above, and they answer
    // for every pointer the render thread passes.
    let access = unsafe {
        ComponentPropertyAccess::from_c(CSlintRenderThreadPropertyAccess {
            read: Some(read_from_c),
            write: Some(write_to_c),
        })
    };
    let any = &component as &dyn std::any::Any;

    assert_eq!(access.read(any, &item, "checked"), Some(ControlPropertyValue::Bool(false)));
    assert_eq!(
        access.read(any, &item, "label"),
        Some(ControlPropertyValue::Text("from the component".into()))
    );
    // Declaring no such property is not a failure, and saying so is what lets
    // the render thread fall back to its own items.
    assert_eq!(access.read(any, &item, "enabled"), None);

    assert!(access.write(any, &item, "checked", &ControlPropertyValue::Bool(true)));
    assert!(component.checked.get());
    assert_eq!(access.read(any, &item, "checked"), Some(ControlPropertyValue::Bool(true)));
    // A property that exists, with a value that does not fit it.
    assert!(!access.write(any, &item, "checked", &ControlPropertyValue::Text("no".into())));
    // A property the component does not have at all.
    assert!(!access.write(any, &item, "enabled", &ControlPropertyValue::Bool(false)));
}

#[test]
fn a_missing_call_leaves_the_questions_to_the_render_thread() {
    let (_probe, item) = a_real_item();
    let component = AppComponent { checked: std::cell::Cell::new(true) };
    let any = &component as &dyn std::any::Any;

    // SAFETY: as above.
    let read_only = unsafe {
        ComponentPropertyAccess::from_c(CSlintRenderThreadPropertyAccess {
            read: Some(read_from_c),
            write: None,
        })
    };
    assert_eq!(read_only.read(any, &item, "checked"), Some(ControlPropertyValue::Bool(true)));
    assert!(!read_only.write(any, &item, "checked", &ControlPropertyValue::Bool(false)));
    assert!(component.checked.get(), "a null write answers nothing rather than anything");

    // SAFETY: as above.
    let write_only = unsafe {
        ComponentPropertyAccess::from_c(CSlintRenderThreadPropertyAccess {
            read: None,
            write: Some(write_to_c),
        })
    };
    assert_eq!(write_only.read(any, &item, "checked"), None);
    assert!(write_only.write(any, &item, "checked", &ControlPropertyValue::Bool(false)));
    assert!(!component.checked.get());
}

#[test]
fn a_text_property_survives_the_trip_in_both_directions() {
    let (_probe, item) = a_real_item();
    let component = a_component();
    // SAFETY: as above.
    let access = unsafe {
        ComponentPropertyAccess::from_c(CSlintRenderThreadPropertyAccess {
            read: Some(read_from_c),
            write: Some(write_to_c),
        })
    };
    let any = &component as &dyn std::any::Any;

    // Out: the string is copied after the read returns and before anything can
    // reuse the buffer, so what comes back is the text and not a borrow.
    let read = access.read(any, &item, "label").unwrap();
    assert_eq!(read, ControlPropertyValue::Text("from the component".into()));

    // And back: the bridge owns the buffer for the length of the call, and the
    // callee reads what the application asked to assign.
    assert!(access.write(any, &item, "label", &ControlPropertyValue::Text("assigned".into())));
    WRITTEN_TEXT.with_borrow(|slot| assert_eq!(slot.as_deref(), Some("assigned")));
}
