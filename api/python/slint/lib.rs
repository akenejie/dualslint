// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore ngettext npgettext pgettext unraisable
use std::cell::{Cell, RefCell};

mod data_transfer;
mod geometry;
mod image;
mod interpreter;
mod language;
use interpreter::{
    CompilationResult, Compiler, ComponentDefinition, ComponentInstance, PyDiagnostic,
    PyDiagnosticLevel, PyValueType,
};
mod api_match;
mod async_adapter;
mod brush;
mod errors;
mod keys;
mod models;
mod styled_text;
mod timer;
mod value;
use i_slint_core::translations::Translator;

fn handle_unraisable(py: Python<'_>, context: String, err: PyErr) {
    let exception = err.value(py);
    let __notes__ = exception
        .getattr(pyo3::intern!(py, "__notes__"))
        .unwrap_or_else(|_| pyo3::types::PyList::empty(py).into_any());
    if let Ok(notes_list) = __notes__.cast::<pyo3::types::PyList>() {
        let _ = notes_list.append(context);
        let _ = exception.setattr(pyo3::intern!(py, "__notes__"), __notes__);
    }

    if EVENT_LOOP_RUNNING.get() && err.is_instance_of::<pyo3::exceptions::PySystemExit>(py) {
        EVENT_LOOP_EXCEPTION.replace(Some(err));
        let _ = slint_interpreter::quit_event_loop();
    } else {
        err.write_unraisable(py, None);
    }
}

thread_local! {
    static EVENT_LOOP_RUNNING: Cell<bool> = Cell::new(false);
    static EVENT_LOOP_EXCEPTION: RefCell<Option<PyErr>> = RefCell::new(None)
}

#[pyfunction]
fn run_event_loop(py: Python<'_>) -> Result<(), PyErr> {
    EVENT_LOOP_EXCEPTION.replace(None);
    EVENT_LOOP_RUNNING.set(true);
    // Release the GIL while running the event loop, so that other Python threads can run.
    let result = py.detach(|| slint_interpreter::run_event_loop());
    EVENT_LOOP_RUNNING.set(false);
    result.map_err(|e| errors::PyPlatformError::from(e))?;
    EVENT_LOOP_EXCEPTION.take().map_or(Ok(()), |err| Err(err))
}

#[pyfunction]
fn quit_event_loop() -> Result<(), errors::PyEventLoopError> {
    slint_interpreter::quit_event_loop().map_err(|e| e.into())
}

#[pyfunction]
fn set_xdg_app_id(app_id: String) -> Result<(), errors::PyPlatformError> {
    slint_interpreter::set_xdg_app_id(app_id).map_err(|e| e.into())
}

/// Request a window repaint through the render thread.
///
/// The winit backend owns presentation on a dedicated thread, so a repaint
/// request has to be addressed there rather than to a UI-side window. The UI
/// thread and worker threads are equal peers here: both may call this. It is a
/// no-op when the render thread was never started.
#[pyfunction]
fn request_redraw() {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    slint_interpreter::render_thread::request_redraw();
}

/// The control under the logical point `x, y`, or `None` for none.
///
/// Reads the geometry the render thread last composited and does not wait for
/// it, so it is cheap enough for a pointer move. Use `hit_test` when the answer
/// must reflect the present rather than the last frame.
#[pyfunction]
fn control_at(x: f32, y: f32) -> Option<u64> {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        return slint_interpreter::render_thread::host().and_then(|host| host.control_at(x, y));
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    {
        let _ = (x, y);
        None
    }
}

/// The control under the logical point `x, y`, resolved on the render thread
/// itself, or `None` for none.
///
/// Unlike `control_at`, this blocks until the render thread answers, which is
/// what makes it right for a click and wrong for a pointer move.
#[pyfunction]
fn hit_test(x: f32, y: f32) -> Option<u64> {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        return slint_interpreter::render_thread::host().and_then(|host| host.hit_test(x, y));
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    {
        let _ = (x, y);
        None
    }
}

/// Assign one property of a borrowed control, blocking until the render thread
/// confirms it.
///
/// `value_type` is one of `"bool"`, `"number"`, `"text"` or `"color"`, and
/// `value` has to match it: a `bool`, a number, a `str`, and a `slint.Color`
/// respectively. A color is therefore written the way CSS spells it, as
/// `slint.Color("rebeccapurple")`.
///
/// Returns whether the property name resolved and the value was applied. A
/// property that had a binding is detached first, the same way assigning through
/// the normal API behaves.
///
/// A `value_type` that names no known type returns false rather than guessing
/// which field was meant. A `value` that does not match its `value_type` raises,
/// because it points straight at the caller's mistake, which a false return
/// would hide.
#[pyfunction]
#[pyo3(signature = (id, property, value_type, value))]
fn set_control_property(
    id: u64,
    property: &str,
    value_type: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    use slint_interpreter::render_thread::ControlPropertyValue;
    let value = match value_type {
        "bool" => ControlPropertyValue::Bool(value.extract::<bool>()?),
        "number" => ControlPropertyValue::Number(value.extract::<f64>()? as f32),
        "text" => ControlPropertyValue::Text(value.extract::<String>()?),
        "color" => {
            let color = value.extract::<brush::PyColor>()?.color;
            ControlPropertyValue::Color {
                r: color.red(),
                g: color.green(),
                b: color.blue(),
                a: color.alpha(),
            }
        }
        _ => return Ok(false),
    };
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    {
        return Ok(slint_interpreter::render_thread::host()
            .is_some_and(|host| host.set_control_property(id, property, value)));
    }
    #[cfg(not(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    )))]
    {
        let _ = (id, property, value);
        Ok(false)
    }
}

/// Report a borrowed control as hovered and/or pressed on the render thread.
///
/// This is how a pointer state the program resolved itself is handed over: the
/// UI thread does it for real pointer input, and a worker driving a control from
/// its own logic does it the same way.
#[pyfunction]
#[pyo3(signature = (id, hovered, pressed=false))]
fn apply_control_state(id: u64, hovered: bool, pressed: bool) {
    #[cfg(any(
        feature = "backend-winit",
        feature = "backend-winit-x11",
        feature = "backend-winit-wayland"
    ))]
    if let Some(host) = slint_interpreter::render_thread::host() {
        host.apply_control_state(id, hovered, pressed);
    }
}

#[pyfunction]
fn invoke_from_event_loop(callable: Py<PyAny>) -> Result<(), errors::PyEventLoopError> {
    slint_interpreter::invoke_from_event_loop(move || {
        Python::attach(|py| {
            if let Err(err) = callable.call0(py) {
                eprintln!("Error invoking python callable from closure invoked via slint::invoke_from_event_loop: {}", err)
            }
        })
    })
    .map_err(|e| e.into())
}

#[pyfunction]
fn init_translations(_py: Python<'_>, translations: Bound<PyAny>) -> PyResult<()> {
    i_slint_backend_selector::with_global_context(|ctx| {
        ctx.set_external_translator(if translations.is_none() {
            None
        } else {
            Some(Box::new(PyGettextTranslator(translations.unbind())))
        });
    })
    .map_err(|e| errors::PyPlatformError(e))?;
    Ok(())
}

/// Returns the list of optional capabilities that were compiled into the loaded
/// native binary. This is how Python can tell whether the "dev" binary (with
/// system-testing and MCP support) was loaded, or just the default lean one.
#[pyfunction]
fn build_features() -> Vec<String> {
    let mut features = Vec::new();
    if cfg!(feature = "backend-testing") {
        features.push("backend-testing".to_string());
    }
    if cfg!(feature = "system-testing") {
        features.push("system-testing".to_string());
    }
    if cfg!(feature = "mcp") {
        features.push("mcp".to_string());
    }
    features
}

struct PyGettextTranslator(
    /// A reference to a `gettext.GNUTranslations` object.
    Py<PyAny>,
);

impl Translator for PyGettextTranslator {
    fn translate<'a>(
        &'a self,
        string: &'a str,
        context: Option<&'a str>,
    ) -> std::borrow::Cow<'a, str> {
        Python::try_attach(|py| {
            match if let Some(context) = context {
                self.0.call_method(py, pyo3::intern!(py, "pgettext"), (context, string), None)
            } else {
                self.0.call_method(py, pyo3::intern!(py, "gettext"), (string,), None)
            } {
                Ok(translation) => Some(translation),
                Err(err) => {
                    handle_unraisable(py, "calling pgettext/gettext".into(), err);
                    None
                }
            }
            .and_then(|maybe_str| maybe_str.extract::<String>(py).ok())
            .map(std::borrow::Cow::Owned)
        })
        .flatten()
        .unwrap_or(std::borrow::Cow::Borrowed(string))
        .into()
    }

    fn ntranslate<'a>(
        &'a self,
        n: u64,
        singular: &'a str,
        plural: &'a str,
        context: Option<&'a str>,
    ) -> std::borrow::Cow<'a, str> {
        Python::try_attach(|py| {
            match if let Some(context) = context {
                self.0.call_method(
                    py,
                    pyo3::intern!(py, "npgettext"),
                    (context, singular, plural, n),
                    None,
                )
            } else {
                self.0.call_method(py, pyo3::intern!(py, "ngettext"), (singular, plural, n), None)
            } {
                Ok(translation) => Some(translation),
                Err(err) => {
                    handle_unraisable(py, "calling npgettext/ngettext".into(), err);
                    None
                }
            }
            .and_then(|maybe_str| maybe_str.extract::<String>(py).ok())
            .map(std::borrow::Cow::Owned)
        })
        .flatten()
        .unwrap_or(std::borrow::Cow::Borrowed(singular))
        .into()
    }
}

use pyo3::prelude::*;

// The native extension is exposed under two names so that the lean release wheel
// (`slint`) and the optional dev wheel (`slint-dev`) can ship binary-compatible
// binaries side by side. The default build registers the module as `slint`
// (imported as `slint.slint`); the dev build, compiled with the `dev-dist`
// feature, registers it as the top-level `slint_dev_native` module. Both expose
// the exact same surface via `register_module`.
#[cfg(not(feature = "dev-dist"))]
#[pymodule]
fn slint(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    register_module(m)
}

#[cfg(feature = "dev-dist")]
#[pymodule]
fn slint_dev_native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    register_module(m)
}

fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    i_slint_backend_selector::with_platform(|_b| {
        // Nothing to do, just make sure a backend was created
        Ok(())
    })
    .map_err(|e| errors::PyPlatformError(e))?;

    m.add_class::<Compiler>()?;
    m.add_class::<CompilationResult>()?;
    m.add_class::<ComponentInstance>()?;
    m.add_class::<ComponentDefinition>()?;
    m.add_class::<image::PyImage>()?;
    m.add_class::<PyValueType>()?;
    m.add_class::<PyDiagnosticLevel>()?;
    m.add_class::<PyDiagnostic>()?;
    m.add_class::<timer::PyTimerMode>()?;
    m.add_class::<timer::PyTimer>()?;
    m.add_class::<brush::PyColor>()?;
    m.add_class::<brush::PyBrush>()?;
    m.add_class::<geometry::PyLogicalPosition>()?;
    m.add_class::<geometry::PyLogicalSize>()?;
    m.add_class::<keys::PyKeys>()?;
    m.add_class::<data_transfer::PyDataTransfer>()?;
    m.add_class::<styled_text::PyStyledText>()?;
    m.add_class::<models::PyModelBase>()?;
    m.add_class::<value::PyStruct>()?;
    m.add_class::<async_adapter::AsyncAdapter>()?;
    m.add_class::<api_match::PyGeneratedAPI>()?;
    m.add_function(wrap_pyfunction!(run_event_loop, m)?)?;
    m.add_function(wrap_pyfunction!(quit_event_loop, m)?)?;
    m.add_function(wrap_pyfunction!(set_xdg_app_id, m)?)?;
    m.add_function(wrap_pyfunction!(request_redraw, m)?)?;
    m.add_function(wrap_pyfunction!(control_at, m)?)?;
    m.add_function(wrap_pyfunction!(hit_test, m)?)?;
    m.add_function(wrap_pyfunction!(set_control_property, m)?)?;
    m.add_function(wrap_pyfunction!(apply_control_state, m)?)?;
    m.add_function(wrap_pyfunction!(invoke_from_event_loop, m)?)?;
    m.add_function(wrap_pyfunction!(init_translations, m)?)?;
    m.add_function(wrap_pyfunction!(build_features, m)?)?;

    language::register_all(m.py(), m)?;

    Ok(())
}
