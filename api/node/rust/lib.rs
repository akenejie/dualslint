// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

mod interpreter;
mod weak_ref;
use std::path::PathBuf;

pub use interpreter::*;

mod types;
pub use types::*;

mod uv_event_loop;
pub use uv_event_loop::*;

use napi::Env;
use napi::bindgen_prelude::*;

/// Set a non-enumerable property on a JS object.
///
/// Properties set this way don't appear in `console.log`, `Object.keys`,
/// or `for…in` loops, keeping internal bookkeeping hidden from users.
pub(crate) fn set_hidden_property<'a, V: napi::JsValue<'a>>(
    obj: &mut Object<'_>,
    key: &str,
    value: &V,
) -> napi::Result<()> {
    let prop = napi::Property::new()
        .with_utf8_name(key)?
        .with_property_attributes(
            napi::PropertyAttributes::Writable | napi::PropertyAttributes::Configurable,
        )
        .with_value(value);
    JsObjectValue::define_properties(obj, &[prop])
}

#[macro_use]
extern crate napi_derive;

#[napi]
pub fn mock_elapsed_time(_ms: f64) {
    #[cfg(feature = "testing")]
    i_slint_backend_testing::mock_elapsed_time(_ms as u64);
}

#[napi]
pub fn get_mocked_time() -> f64 {
    #[cfg(feature = "testing")]
    return i_slint_backend_testing::get_mocked_time() as f64;
    #[cfg(not(feature = "testing"))]
    return 0.0;
}

#[napi]
pub enum ProcessEventsResult {
    Continue,
    Exited,
}

fn process_events_with_timeout(
    timeout: Option<std::time::Duration>,
) -> napi::Result<ProcessEventsResult> {
    i_slint_backend_selector::with_platform(|b| {
        b.process_events(timeout, i_slint_core::InternalToken)
    })
    .map_err(|e| napi::Error::from_reason(e.to_string()))
    .map(|result| match result {
        core::ops::ControlFlow::Break(()) => ProcessEventsResult::Exited,
        core::ops::ControlFlow::Continue(()) => ProcessEventsResult::Continue,
    })
}

#[napi]
pub fn process_events() -> napi::Result<ProcessEventsResult> {
    process_events_with_timeout(Some(std::time::Duration::ZERO))
}

#[napi]
pub fn invoke_from_event_loop(
    env: &Env,
    #[napi(ts_arg_type = "() => void")] callback: DynFunction<'_>,
) -> napi::Result<()> {
    i_slint_backend_selector::with_platform(|_b| {
        // Nothing to do, just make sure a backend was created
        Ok(())
    })
    .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    let func_ref = callback.create_ref()?;
    let env = *env;
    let wrapper = send_wrapper::SendWrapper::new((func_ref, env));
    i_slint_core::api::invoke_from_event_loop(move || {
        let (func_ref, env) = wrapper.take();
        if func_ref.borrow_back(&env).and_then(|f| f.call(DynArgs(vec![]))).is_err() {
            eprintln!("Node.js: JavaScript invoke_from_event_loop threw an exception");
        }
    })
    .map_err(|e| napi::Error::from_reason(e.to_string()))
}

#[napi]
pub fn quit_event_loop() -> napi::Result<()> {
    // Don't call core's quit_event_loop — that permanently terminates the winit event loop.
    // Set a flag so process_slint_events returns Exited on the next iteration.
    uv_event_loop::request_quit();
    Ok(())
}

#[napi]
pub fn set_quit_on_last_window_closed(quit_on_last_window_closed: bool) -> napi::Result<()> {
    if !quit_on_last_window_closed {
        i_slint_backend_selector::with_platform(|b| {
            #[allow(deprecated)]
            b.set_event_loop_quit_on_last_window_closed(false);
            Ok(())
        })
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    }
    Ok(())
}

#[napi]
pub fn init_testing() {
    #[cfg(feature = "testing")]
    i_slint_backend_testing::init_integration_test_with_mock_time();
}

/// Returns the list of optional capabilities that were compiled into the loaded
/// native binary. This is how JavaScript can tell whether the "dev" binary
/// (with system-testing and MCP support) was loaded, or just the default one.
#[napi]
pub fn build_features() -> Vec<String> {
    let mut features = Vec::new();
    if cfg!(feature = "testing") {
        features.push("testing".to_string());
    }
    if cfg!(feature = "system-testing") {
        features.push("system-testing".to_string());
    }
    if cfg!(feature = "mcp") {
        features.push("mcp".to_string());
    }
    features
}

#[napi]
pub fn init_translations(domain: String, dir_name: String) -> napi::Result<()> {
    i_slint_core::translations::gettext_bindtextdomain(domain.as_str(), PathBuf::from(dir_name))
        .map_err(|e| napi::Error::from_reason(e.to_string()))
}

#[napi]
pub fn set_xdg_app_id(app_id: String) -> napi::Result<()> {
    i_slint_backend_selector::with_global_context(|ctx| ctx.set_xdg_app_id(app_id.into()))
        .map_err(|e| napi::Error::from_reason(e.to_string()))
}

/// Request a window repaint through the render thread.
///
/// The winit backend owns presentation on a dedicated thread, so a repaint
/// request has to go there rather than to a UI-side window. The UI thread and
/// worker threads are equal peers here: both may call this. It is a no-op when
/// the render thread was never started.
#[napi]
pub fn request_redraw() {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    slint_interpreter::render_thread::request_redraw();
}

// ---------------------------------------------------------------------------
// Borrowing the render-owned controls
//
// The render thread owns the controls, so a program that wants to touch one
// borrows it: identify the control, then assign a property. The UI thread and
// any worker are equal peers on this API.
// ---------------------------------------------------------------------------

/// A value to assign to a borrowed control's property.
///
/// Tagged the same way as the C ABI, so the four kinds travel as one value
/// rather than as four functions: `kind` selects the field to read, and the
/// others are ignored.
#[napi(object)]
pub struct ControlPropertyValue {
    /// One of `bool`, `number`, `text` or `color`.
    pub kind: String,
    /// A `bool` property, such as a `TouchArea`'s `enabled`.
    pub boolean: Option<bool>,
    /// A numeric property, such as `opacity` or `width`.
    pub number: Option<f64>,
    /// A string property, such as a `Text`'s `text`.
    pub text: Option<String>,
    /// A color property, given as `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`, such
    /// as a `Text`'s `color`. A color that cannot be read is refused rather than
    /// replaced by a default, so a typo cannot quietly paint the wrong thing.
    pub color: Option<String>,
}

impl ControlPropertyValue {
    /// Resolve the tag to a value. A tag that is not one of the four kinds, or a
    /// kind whose field is missing, has nothing to assign and is refused.
    fn into_slint(self) -> Option<slint_interpreter::render_thread::ControlPropertyValue> {
        use slint_interpreter::render_thread::ControlPropertyValue as Value;
        let value = match self.kind.as_str() {
            "bool" => Value::Bool(self.boolean?),
            "number" => Value::Number(self.number? as f32),
            "text" => Value::Text(self.text?),
            "color" => {
                let (r, g, b, a) = parse_color(&self.color?)?;
                Value::Color { r, g, b, a }
            }
            _ => return None,
        };
        Some(value)
    }
}

impl From<slint_interpreter::render_thread::ControlPropertyValue> for ReadControlPropertyValue {
    fn from(value: slint_interpreter::render_thread::ControlPropertyValue) -> Self {
        use slint_interpreter::render_thread::ControlPropertyValue as Value;
        match value {
            Value::Bool(v) => Self {
                kind: "bool".into(),
                boolean: v,
                number: 0.,
                text: String::new(),
                color: String::new(),
            },
            Value::Number(v) => Self {
                kind: "number".into(),
                boolean: false,
                number: v as f64,
                text: String::new(),
                color: String::new(),
            },
            Value::Text(v) => Self {
                kind: "text".into(),
                boolean: false,
                number: 0.,
                text: v,
                color: String::new(),
            },
            Value::Color { r, g, b, a } => Self {
                kind: "color".into(),
                boolean: false,
                number: 0.,
                text: String::new(),
                // The same hex spelling the setter reads, so a read can be
                // handed straight back to `setControlProperty`.
                color: format!("#{r:02x}{g:02x}{b:02x}{a:02x}"),
            },
        }
    }
}

/// A control property read back from the render thread.
///
/// The fields are all filled in, and `kind` names the one that carries the
/// value, which is how it crosses the native boundary as one flat object. The
/// TypeScript side turns that back into the same discriminated union
/// `setControlProperty` takes, so a read value can be written straight back.
#[napi(object)]
pub struct ReadControlPropertyValue {
    /// One of `bool`, `number`, `text` or `color`.
    pub kind: String,
    /// The value for a `bool` property.
    pub boolean: bool,
    /// The value for a `number` property.
    pub number: f64,
    /// The value for a `text` property.
    pub text: String,
    /// The value for a `color` property, as `#rrggbbaa`.
    pub color: String,
}

/// Read one property of a borrowed control, blocking until the render thread
/// answers. Returns `null` when the id or the property name does not resolve.
///
/// The `.slint` side of the tree belongs to the render thread, so a caller that
/// needs to know what a control looks like asks here instead of keeping a second
/// copy of the component.
#[napi]
pub fn get_control_property(id: i64, property: String) -> Option<ReadControlPropertyValue> {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        if id <= 0 {
            return None;
        }
        return slint_interpreter::render_thread::host()
            .and_then(|host| host.get_control_property(id as u64, &property))
            .map(Into::into);
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    {
        let _ = (id, property);
        None
    }
}

/// Parse `#rgb`, `#rgba`, `#rrggbb` and `#rrggbbaa`, with or without the `#`.
///
/// Returns `None` for anything else, including a color name: the Python binding
/// takes a `slint.Color` and so understands names, and this one deliberately
/// stays a plain hex reader. A color it cannot read is refused by the caller
/// rather than turned into a default, because writing a color nobody asked for
/// is worse than doing nothing.
fn parse_color(text: &str) -> Option<(u8, u8, u8, u8)> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    let byte = |i: usize| u8::from_str_radix(digits.get(i * 2..i * 2 + 2)?, 16).ok();
    match digits.len() {
        // The short forms, where each nibble is doubled, so `#abc` is
        // `#aabbcc`.
        3 | 4 => {
            let mut rgba = [0u8; 4];
            rgba[3] = 0xff;
            for (index, slot) in rgba.iter_mut().enumerate().take(digits.len()) {
                let nibble = *digits.as_bytes().get(index)?;
                *slot = match nibble {
                    c @ b'0'..=b'9' => c - b'0',
                    c @ b'a'..=b'f' => c - b'a' + 10,
                    c @ b'A'..=b'F' => c - b'A' + 10,
                    _ => return None,
                };
                *slot *= 17;
            }
            Some((rgba[0], rgba[1], rgba[2], rgba[3]))
        }
        6 => Some((byte(0)?, byte(1)?, byte(2)?, 0xff)),
        8 => Some((byte(0)?, byte(1)?, byte(2)?, byte(3)?)),
        _ => None,
    }
}

/// The control under the logical point `x`, `y`, or `null` for none.
///
/// Reads the geometry the render thread last composited and does not wait for
/// it, so it is cheap enough for a pointer move. Use `hitTest` when the answer
/// must reflect the present rather than the last frame.
#[napi]
pub fn control_at(x: f64, y: f64) -> Option<i64> {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        return slint_interpreter::render_thread::host()
            .and_then(|host| host.control_at(x as f32, y as f32))
            .map(|id| id as i64);
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    None
}

/// The control under the logical point `x`, `y`, resolved on the render thread
/// itself. Blocks until the render thread answers, which is what makes it right
/// for a click and wrong for a pointer move.
#[napi]
pub fn hit_test(x: f64, y: f64) -> Option<i64> {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        return slint_interpreter::render_thread::host()
            .and_then(|host| host.hit_test(x as f32, y as f32))
            .map(|id| id as i64);
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    None
}

/// Assign one property of a borrowed control, blocking until the render thread
/// confirms it. Returns whether the property name resolved and the value was
/// applied; a property that had a binding is detached first, the same way
/// assigning through the normal API behaves.
#[napi]
pub fn set_control_property(id: i64, property: String, value: ControlPropertyValue) -> bool {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        if id <= 0 {
            return false;
        }
        // A tag that names no known kind, or a kind whose field is missing, has
        // no value to assign. Refuse it rather than fall back to a default,
        // which would write that default into whatever property was named.
        let Some(value) = value.into_slint() else { return false };
        return slint_interpreter::render_thread::host()
            .is_some_and(|host| host.set_control_property(id as u64, &property, value));
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    {
        let _ = (id, property, value);
        false
    }
}

/// Report a borrowed control as hovered and/or pressed on the render thread.
///
/// This is how a pointer state the program resolved itself is handed over; the
/// UI thread does it for real pointer input, and a worker driving a control from
/// its own logic does it the same way.
#[napi]
pub fn apply_control_state(id: i64, hovered: bool, pressed: bool) {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    if id > 0 {
        if let Some(host) = slint_interpreter::render_thread::host() {
            host.apply_control_state(id as u64, hovered, pressed);
        }
    }
}

pub fn print_to_console(env: Env, function: &str, arguments: core::fmt::Arguments) {
    let Ok(global) = env.get_global() else {
        eprintln!("Unable to obtain global object");
        return;
    };

    let console_object: Object = match global.get_named_property("console") {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Unable to obtain console object for logging");
            return;
        }
    };

    let log_fn: Function<Unknown, Unknown> = match console_object.get_named_property(function) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("Unable to obtain console.{function}");
            return;
        }
    };

    let message = arguments.to_string();
    let Ok(js_message) = env.create_string(&message) else {
        eprintln!("Unable to provide log message to JS env");
        return;
    };

    let Ok(js_message_unknown) = js_message.into_unknown(&env) else {
        eprintln!("Unable to convert log message to unknown");
        return;
    };

    if let Err(err) = log_fn.apply(console_object, js_message_unknown) {
        eprintln!("Unable to invoke console.{function}: {err}");
    }
}

#[macro_export]
macro_rules! console_err {
    ($env:expr, $($t:tt)*) => ($crate::print_to_console($env, "error", format_args!($($t)*)))
}

/// Route Slint log messages (the `debug()` function in Slint code and runtime warnings)
/// to `console.log`, like on wasm.
pub(crate) fn install_log_message_handler(env: &Env, ctx: &i_slint_core::SlintContext) {
    let env = *env;
    ctx.set_log_message_handler(Some(Box::new(move |message| {
        let arguments = message.message_arguments();
        // A handle scope is needed because a message can arrive outside of any JS frame,
        // e.g. from a timer dispatched by the integrated event loop.
        let result = env.run_in_scope(|| {
            match message.location() {
                Some(l) => print_to_console(
                    env,
                    "log",
                    format_args!("{}:{}:{}: {arguments}", l.path, l.line, l.column),
                ),
                None => print_to_console(env, "log", arguments),
            }
            Ok(())
        });
        if result.is_err() {
            i_slint_core::debug_log::default_log_message(arguments);
        }
    })));
}

#[cfg(test)]
mod control_property_tests {
    use super::{ControlPropertyValue, parse_color};
    use slint_interpreter::render_thread::ControlPropertyValue as Value;

    /// A value naming `kind` and carrying one field, leaving the rest missing.
    /// Building values field by field is the point: a value that is missing its
    /// own field has to be refused, so tests need to be able to leave holes.
    fn with(kind: &str, field: &str) -> ControlPropertyValue {
        let mut v = bare(kind);
        match field {
            "boolean" => v.boolean = Some(true),
            "number" => v.number = Some(0.5),
            "text" => v.text = Some("x".into()),
            "color" => v.color = Some("#ff8000".into()),
            other => panic!("unknown field {other}"),
        }
        v
    }

    /// A value naming `kind` and carrying nothing else, so a test can fill in
    /// one field and leave the rest missing.
    fn bare(kind: &str) -> ControlPropertyValue {
        ControlPropertyValue {
            kind: kind.into(),
            boolean: None,
            number: None,
            text: None,
            color: None,
        }
    }

    #[test]
    fn parses_the_css_color_forms() {
        // The long forms.
        assert_eq!(parse_color("#ff8000"), Some((255, 128, 0, 255)));
        assert_eq!(parse_color("#ff800080"), Some((255, 128, 0, 128)));
        // The short forms, where each nibble is doubled.
        assert_eq!(parse_color("#abc"), Some((0xaa, 0xbb, 0xcc, 255)));
        assert_eq!(parse_color("#abcd"), Some((0xaa, 0xbb, 0xcc, 0xdd)));
        // Upper and lower case both work, and the `#` is optional.
        assert_eq!(parse_color("#AABBCC"), Some((0xaa, 0xbb, 0xcc, 255)));
        assert_eq!(parse_color("aabbcc"), Some((0xaa, 0xbb, 0xcc, 255)));
        // Eight digits is a valid `#rrggbbaa`, whatever it happens to look like.
        assert_eq!(parse_color("#abcdefff"), Some((0xab, 0xcd, 0xef, 0xff)));
    }

    #[test]
    fn a_color_it_cannot_read_is_refused() {
        // Writing a default instead would paint something nobody asked for, so
        // every unreadable form has to come back as "no value".
        for text in ["", "#", "#xyz", "#abcde", "#12345", "rebeccapurple", "#gghhii", "#1234567g"] {
            assert_eq!(parse_color(text), None, "{text:?} should not parse");
        }
    }

    #[test]
    fn every_kind_converts_to_a_value() {
        assert!(matches!(with("bool", "boolean").into_slint(), Some(Value::Bool(true))));
        assert!(
            matches!(with("number", "number").into_slint(), Some(Value::Number(n)) if n == 0.5)
        );
        assert!(matches!(with("text", "text").into_slint(), Some(Value::Text(t)) if t == "x"));
        assert!(matches!(
            with("color", "color").into_slint(),
            Some(Value::Color { r: 255, g: 128, b: 0, a: 255 })
        ));
    }

    #[test]
    fn a_kind_without_its_field_is_refused() {
        // Naming a kind but leaving its field out means the caller meant to send
        // a value and did not, so there is nothing to assign.
        assert!(bare("text").into_slint().is_none());
        assert!(bare("bool").into_slint().is_none());
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        // Refused for the same reason as a missing field: there is no value to
        // assign, and defaulting would write that default into the named
        // property.
        assert!(bare("colour").into_slint().is_none());
        assert!(bare("").into_slint().is_none());
        // A known kind whose value is unreadable is refused the same way, since
        // there is no value to assign either.
        let bad_color =
            ControlPropertyValue { color: Some("rebeccapurple".into()), ..bare("color") };
        assert!(bad_color.into_slint().is_none());
    }
}
