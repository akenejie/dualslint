// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// dualslint — 2-thread render separation for the Slint GUI toolkit.
//
// This module is the cross-thread protocol and the render-thread GL driver.
//
// Nothing crosses as a frame.  There is no scene serializer left: either the
// window keeps its upstream renderer on the UI thread, or the component is
// handed to the render thread (`RenderHost::attach_component`) and the whole
// screen is drawn here.  In the latter case the app's component is
// instantiated on this thread against a headless window adapter, so the
// upstream Slint draw path — including text shaping — runs on the render
// thread, and the UI thread becomes a peer that borrows the published
// coordinate/state table and asks for changes through
// `RenderHost::apply_control_state`.

#[cfg(feature = "renderer-femtovg")]
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::rc::Rc;
#[cfg(feature = "renderer-femtovg")]
use std::rc::Weak;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

#[cfg(feature = "renderer-femtovg")]
use i_slint_core::api::PhysicalSize;
use i_slint_core::api::Window as SlintApiWindow;
use i_slint_core::graphics::Color;
use i_slint_core::input::{BackendMouseEvent, InternalKeyEvent, PointerEventButton};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::lengths::LogicalPoint;
use i_slint_core::platform::WindowEvent;
#[cfg(feature = "renderer-femtovg")]
use i_slint_core::platform::{Clipboard, Platform, PlatformError};
use i_slint_core::window::{WindowAdapter, WindowInner};

// Brings the extension traits that give `PossiblyCurrentContext` and
// `Surface<WindowSurface>` their `make_current` / `swap_buffers` / `resize`
// methods into scope for the render-thread GL context, plus `GetGlDisplay` for
// `get_proc_address`.
#[cfg(feature = "renderer-femtovg")]
use glutin::display::GetGlDisplay;

use crate::winit_compat::WindowSurfaceSizeExt;
use crate::winitwindowadapter::WinitWindowAdapter;

// The cross-thread control protocol types and the control geometry encoder live
// in the `i-slint-backend-scene` crate, so other 2-thread backends can reuse
// them.
pub use i_slint_backend_scene::*;

// ---------------------------------------------------------------------------
// Shared global state
// ---------------------------------------------------------------------------

/// Render host — the send-half.  The UI thread and any worker thread holds
/// this to reach the render thread.
pub(crate) static GLOBAL_RENDER_HOST: OnceLock<RenderHost> = OnceLock::new();

/// Shared coordinate table between the render thread (writer: publishes the
/// composited controls' geometry + state) and the UI thread / workers
/// (reader: hit-testing during event processing).  Initialised together with
/// the render host in `ensure_render_thread`.
pub(crate) static GLOBAL_COORDINATE_MAP: OnceLock<Arc<Mutex<PublishedControls>>> = OnceLock::new();

/// Global HWND (Windows only) stored when the winit window is created.
#[cfg(target_os = "windows")]
pub(crate) static GLOBAL_HWND: OnceLock<isize> = OnceLock::new();

// ---------------------------------------------------------------------------
// Headless window adapter — used when a render-owned component is attached
// ---------------------------------------------------------------------------

#[cfg(feature = "renderer-femtovg")]
thread_local! {
    /// Stores the adapter created by the headless platform during
    /// `Platform::create_window_adapter` so the caller can retrieve it
    /// after `App::new()` wires everything up.
    static HEADLESS_ADAPTER_SLOT: OnceCell<Rc<dyn WindowAdapter>> = OnceCell::new();
}

/// The renderer the render-owned window reports to the runtime.
///
/// The window is drawn by the upstream renderer bound to this thread's own
/// graphics context, so this one never draws anything itself; it exists because
/// a window has to answer the question, and because a resize has to reach the
/// context that presents it.
#[cfg(feature = "renderer-femtovg")]
struct MirrorRenderer {
    window_adapter: RefCell<Option<Rc<dyn WindowAdapter>>>,
}

#[cfg(feature = "renderer-femtovg")]
impl i_slint_core::renderer::RendererSealed for MirrorRenderer {
    fn set_window_adapter(&self, window_adapter: &Rc<dyn WindowAdapter>) {
        *self.window_adapter.borrow_mut() = Some(window_adapter.clone());
    }

    fn window_adapter(&self) -> Option<Rc<dyn WindowAdapter>> {
        self.window_adapter.borrow().clone()
    }

    fn supports_transformations(&self) -> bool {
        true
    }

    fn resize(&self, _size: i_slint_core::api::PhysicalSize) -> Result<(), PlatformError> {
        // Nothing to forward: this renderer runs *on* the render thread, which
        // is the thread that owns the GL surface, and it resizes that surface
        // itself when the UI thread reports a new window size.  Sending itself
        // a message here would only make the two threads trade the same size
        // back and forth forever.
        Ok(())
    }
}

/// Minimal window adapter for the render-thread component.  The Slint runtime
/// queries it for the window geometry; the drawing is the render thread's
/// upstream renderer, which presents into the window the UI thread created.
#[cfg(feature = "renderer-femtovg")]
struct RenderWindowAdapter {
    window: SlintApiWindow,
    size: Cell<PhysicalSize>,
    /// Set by the tree this adapter hosts when its properties change.  The
    /// render loop reads it to decide that the tree owes the screen a frame.
    frame_request: Rc<Cell<bool>>,
    renderer: MirrorRenderer,
}

#[cfg(feature = "renderer-femtovg")]
impl WindowAdapter for RenderWindowAdapter {
    fn window(&self) -> &SlintApiWindow {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: i_slint_core::api::WindowSize) {
        self.size.set(size.to_physical(self.window.scale_factor()));
    }

    fn renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        // The tree behind this adapter is the render thread's own, and drawing
        // is what the render thread does, so the request stays here: the loop
        // picks the flag up and presents.  Passing it on to the UI thread would
        // let the thread that does not draw decide when the thread that does
        // paints.
        self.frame_request.set(true);
    }
}

/// Headless platform for the render thread.
///
/// A `TextInput` reaches the system clipboard through its window's platform, and
/// the platform on this thread is this one, so without a clipboard of its own a
/// copy would land nowhere the user could paste from -- the one thing a copy has
/// to avoid. `create_window_adapter` is here because the mirror tree needs a
/// window to live in; what else the mirror needs is either a default that works
/// (`duration_since_start`, which gives its animations a clock) or the render
/// thread's own business.
#[cfg(feature = "renderer-femtovg")]
struct RenderMirrorPlatform {
    clipboard: RefCell<crate::clipboard::ClipboardPair>,
    /// Handed to the adapter that hosts the mirror tree, so the tree's redraw
    /// requests reach the render loop.
    frame_request: Rc<Cell<bool>>,
}

#[cfg(feature = "renderer-femtovg")]
impl RenderMirrorPlatform {
    fn new(frame_request: Rc<Cell<bool>>) -> Self {
        Self { clipboard: RefCell::new(crate::clipboard::create_clipboard()), frame_request }
    }
}

#[cfg(feature = "renderer-femtovg")]
impl Platform for RenderMirrorPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let adapter: Rc<RenderWindowAdapter> =
            Rc::new_cyclic(|weak: &Weak<RenderWindowAdapter>| {
                let window = SlintApiWindow::new(weak.clone() as Weak<dyn WindowAdapter>);
                RenderWindowAdapter {
                    window,
                    // Placeholder: the UI thread reports the real size with
                    // `adapter_configured`, which reaches this through `set_size`.
                    size: Cell::new(PhysicalSize::new(800, 600)),
                    frame_request: self.frame_request.clone(),
                    renderer: MirrorRenderer { window_adapter: Default::default() },
                }
            });
        let _ = HEADLESS_ADAPTER_SLOT.with(|slot| slot.set(adapter.clone()));
        Ok(adapter)
    }

    fn set_clipboard_text(&self, text: &str, clipboard: Clipboard) {
        let mut pair = self.clipboard.borrow_mut();
        if let Some(provider) = crate::clipboard::select_clipboard(&mut pair, clipboard) {
            provider.set_contents(text.to_owned()).ok();
        }
    }

    fn clipboard_text(&self, clipboard: Clipboard) -> Option<String> {
        let mut pair = self.clipboard.borrow_mut();
        let provider = crate::clipboard::select_clipboard(&mut pair, clipboard)?;
        provider.get_contents().ok()
    }
}

// ---------------------------------------------------------------------------
// Protocol types (UI thread → render thread)
// ---------------------------------------------------------------------------

/// Messages the UI thread sends to the render thread.
pub enum RenderMessage {
    /// Provide the winit window + initial configuration. The render thread
    /// creates the glutin GL context and FemtoVG canvas on its own thread.
    Configure {
        /// The winit window, created on the UI thread.  Render thread uses
        /// its raw display/window handles to bootstrap glutin.
        window: Arc<winit::window::Window>,
        /// Initial physical pixel width.
        width: u32,
        /// Initial physical pixel height.
        height: u32,
        /// Scale factor for text/transform scaling.
        scale_factor: f64,
    },
    /// The GL surface has been resized.
    Resize {
        /// The winit window that changed size.  This thread draws one window, so
        /// a resize of another one is not its business.
        window_id: winit::window::WindowId,
        /// New physical pixel width.
        width: u32,
        /// New physical pixel height.
        height: u32,
    },
    /// Execute an arbitrary closure on the render thread.
    User(Box<dyn FnOnce() + Send>),
    /// Instantiate the app's component on the render thread.  `factory` runs on
    /// the render thread (it must call the component's `new()` *there*, since
    /// ItemTree/Property are single-threaded) and returns the component's
    /// strong handle, type-erased.  The render thread then draws the whole
    /// window from its own component, running the upstream Slint render path
    /// (text shaping included) on this thread.
    AttachComponent {
        /// Runs on the render thread.  Must return a `Box<dyn Any>` holding
        /// the strong component handle created on this thread.
        factory: Box<dyn FnOnce() -> Box<dyn std::any::Any> + Send>,
        /// Answers for the properties no built-in item spells out, or `None`
        /// when the component does not need it.
        property_access: Option<ComponentPropertyAccess>,
    },
    /// Request a control's interaction state from the render thread, the
    /// visual authority.  Translated into upstream pointer input so the
    /// `has-hover` / `pressed` bindings recompute exactly as if the pointer
    /// were there, then the scene is re-encoded, republished and re-presented.
    ///
    /// Ui thread and workers are equal peers here: both read the shared
    /// coordinate table (`coordinate_map()`) to borrow the current state and
    /// both send this message to request a change.
    ApplyControlState {
        /// The control id (as published in the shared coordinate table).
        id: u64,
        /// Desired pointer-hover state.
        hovered: bool,
        /// Desired pressed (button down) state.
        pressed: bool,
    },
    /// Apply a named property assignment to a control in the render thread's
    /// mirror tree (the control whose id was published in the shared
    /// coordinate table), then re-encode and re-present.  The sender receives
    /// `Ok` on the response channel when the property was found and set.
    ///
    /// Ui thread and external workers are equal peers here.  This is the
    /// property-unit loan API: the caller "borrows" the control and sets one
    /// of its properties directly, exactly as if it held the instance.
    SetControlProperty {
        /// The control id (as published in the shared coordinate table).
        id: u64,
        /// The property name (`"text"`, `"color"`, `"background"`, ...).
        property: String,
        /// The value to assign.
        value: ControlPropertyValue,
        /// Reply channel: `true` when the assignment was applied.
        response: std::sync::mpsc::SyncSender<bool>,
    },
    /// Read one of a control's properties, the way a Ui thread asks what a
    /// control looks like before it decides what an event means.  The reply
    /// carries `None` when the id or the property name does not resolve.
    GetControlProperty {
        /// The control id (as published in the shared coordinate table).
        id: u64,
        /// The property name.
        property: String,
        /// Reply channel carrying the current value.
        response: std::sync::mpsc::SyncSender<Option<ControlPropertyValue>>,
    },
    /// A click landed on a render-owned control.
    ///
    /// The UI thread resolved the pointer against the composited geometry and
    /// decided that the press and the release both landed on this control; what
    /// a click *means* is not something it can decide, because the thing that
    /// answers it -- the `TouchArea` behind a `Button`, the `toggled` handler
    /// behind a `CheckBox`, the `focus-on-click` of a `FocusScope` -- is an
    /// item in the tree on this thread. So the message names the control and
    /// nothing else: no position, no button, no timing. The render thread
    /// turns it into the press and release pair that the pointer would have
    /// produced, which is what runs the application's own callbacks.
    ActivateControl {
        /// The control id (as published in the shared coordinate table).
        id: u64,
        /// Reply channel: `true` when the control took the activation.
        response: std::sync::mpsc::SyncSender<bool>,
    },
    /// A key was pressed, and the control that has the focus lives on this
    /// thread.
    ///
    /// The UI thread's job ends at turning the OS key into a Slint one; the
    /// meaning of that key belongs to the item that has the focus, so the event
    /// is handed to the tree rather than interpreted against a property. The
    /// reply says whether the tree used it.
    KeyToRenderControl {
        /// The key, already in the runtime's own representation.
        event: InternalKeyEvent,
        /// Reply channel: `true` when an item consumed the key.
        response: std::sync::mpsc::SyncSender<bool>,
    },
    /// Re-encode the render thread's mirror component and re-present it.  An
    /// explicit redraw command; also used implicitly after every property
    /// assignment and control-state change.
    RequestRedraw,
    /// Forward the host's system accent colour (from the OS/xdg settings) to
    /// the mirror context.  The mirror has no OS connection of its own, so
    /// without this its widget palette would fall back to the default accent.
    SetAccent {
        /// The colour the OS reports, forwarded unchanged.
        color: Color,
    },
    /// Borrow a control from the render thread for hit-testing: the UI thread
    /// detects a click or key event but the control geometry lives with the
    /// render thread, so it asks here which control owns the logical point.
    /// The reply is the most specific (smallest) control containing the point.
    HitTest {
        /// Logical x coordinate of the pointer.
        x: f32,
        /// Logical y coordinate of the pointer.
        y: f32,
        /// Reply channel carrying the control id, or `None`.
        response: std::sync::mpsc::SyncSender<Option<u64>>,
    },
    /// The UI thread has released the window's graphics context, so the render
    /// thread may create the one context a window is allowed to have.
    GraphicsReleased {
        /// The winit window whose context was released.
        window_id: winit::window::WindowId,
    },
    /// The window the attached component is drawn into.  The UI thread sends this
    /// when the application names a window, and again whenever that window's
    /// native window is created, because a window that was shown again is a
    /// different native window.
    SetSurfaceOwner {
        /// The winit window to present into.
        window_id: winit::window::WindowId,
    },
    /// Drop the render-thread GL state (window hidden / context suspended).
    /// The render thread releases its `Arc<winit::window::Window>`, allowing
    /// the UI thread's `suspend` to actually destroy the native window.
    Suspend {
        /// The winit window that is going away.
        window_id: winit::window::WindowId,
    },
    /// Terminate the render thread.
    Quit,
}

thread_local! {
    /// Whether this thread is the render thread.
    ///
    /// The mirror tree's own window adapter is a [`WinitWindowAdapter`] without a
    /// native window, so the draws that present the mirror run through the same
    /// `draw()` as the UI thread's, one thread away.  A report about who is
    /// drawing has to tell those two apart, or it would accuse the render thread
    /// of drawing the window it exists to draw.
    static ON_RENDER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// The window the render thread draws the attached component into, as the UI
    /// thread knows it: the window the application named when it attached, or --
    /// when it named none -- the first window that was created.
    ///
    /// The choice lives here rather than on the render thread because only this
    /// thread knows which window the application meant, and because the answer has
    /// to be given again whenever the native window is created: a window that was
    /// hidden and shown again is a different native window with a different id,
    /// while the adapter here is the same object.
    static SURFACE_OWNER: std::cell::RefCell<Option<std::rc::Weak<dyn WindowAdapter>>> =
        const { std::cell::RefCell::new(None) };
}

/// Name `window` as the one the render thread draws into, and report its native
/// window if it already has one.  The record outlives the native window, because
/// a window that is hidden and shown again comes back as a new native window.
fn name_surface_owner(window: &SlintApiWindow) -> Option<winit::window::WindowId> {
    let window_adapter = WindowInner::from_pub(window).window_adapter();
    let (weak, window_id) = window_adapter
        .internal(i_slint_core::InternalToken)
        .and_then(|wa| (wa as &dyn std::any::Any).downcast_ref::<WinitWindowAdapter>())
        .map(|adapter| (adapter.self_weak.clone(), adapter.winit_window().map(|w| w.id())))?;
    SURFACE_OWNER.with(|owner| *owner.borrow_mut() = Some(weak));
    window_id
}

// ---------------------------------------------------------------------------
// RenderHost — send half (UI thread + workers)
// ---------------------------------------------------------------------------

/// The send half of the render-thread channel.  Clonable and Send-safe.
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,
    event_loop_proxy: Option<winit::event_loop::EventLoopProxy<crate::SlintEvent>>,
    /// Set to `true` once `AttachComponent` has been sent.  Used to suppress
    /// the UI-thread encode path once the render thread owns the screen.
    /// Shared across all clones of the host so every thread sees the state.
    attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Clone for RenderHost {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            event_loop_proxy: self.event_loop_proxy.clone(),
            attached: self.attached.clone(),
        }
    }
}

impl RenderHost {
    /// Whether a render-owned component has been attached and is now the
    /// visual authority on the render thread.
    pub fn has_attached_component(&self) -> bool {
        self.attached.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Send the app's component factory to the render thread.  `factory`
    /// executes on the render thread and must call the generated `App::new()`
    /// *there* (after seeding a headless platform via
    /// `RenderMirrorPlatform`).  The strong component handle is leaked so the
    /// render tree stays alive for the lifetime of the render thread.
    ///
    /// Once attached the UI-thread encode path (`dual.rs`) is suppressed;
    /// the render thread redraws from its own component on every
    /// `submit_scene` or `apply_control_state` call.
    ///
    /// Without a window to draw into, the first window that gets created is the
    /// one: an application with several windows should say which one it means
    /// with [`Self::attach_component_to`].
    pub fn attach_component<F>(&self, factory: F)
    where
        F: FnOnce() -> Box<dyn std::any::Any> + Send + 'static,
    {
        self.attach_component_with(factory, None)
    }

    /// [`Self::attach_component`], with a way to reach the properties that
    /// belong to a `.slint` component rather than to a built-in item.
    ///
    /// A widget from a `.slint` file is a group of items and the bindings
    /// between them, so most of what the application asks about — whether a
    /// `CheckBox` is `checked` — is a property of that group and of no item in
    /// it. The render thread cannot answer on its own, because it is a
    /// windowing backend and does not know what a `.slint` component is;
    /// `property_access` is how the application, which does, lends it that
    /// knowledge for the component it attached.
    pub fn attach_component_with<F>(
        &self,
        factory: F,
        property_access: Option<ComponentPropertyAccess>,
    ) where
        F: FnOnce() -> Box<dyn std::any::Any> + Send + 'static,
    {
        self.attached.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .sender
            .send(RenderMessage::AttachComponent { factory: Box::new(factory), property_access });
    }

    /// [`Self::attach_component`], naming the window the component is drawn
    /// into.
    ///
    /// An application with several windows has to say which one the component
    /// belongs to.  The other windows keep their upstream renderer on the UI
    /// thread and are drawn there, which is the whole point of this setup: the
    /// UI thread does the input, and the render thread does the drawing, but
    /// neither thread is told to do it by whichever window happened to be
    /// created first.
    ///
    /// The window need not exist yet, and hiding it does not change the answer:
    /// showing it again creates a new native window, and the UI thread says
    /// which window that is.
    pub fn attach_component_to<F>(&self, window: &SlintApiWindow, factory: F)
    where
        F: FnOnce() -> Box<dyn std::any::Any> + Send + 'static,
    {
        self.attach_component_with_to(window, factory, None)
    }

    /// [`Self::attach_component_to`], with
    /// [`Self::attach_component_with`]'s way to reach the properties that belong
    /// to a `.slint` component rather than to a built-in item.
    pub fn attach_component_with_to<F>(
        &self,
        window: &SlintApiWindow,
        factory: F,
        property_access: Option<ComponentPropertyAccess>,
    ) where
        F: FnOnce() -> Box<dyn std::any::Any> + Send + 'static,
    {
        if let Some(window_id) = name_surface_owner(window) {
            let _ = self.sender.send(RenderMessage::SetSurfaceOwner { window_id });
        }
        self.attached.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .sender
            .send(RenderMessage::AttachComponent { factory: Box::new(factory), property_access });
    }

    /// Request the render thread to apply a pointer state to the given control
    /// in its own component and redraw.  Ui thread and workers are equal
    /// peers here: both call this to indicate that a control should be
    /// hovered / pressed.
    pub fn apply_control_state(&self, id: u64, hovered: bool, pressed: bool) {
        let _ = self.sender.send(RenderMessage::ApplyControlState { id, hovered, pressed });
    }

    /// Tell the render thread that a click landed on a render-owned control,
    /// blocking until the tree there has applied it.  Returns `true` when the
    /// control took the activation.
    ///
    /// The UI thread knows which control the pointer landed on; it does not
    /// know what a click means, and it must not decide -- the `clicked`,
    /// `toggled` and `accepted` handlers that answer it belong to the items in
    /// the tree on the render thread, and they run there with the application's
    /// own code. So this is a statement about the control, not a command to
    /// imitate one: no widget is named, no property is set, and nothing here
    /// has to be updated when a new widget is added.
    ///
    /// Ui thread and library-external workers are equal peers here.
    pub fn activate_control(&self, id: u64) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let _ = self.sender.send(RenderMessage::ActivateControl { id, response: tx });
        rx.recv().unwrap_or(false)
    }

    /// Hand a key to the tree on the render thread, blocking until it answers.
    /// Returns `true` when an item there consumed the key.
    ///
    /// The key arrives in the runtime's own representation because that is
    /// where the OS key stops being an OS key: the UI thread's work is done
    /// once the key is named, and from there on the item that has the focus
    /// decides what it means. A field, a `Flickable`, a widget with a
    /// `key-pressed` handler and a `TextInput` are then all answered by the
    /// same code that answers them when the pointer does reach the tree.
    pub fn send_key_to_control(&self, event: &InternalKeyEvent) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let _ = self
            .sender
            .send(RenderMessage::KeyToRenderControl { event: event.clone(), response: tx });
        rx.recv().unwrap_or(false)
    }

    /// Borrow the render-owned control identified by `id` and assign one of
    /// its properties, blocking until the render thread confirms the
    /// assignment (detaching any previous binding, upstream-style).  Returns
    /// `true` when the property name resolved and the value was applied.
    ///
    /// Ui thread and library-external workers are equal peers here.
    pub fn set_control_property(
        &self,
        id: u64,
        property: &str,
        value: ControlPropertyValue,
    ) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let _ = self.sender.send(RenderMessage::SetControlProperty {
            id,
            property: property.to_string(),
            value,
            response: tx,
        });
        rx.recv().unwrap_or(false)
    }

    /// Ask for a window repaint (an explicit redraw command).
    ///
    /// A window this thread draws is re-encoded and re-presented here.  A window
    /// whose graphics this thread never took is drawn by the UI thread, so the
    /// request is handed to that thread instead, which skips the windows this
    /// thread owns.  Either way the thread that draws decides when to present.
    pub fn request_redraw(&self) {
        self.submit_repaint_unowned_windows();
        let _ = self.sender.send(RenderMessage::RequestRedraw);
    }

    /// Read one of a control's properties, blocking until the render thread
    /// answers.
    ///
    /// The `.slint` side of the tree belongs to the render thread, so a Ui
    /// thread that needs to know what a control looks like asks here instead
    /// of keeping a second copy of the component to answer the question
    /// itself.  Ui thread and library-external workers are equal peers here.
    ///
    /// Returns `None` when the id or the property name does not resolve.
    pub fn get_control_property(&self, id: u64, property: &str) -> Option<ControlPropertyValue> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = self.sender.send(RenderMessage::GetControlProperty {
            id,
            property: property.to_string(),
            response: tx,
        });
        rx.recv().unwrap_or(None)
    }

    /// Borrow the render-owned controls for hit-testing: asks the render
    /// thread which control owns the logical point `(x, y)` and returns the
    /// most specific control id.  The UI thread calls this when it detects a
    /// click or key event, since the control geometry lives with the render
    /// thread.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<u64> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Option<u64>>(1);
        let _ = self.sender.send(RenderMessage::HitTest { x, y, response: tx });
        rx.recv().unwrap_or(None)
    }

    /// The control that owns the logical point `(x, y)`, read from the geometry
    /// the render thread last composited.
    ///
    /// Unlike [`Self::hit_test`] this does not wait for the render thread, which
    /// is what makes it usable for pointer moves: the pointer position only
    /// exists on the thread that received the OS event, so blocking that thread
    /// on every move would put the render thread in the input path and stall
    /// input behind compositing.  The answer is one composite old at worst,
    /// which for a pointer move is imperceptible.
    pub fn control_at(&self, x: f32, y: f32) -> Option<u64> {
        coordinate_map()?.lock().unwrap().control_at(x, y)
    }

    /// Send an arbitrary closure to execute on the render thread.
    pub fn send_user(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(RenderMessage::User(Box::new(f)));
    }

    /// Ask the render thread to quit.
    pub fn send_quit(&self) {
        let _ = self.sender.send(RenderMessage::Quit);
    }

    /// Configure the render thread with the winit window + initial size.
    pub(crate) fn submit_configure(
        &self,
        window: Arc<winit::window::Window>,
        width: u32,
        height: u32,
        scale_factor: f64,
        adapter: std::rc::Weak<dyn WindowAdapter>,
    ) {
        let window_id = window.id();
        let _ = self.sender.send(RenderMessage::Configure { window, width, height, scale_factor });
        // A window is the surface or it is not, and this thread is the one that
        // can tell: an application with several windows attaches to the one it
        // means, and everything else stays the UI thread's to draw.
        let is_surface = SURFACE_OWNER.with(|owner| {
            let mut owner = owner.borrow_mut();
            if owner.is_none() {
                *owner = Some(adapter.clone());
            }
            owner.as_ref().is_some_and(|known| known.ptr_eq(&adapter))
        });
        if is_surface {
            let _ = self.sender.send(RenderMessage::SetSurfaceOwner { window_id });
        }
    }

    /// Notify the render thread that the window was resized.
    pub(crate) fn submit_resize(
        &self,
        window_id: winit::window::WindowId,
        width: u32,
        height: u32,
    ) {
        let _ = self.sender.send(RenderMessage::Resize { window_id, width, height });
    }

    /// Forward the host's system accent colour to the mirror context so the
    /// mirror widget palette matches what the host would render (checked
    /// boxes, highlights, ...).  No-op outside the dualslint 2-thread path.
    pub(crate) fn submit_accent(&self, color: Color) {
        let _ = self.sender.send(RenderMessage::SetAccent { color });
    }

    /// Ask the render thread to tear down its GL context and release the window.
    pub(crate) fn submit_suspend(&self, window_id: winit::window::WindowId) {
        let _ = self.sender.send(RenderMessage::Suspend { window_id });
    }

    /// Tell the UI thread that the render thread needs the window's graphics.
    ///
    /// A window has one OpenGL context, not one per thread, so the render
    /// thread cannot have a drawing context while the UI thread's renderer
    /// holds one. Asking is what keeps that from becoming a driver-level
    /// failure: the UI thread releases its own context and answers with
    /// [`RenderMessage::GraphicsReleased`].
    pub(crate) fn submit_graphics_handover(&self, window_id: winit::window::WindowId) {
        if let Some(proxy) = &self.event_loop_proxy {
            let _ = proxy.send_event(crate::SlintEvent(
                crate::event_loop::CustomEvent::HandOverGraphics { window_id },
            ));
        }
    }

    /// Hand a repaint request to the UI thread for the windows this one does
    /// not draw.
    ///
    /// A window that never handed its graphics over is painted by the UI thread,
    /// and only that thread can paint it.  Windows the render thread owns are
    /// filtered out on arrival, so this cannot make a second thread present a
    /// render-owned window.
    pub(crate) fn submit_repaint_unowned_windows(&self) {
        if let Some(proxy) = &self.event_loop_proxy {
            let _ = proxy.send_event(crate::SlintEvent(
                crate::event_loop::CustomEvent::RepaintUnownedWindows,
            ));
        }
    }

    /// Report that the UI thread's renderer has released the window's graphics.
    pub(crate) fn notify_graphics_released(&self, window_id: winit::window::WindowId) {
        let _ = self.sender.send(RenderMessage::GraphicsReleased { window_id });
    }
}

// ---------------------------------------------------------------------------
// RenderCore — receive half (runs on the render thread)
// ---------------------------------------------------------------------------

/// The render-thread receive half and event loop driver.
pub(crate) struct RenderCore {
    rx: mpsc::Receiver<RenderMessage>,
    host: RenderHost,
}

/// The window the render thread presents into.
///
/// A window has one OpenGL context, not one per thread, so the render thread
/// cannot have a drawing context while the UI thread's renderer holds one: a
/// second context on the same window is refused by the driver. That is why the
/// configured window is only kept here, and the context is created once the UI
/// thread has given up its own.
struct RenderSurface {
    window: Arc<winit::window::Window>,
    width: u32,
    height: u32,
    /// The window's scale factor, which a resize message does not repeat: only
    /// the pixel size changed, so the logical size the tree is laid out for is
    /// that size at this factor.
    #[cfg(feature = "renderer-femtovg")]
    scale_factor: f32,
}

/// The render thread's side of the windows the UI thread has created: each
/// window's size, and the drawing context of the one this thread draws into.
///
/// A window has one OpenGL context and this thread has one tree, so one window is
/// *the* surface: the one the attached component asked for, or -- when the
/// application did not say -- the first one that was configured. The others are
/// the UI thread's to draw, because this thread has no tree for them.
///
/// Every message about a window says which window it is about. That is not
/// bookkeeping: a window whose graphics were handed over is drawn by this thread
/// and by nothing else, so a `Resize` or a `Suspend` that landed on the wrong
/// window would either resize the wrong surface or -- worse -- let go of a window
/// that is still on screen and leave it blank.
struct SurfaceState {
    /// The windows the UI thread has configured, in the order they arrived, with
    /// their current size.  An application can create its windows in any order and
    /// attach to one of them later, so every one of them is kept until it is
    /// suspended: the choice of which to draw into is made when it is made, not
    /// when the first window happens to appear.
    configured: Vec<(winit::window::WindowId, RenderSurface)>,
    /// The window the attached component is drawn into.
    owner: Option<winit::window::WindowId>,
    /// The glutin context and the upstream renderer bound to it.
    gl: Option<GlRenderState>,
    /// Set while the UI thread still owns the owner's graphics, which keeps this
    /// thread from creating a context of its own until it answers.
    handover_pending: bool,
}

impl SurfaceState {
    fn new() -> Self {
        Self { configured: Vec::new(), owner: None, gl: None, handover_pending: false }
    }

    /// Remember a window and its size, without touching its graphics yet.
    fn configure(
        &mut self,
        window: Arc<winit::window::Window>,
        width: u32,
        height: u32,
        #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_variables))] scale_factor: f32,
    ) {
        let window_id = window.id();
        let surface = RenderSurface {
            window,
            width,
            height,
            #[cfg(feature = "renderer-femtovg")]
            scale_factor,
        };
        match self.configured.iter_mut().find(|(id, _)| *id == window_id) {
            Some((_, known)) => *known = surface,
            None => self.configured.push((window_id, surface)),
        }
    }

    /// Name the window the attached component is drawn into.  Returns whether
    /// that was a change, which is when something has to be drawn again.
    ///
    /// The UI thread decides which window this is, because only it knows which
    /// window the application meant.  Naming a different one gives the old one
    /// back to the UI thread: its graphics were never handed over -- this thread
    /// is being told to ask for another window's -- so it still draws itself.
    fn set_owner(&mut self, window_id: winit::window::WindowId) -> bool {
        if self.owner == Some(window_id) {
            return false;
        }
        self.owner = Some(window_id);
        // The context belongs to the window it was created for.
        self.drop_context();
        true
    }

    /// The surface to present into.
    fn surface(&self) -> Option<&RenderSurface> {
        let owner = self.owner?;
        self.configured.iter().find(|(id, _)| *id == owner).map(|(_, surface)| surface)
    }

    /// Whether the surface belongs to this window.
    fn owns(&self, window_id: winit::window::WindowId) -> bool {
        self.surface().is_some_and(|surface| surface.window.id() == window_id)
    }

    /// The window's scale factor, or 1 before it has been configured.
    #[cfg(feature = "renderer-femtovg")]
    fn scale_factor(&self) -> f32 {
        self.surface().map_or(1., |surface| surface.scale_factor)
    }

    /// Whether the surface's graphics are on their way from the UI thread.  The
    /// loop waits to be spoken to rather than on the clock until they arrive.
    #[cfg(feature = "renderer-femtovg")]
    fn handover_pending(&self) -> bool {
        self.handover_pending
    }

    fn obtain(&mut self, host: &RenderHost) -> Option<&mut GlRenderState> {
        if self.gl.is_none() && !self.handover_pending {
            // A component can be attached before the window exists, and a
            // handover can only name a window, so the request waits for the
            // window rather than being marked as asked for.
            if let Some(window_id) = self.surface().map(|surface| surface.window.id()) {
                self.handover_pending = true;
                host.submit_graphics_handover(window_id);
            }
        }
        // A resize that arrived while the handover was in flight has not been
        // given to the context yet; this is where it takes effect.
        let wanted = self.surface().map(|surface| (surface.width, surface.height));
        if let (Some(gl), Some((width, height))) = (self.gl.as_mut(), wanted)
            && (gl.width != width || gl.height != height)
        {
            gl.resize(width, height);
        }
        self.gl.as_mut()
    }

    /// Record that the UI thread released the window's graphics, and create the
    /// context now that the window has none.  Returns the state to present
    /// with, or `None` if the release names a window this thread does not draw
    /// into, or the context cannot be created.
    fn released(&mut self, window_id: winit::window::WindowId) -> Option<&mut GlRenderState> {
        if !self.owns(window_id) {
            // Another window's UI renderer let go of its context. That is not an
            // answer to a question this thread asked, so the handover is still
            // open for the window that was asked about.
            return None;
        }
        self.handover_pending = false;
        if self.gl.is_some() {
            return self.gl.as_mut();
        }
        let surface = self.surface()?;
        match GlRenderState::new(surface.window.clone(), surface.width, surface.height) {
            Ok(state) => {
                self.gl = Some(state);
                self.gl.as_mut()
            }
            Err(e) => {
                eprintln!("dualslint render thread: GL init failed: {e}");
                None
            }
        }
    }

    /// Record the size the UI thread reports.
    ///
    /// A resize does not take the window's graphics: the drawing context
    /// follows the size at the next present, and a handover that is still in
    /// flight builds its context at the new size rather than the one the window
    /// had when it was asked for.
    ///
    /// The size is kept for any window, not only the one being drawn into: an
    /// application can attach to this window later, and a context built at the
    /// size the window had when it was asked for is a context of the wrong size.
    fn note_resize(&mut self, window_id: winit::window::WindowId, width: u32, height: u32) {
        if let Some((_, surface)) = self.configured.iter_mut().find(|(id, _)| *id == window_id) {
            surface.width = width;
            surface.height = height;
        }
    }

    /// Let go of a window that is going away, so the UI thread's `suspend` can
    /// destroy it.  Returns whether it was the window being drawn into; the
    /// drawing context goes with it, and the published geometry describes a
    /// window that no longer exists.
    fn release(&mut self, window_id: winit::window::WindowId) -> bool {
        self.configured.retain(|(id, _)| *id != window_id);
        if !self.owns(window_id) {
            return false;
        }
        // The window is gone, and showing it again makes a *new* native window
        // with a new id, which the UI thread names again when it creates it.
        self.owner = None;
        self.drop_context();
        true
    }

    /// Forget the drawing context and the window it belongs to, keeping the
    /// record of the windows themselves.
    fn drop_context(&mut self) {
        self.gl = None;
        self.handover_pending = false;
    }
}

impl RenderCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, host: RenderHost) -> Self {
        Self { rx, host }
    }

    /// Run the render-thread event loop.  Blocks until `Quit`.
    pub(crate) fn run(&mut self) {
        ON_RENDER_THREAD.with(|on| on.set(true));
        // Render-thread state: the window to present into, and the GL context
        // and upstream renderer once the UI thread has handed the window's
        // graphics over.
        let mut surface = SurfaceState::new();
        // Mirror state lives only here, on this thread — it would make
        // `RenderCore` non-Send otherwise.
        // Filled in by the attach handler, which only the GPU renderers run.
        #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_mut))]
        let mut render_window_adapter: Option<Rc<dyn WindowAdapter>> = None;
        let mut render_controls: HashMap<u64, ControlRegion> = HashMap::new();
        // The same control ids in paint order, so a hit test can tell which of
        // several overlapping controls is on top.
        let mut render_control_order: Vec<u64> = Vec::new();
        // Render-side item behind each control id, so property loans can
        // assign to the mirror tree item directly.  Stays on this thread.
        let mut render_item_rcs: HashMap<u64, ItemRc> = HashMap::new();
        let mut interaction = ControlInteraction::default();
        // Keeps the mirror component tree alive for the lifetime of the
        // render thread (the strong handle owns the ItemTree).  Moved in
        // only from `AttachComponent`; never leaves this thread.
        // Filled in by the attach handler, which only the GPU renderers run.
        #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_mut))]
        let mut render_component: Option<Box<dyn std::any::Any>> = None;
        // How to ask the application about the properties of that component.
        // Filled in by the attach handler, which only the GPU renderers run.
        #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_mut))]
        let mut render_property_access: Option<ComponentPropertyAccess> = None;
        // Last system accent forwarded by the UI thread; applied to the
        // mirror context on attach in case the accent update arrives before
        // the mirror component exists.
        // Read back in the mirror's attach, which only the GPU renderers do.
        #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_variables))]
        let mut accent: Option<Color> = None;
        // Set by the mirror tree when its own properties change, so the loop
        // knows the tree owes the screen a frame without the UI thread having to
        // ask for it.  A fresh cell per attachment: the flag belongs to the tree
        // that set it, not to this thread.
        #[cfg(feature = "renderer-femtovg")]
        let mut frame_request: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        loop {
            // How long this thread may sleep.  A running animation is not a
            // timer, so while one plays the wait is capped at a frame interval;
            // otherwise the answer is when the tree's next timer is due.  This
            // is the shape the android-activity loop uses.
            #[cfg(feature = "renderer-femtovg")]
            let wait = if surface.handover_pending() {
                // Nothing this thread draws can reach the screen until the UI
                // thread hands the window's graphics over, and that answer comes
                // as a message.  Watching the clock until then would only spin on
                // a deadline that no frame can act on yet.
                None
            } else {
                let frame = std::time::Duration::from_millis(RENDER_FRAME_INTERVAL_MS);
                let next_timer = i_slint_core::platform::duration_until_next_timer_update();
                if animating(&render_window_adapter) {
                    Some(next_timer.map_or(frame, |timer| timer.min(frame)))
                } else {
                    next_timer
                }
            };
            // Without a mirror tree there is no clock to keep here: nothing on
            // this thread changes on its own, so it sleeps until it is spoken to.
            #[cfg(not(feature = "renderer-femtovg"))]
            let wait: Option<std::time::Duration> = None;
            let msg = match wait {
                Some(wait) => match self.rx.recv_timeout(wait) {
                    Ok(msg) => Some(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
            };
            // What woke this thread is the only reason there can be a frame to
            // draw: a message asked for one, or the wait ran out because an
            // animation is playing.  Deciding to draw is this thread's own
            // business; nothing about the frame crosses from the UI thread.
            // A wait that ran out is this thread's own clock moving: a timer came
            // due, or an animation is playing.  Firing that timer is part of
            // drawing the frame, so the timeout is itself a reason to present.
            // Leaving it out deadlocks the two: a due timer only becomes an
            // active animation by firing, and only an active animation gets this
            // thread to wake for the next one, so the clock stays pinned at the
            // same deadline and the screen keeps the frame it has.
            let mut present = msg.is_none();
            if let Some(msg) = msg {
                match msg {
                    #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_variables))]
                    RenderMessage::Configure { window, width, height, scale_factor } => {
                        let window_id = window.id();
                        surface.configure(window, width, height, scale_factor as f32);
                        // A window this thread does not draw into is the UI
                        // thread's to draw, and the tree here is not told about
                        // its size either: a tree laid out for another window is a
                        // tree whose controls sit where nobody is pointing.
                        if surface.owns(window_id) {
                            // The headless adapter that hosts the render-owned
                            // component has to learn the window's size, or its
                            // layout is computed for a different size than the one
                            // we present into and the right-hand side is clipped
                            // away.
                            #[cfg(feature = "renderer-femtovg")]
                            if let Some(adapter) = &render_window_adapter {
                                adapter_configured(
                                    adapter,
                                    PhysicalSize::new(width, height),
                                    scale_factor as f32,
                                );
                            }
                            // If a component was attached before the window
                            // existed, present it now.
                            present = true;
                        }
                    }
                    RenderMessage::SetSurfaceOwner { window_id } => {
                        if surface.set_owner(window_id) {
                            present = true;
                        }
                    }
                    RenderMessage::GraphicsReleased { window_id } => {
                        // The window has no context on this side of the handover
                        // yet; take the one it is allowed to have.
                        if surface.released(window_id).is_some() {
                            present = true;
                        }
                    }
                    RenderMessage::Resize { window_id, width, height } => {
                        if !surface.owns(window_id) {
                            continue;
                        }
                        // The render-owned tree is laid out for the window's size,
                        // so a resize has to reach the tree and not only the GL
                        // surface: a tree laid out for the old size is a tree whose
                        // controls sit where the user no longer points.
                        #[cfg(feature = "renderer-femtovg")]
                        if let Some(adapter) = &render_window_adapter {
                            adapter_configured(
                                adapter,
                                PhysicalSize::new(width, height),
                                surface.scale_factor(),
                            );
                        }
                        surface.note_resize(window_id, width, height);
                        present = true;
                    }
                    #[allow(unused_assignments)]
                    RenderMessage::SetAccent { color } => {
                        // Replayed onto the mirror when the component is attached,
                        // in case the accent arrives before that.
                        accent = Some(color);
                        if let Some(adapter) = &render_window_adapter {
                            let context = WindowInner::from_pub(adapter.window()).context();
                            context.set_accent_color(color);
                        }
                    }
                    RenderMessage::User(f) => {
                        f();
                    }
                    // A mirror needs a GL context to present into, so it exists
                    // only on the GPU renderers; the software build keeps replaying
                    // the UI-side tree and has no use for the component.
                    #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_variables))]
                    RenderMessage::AttachComponent { factory, property_access } => {
                        // The factory runs *after* the headless platform is seeded:
                        // it instantiates the app's component, which needs this
                        // thread's `GLOBAL_CONTEXT` to already be claimed.
                        #[cfg(feature = "renderer-femtovg")]
                        {
                            // A tree that has just been built owes the screen a
                            // frame, and the adapter that hosts it reports the ones
                            // that follow.
                            frame_request = Rc::new(Cell::new(true));
                            let _ = i_slint_core::platform::set_platform(Box::new(
                                RenderMirrorPlatform::new(frame_request.clone()),
                            ));
                            render_component = Some(factory());
                            render_property_access = property_access;
                            render_window_adapter =
                                HEADLESS_ADAPTER_SLOT.with(|slot| slot.get().cloned());
                            // Mirror the host's system accent into the mirror context
                            // so widget palettes (checked boxes, etc.) resolve the
                            // same colour the host would, instead of the default.
                            if let Some(accent_color) = accent
                                && let Some(adapter) = &render_window_adapter
                            {
                                WindowInner::from_pub(adapter.window())
                                    .context()
                                    .set_accent_color(accent_color);
                            }
                            // From here on the render thread owns every control.  Drop
                            // the UI-thread's publish so peers never borrow stale ids.
                            if let Some(map) = coordinate_map() {
                                map.lock().unwrap().clear();
                            }
                            present = true;
                        }
                    }
                    RenderMessage::ApplyControlState { id, hovered, pressed } => {
                        // Hover and press are *state* the UI thread resolved from
                        // the published geometry, and both are plain properties of
                        // the interactive item, so they are assigned rather than
                        // synthesized as input. The click itself is not state: it
                        // is a decision the item tree has to make, and that is what
                        // `ActivateControl` below delegates to the tree.
                        let Some(item_rc) = render_item_rcs.get(&id) else {
                            continue;
                        };
                        let mut changed = false;
                        if hovered != interaction.is_hovered(id) {
                            interaction.set_hovered(id, hovered);
                            changed |= apply_control_property(
                                render_component.as_deref(),
                                render_property_access.as_ref(),
                                item_rc,
                                "has-hover",
                                &ControlPropertyValue::Bool(hovered),
                            );
                        }
                        if pressed != interaction.is_pressed(id) {
                            interaction.set_pressed(id, pressed);
                            changed |= apply_control_property(
                                render_component.as_deref(),
                                render_property_access.as_ref(),
                                item_rc,
                                "pressed",
                                &ControlPropertyValue::Bool(pressed),
                            );
                        }
                        // Re-encode and present so the new state is visible.
                        present |= changed;
                    }
                    RenderMessage::SetControlProperty { id, property, value, response } => {
                        let ok = render_item_rcs
                            .get(&id)
                            .map(|item_rc| {
                                apply_control_property(
                                    render_component.as_deref(),
                                    render_property_access.as_ref(),
                                    item_rc,
                                    &property,
                                    &value,
                                )
                            })
                            .unwrap_or(false);
                        let _ = response.send(ok);
                        // Re-encode and re-present so the borrowed property change
                        // is visible on screen (also refreshes the id→item map).
                        present |= ok;
                    }
                    RenderMessage::GetControlProperty { id, property, response } => {
                        // Answering a read changes nothing on screen, so this does
                        // not re-present: the caller only needed to know the value.
                        let value = render_item_rcs.get(&id).and_then(|item_rc| {
                            read_control_property(
                                render_component.as_deref(),
                                render_property_access.as_ref(),
                                item_rc,
                                &property,
                            )
                        });
                        let _ = response.send(value);
                    }
                    RenderMessage::ActivateControl { id, response } => {
                        // A press and a release at the same point, which is all a
                        // click is before the tree has had its say about it.
                        let activated = render_window_adapter
                            .as_ref()
                            .zip(render_item_rcs.get(&id))
                            .is_some_and(|(adapter, item_rc)| activate_control(adapter, item_rc));
                        let _ = response.send(activated);
                        // The tree changed: a `pressed` binding fired, a `clicked`
                        // handler may have changed anything at all.
                        present |= activated;
                    }
                    RenderMessage::KeyToRenderControl { event, response } => {
                        let used = render_window_adapter
                            .as_ref()
                            .is_some_and(|adapter| send_key_to_tree(adapter, &event));
                        let _ = response.send(used);
                        // A key that a `TextInput` took changed its text, and the
                        // `accepted` handler that goes with it may have changed the
                        // rest of the application.
                        present |= used;
                    }
                    RenderMessage::HitTest { x, y, response } => {
                        // The last control in paint order that contains the point is
                        // the topmost one, which is the one the user is pointing at.
                        // Same rule as `control_at`, so a caller that reads the
                        // published table and a caller that asks the render thread
                        // cannot disagree.
                        let hit = render_control_order
                            .iter()
                            .rev()
                            .find(|id| {
                                render_controls
                                    .get(id)
                                    .is_some_and(|r| r.geometry.contains(LogicalPoint::new(x, y)))
                            })
                            .copied();
                        let _ = response.send(hit);
                    }
                    RenderMessage::RequestRedraw => {
                        // Repaint what this thread draws.  A window whose graphics it does
                        // not own was handed to the UI thread by
                        // `submit_repaint_unowned_windows`, which is where the windows drawn
                        // elsewhere get their repaint.
                        present = true;
                    }
                    RenderMessage::Suspend { window_id } => {
                        // Drop the GL context + canvas and release the winit window
                        // Arc so the UI thread can destroy the native window. A
                        // window that is not the one being drawn into leaves the
                        // surface alone: its published geometry belongs to the
                        // window that is still there.
                        if !surface.release(window_id) {
                            continue;
                        }
                        // The component and its window stay.  They belong to this
                        // thread, not to a native window, and showing the window
                        // again brings a new one to draw them into: a `Configure`
                        // re-lays the tree out for the new size and asks for the
                        // new window's graphics.  The published geometry goes,
                        // because it described a window that no longer exists.
                        render_controls.clear();
                        render_control_order.clear();
                        render_item_rcs.clear();
                        interaction.clear();
                    }
                    RenderMessage::Quit => break,
                }
            }

            // The tree can also be the reason there is a frame to draw: its own
            // properties changed while nothing was happening, or an animation is
            // playing and the frame clock is not a timer.
            #[cfg(feature = "renderer-femtovg")]
            let (frame_requested, animating_now) =
                (frame_request.replace(false), animating(&render_window_adapter));
            #[cfg(not(feature = "renderer-femtovg"))]
            let (frame_requested, animating_now) = (false, false);
            // Only a window this thread has a tree for is drawn here.  Without
            // one the render thread stays out of the way, and its context must
            // never be taken from a window the UI thread draws itself.
            if render_component.is_some()
                && (present || frame_requested || animating_now)
                && let Some(state) = surface.obtain(&self.host)
            {
                present_render_owned(
                    state,
                    &render_window_adapter,
                    &mut render_controls,
                    &mut render_control_order,
                    &mut render_item_rcs,
                    &mut interaction,
                );
            }
        }
    }
}

/// Walk the render-owned mirror component and record what the UI thread needs
/// to know about its controls: the geometry to hit-test a pointer against, and
/// the `ItemRc` behind each id to answer and assign a control's properties.
/// Returns `false` if the adapter is not ready yet.
#[cfg(feature = "renderer-femtovg")]
#[allow(clippy::too_many_arguments)]
fn publish_mirror_controls(
    render_window_adapter: &Option<Rc<dyn WindowAdapter>>,
    render_controls: &mut HashMap<u64, ControlRegion>,
    render_control_order: &mut Vec<u64>,
    render_item_rcs: &mut HashMap<u64, ItemRc>,
    interaction: &mut ControlInteraction,
) -> bool {
    let Some(adapter) = render_window_adapter.as_ref() else {
        return false;
    };
    let adapter: &dyn WindowAdapter = &**adapter;
    // The mirror has no event loop of its own, so its timers and
    // `.slint`-declared animations must be advanced on this thread before
    // encoding; otherwise the render-owned tree would show a frozen clock.
    i_slint_core::platform::update_timers_and_animations();
    let Ok(table) = i_slint_backend_scene::controls::encode_window_controls(adapter.window())
    else {
        return false;
    };
    *render_controls = table.controls.iter().map(|r| (r.id, r.clone())).collect();
    render_control_order.clear();
    render_control_order.extend(table.controls.iter().map(|r| r.id));
    render_item_rcs.clear();
    render_item_rcs.extend(table.item_refs);
    interaction.retain(&table.controls);
    // The only publish on the GPU path. `render_scene`, which publishes as a
    // side effect of compositing an encoded frame, does not run there, and
    // without this the UI thread would hit-test an empty table and no pointer
    // event would ever reach a control.
    publish_control_coords(&table.controls, interaction);
    true
}

/// How long the render thread waits between frames while an animation plays.
/// It has no vsync source of its own, so this stands in for the display's
/// refresh interval; the android-activity loop uses the same fallback.
#[cfg(feature = "renderer-femtovg")]
const RENDER_FRAME_INTERVAL_MS: u64 = 10;

/// Whether the render-owned tree is animating, which is what tells the render
/// thread that the screen is owed a frame without a message to say so.
#[cfg(feature = "renderer-femtovg")]
fn animating(adapter: &Option<Rc<dyn WindowAdapter>>) -> bool {
    adapter.as_ref().is_some_and(|adapter| adapter.window().has_active_animations())
}

/// Tell the headless adapter that hosts the render-owned component about the
/// real window geometry.  Without this its layout stays at the placeholder
/// size from `create_window_adapter`, and the tree is laid out and clipped
/// for a window that is not the one being presented into.
#[cfg(feature = "renderer-femtovg")]
fn adapter_configured(adapter: &Rc<dyn WindowAdapter>, size: PhysicalSize, scale_factor: f32) {
    // The upstream renderer asks the platform how big the window is rather than
    // taking it from the tree, so the adapter itself has to be told as well.
    adapter.set_size(i_slint_core::api::WindowSize::Physical(size));
    let slint_window = adapter.window();
    slint_window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
    slint_window.dispatch_event(WindowEvent::Resized { size: size.to_logical(scale_factor) });
}

/// Refresh the control table from the render-owned tree and present it.
///
/// The drawing is upstream's: `render_upstream` hands the render-owned item
/// tree to `FemtoVGRenderer`, so no private re-implementation of the item
/// renderer takes part in it.  What this pass still owes the tree is the
/// control geometry, because that is what the UI thread hit-tests the pointer
/// against -- and it asks for the geometry alone, without encoding a frame that
/// nobody would read.
#[cfg(feature = "renderer-femtovg")]
#[allow(clippy::too_many_arguments)]
fn present_render_owned(
    state: &mut GlRenderState,
    render_window_adapter: &Option<Rc<dyn WindowAdapter>>,
    render_controls: &mut HashMap<u64, ControlRegion>,
    render_control_order: &mut Vec<u64>,
    render_item_rcs: &mut HashMap<u64, ItemRc>,
    interaction: &mut ControlInteraction,
) {
    if !publish_mirror_controls(
        render_window_adapter,
        render_controls,
        render_control_order,
        render_item_rcs,
        interaction,
    ) {
        return;
    }
    let Some(adapter) = render_window_adapter.as_ref() else {
        return;
    };
    if let Err(e) = state.ensure_upstream(adapter) {
        eprintln!("dualslint render thread: upstream renderer init failed: {e}");
        return;
    }
    if let Err(e) = state.render_upstream() {
        eprintln!("dualslint render thread: upstream render failed: {e}");
    }
}

/// The same present on a build with no upstream item renderer for the render
/// thread to drive.  A component is only ever attached where one exists, so
/// this has nothing to draw; it is here so that every build presents a message
/// the same way instead of each handler remembering the feature list.
#[cfg(not(feature = "renderer-femtovg"))]
#[allow(clippy::too_many_arguments)]
fn present_render_owned(
    _state: &mut GlRenderState,
    _render_window_adapter: &Option<Rc<dyn WindowAdapter>>,
    _render_controls: &mut HashMap<u64, ControlRegion>,
    _render_control_order: &mut Vec<u64>,
    _render_item_rcs: &mut HashMap<u64, ItemRc>,
    _interaction: &mut ControlInteraction,
) {
}

/// A published control's property, as a reference to the property itself.
///
/// Resolving a name to one of these is what both halves of the control protocol
/// do, so a name that can be written is necessarily a name that can be read:
/// the two lists cannot drift apart, because there is only one list.
enum ControlPropertyRef<'a> {
    Bool(&'a i_slint_core::properties::Property<bool>),
    Text(&'a i_slint_core::properties::Property<i_slint_core::SharedString>),
    Color(&'a i_slint_core::properties::Property<i_slint_core::graphics::Brush>),
    Number(&'a i_slint_core::properties::Property<i_slint_core::lengths::LogicalLength>),
}

impl ControlPropertyRef<'_> {
    /// The value the property currently holds.
    ///
    /// A color property holding a gradient has no single value to report:
    /// reporting one of its stops would hand the caller a color the control is
    /// not actually filled with, so it reports nothing instead.
    fn get(&self) -> Option<ControlPropertyValue> {
        use i_slint_core::graphics::Brush;
        Some(match self {
            ControlPropertyRef::Bool(p) => ControlPropertyValue::Bool(read_property(p)),
            ControlPropertyRef::Text(p) => ControlPropertyValue::Text(read_property(p).to_string()),
            ControlPropertyRef::Color(p) => {
                let Brush::SolidColor(color) = read_property(p) else { return None };
                ControlPropertyValue::Color {
                    r: color.red(),
                    g: color.green(),
                    b: color.blue(),
                    a: color.alpha(),
                }
            }
            ControlPropertyRef::Number(p) => ControlPropertyValue::Number(read_property(p).0),
        })
    }

    /// Assign the value, using the upstream `Property::set` semantics: a
    /// previous binding is detached.  The compiler's constant flag is cleared
    /// first, so a property whose binding compiled to a literal stays writable.
    ///
    /// Returns `false` when the value is not of the property's own kind, so a
    /// mismatched assignment is refused rather than turned into something the
    /// caller did not ask for.
    fn set(&self, value: &ControlPropertyValue) -> bool {
        match (self, value) {
            (ControlPropertyRef::Bool(p), ControlPropertyValue::Bool(v)) => {
                write_property(p, *v);
                true
            }
            (ControlPropertyRef::Text(p), ControlPropertyValue::Text(v)) => {
                write_property(p, i_slint_core::SharedString::from(v.as_str()));
                true
            }
            (ControlPropertyRef::Color(p), ControlPropertyValue::Color { r, g, b, a }) => {
                write_property(
                    p,
                    i_slint_core::graphics::Color::from_argb_u8(*a, *r, *g, *b).into(),
                );
                true
            }
            (ControlPropertyRef::Number(p), ControlPropertyValue::Number(v)) => {
                write_property(p, i_slint_core::lengths::LogicalLength::new(*v));
                true
            }
            _ => false,
        }
    }
}

/// Read a property. `Property::get` takes a `Pin<&Self>` and `Property` is
/// `!Unpin`, so the pin has to be reconstructed.
fn read_property<T: Clone>(property: &i_slint_core::properties::Property<T>) -> T {
    // SAFETY: every caller reaches the property through `Pin::get_ref` on a
    // `#[pin]` item, so the reference carries the pinned lifetime and the
    // property is structurally pinned for it. A `Property` never moves out of
    // the item it belongs to, which is what the pin is there to guarantee.
    unsafe { std::pin::Pin::new_unchecked(property) }.get()
}

/// Assign a property, detaching whatever binding was on it first.
fn write_property<T: Clone + PartialEq>(
    property: &i_slint_core::properties::Property<T>,
    value: T,
) {
    property.release_constant();
    property.set(value);
}

/// Hand the property a control names to `f`, if the control has one by that name.
///
/// The table is the published controls, which is the same set
/// `controls::ControlEncoder` walks: a control is
/// something the user can act on, and a name that is not in here is not
/// something the control protocol answers to.  Which item an id belongs to was
/// settled when the id was published, so this only has to pick the property out
/// of the item the id already names.
///
/// The property is passed to a closure rather than returned, because the pinned
/// item it points into lives only for the length of this call.
fn with_control_property<T>(
    item_rc: &ItemRc,
    property: &str,
    f: impl FnOnce(&ControlPropertyRef<'_>) -> T,
) -> Option<T> {
    use i_slint_core::items as it;
    if let Some(item) = item_rc.downcast::<it::TouchArea>() {
        let item = item.as_pin_ref();
        let item = item.get_ref();
        return Some(match property {
            "enabled" => f(&ControlPropertyRef::Bool(&item.enabled)),
            "pressed" => f(&ControlPropertyRef::Bool(&item.pressed)),
            // The item spells it with an underscore; callers use the `.slint`
            // name, so translate rather than leaking the field name.
            "has-hover" => f(&ControlPropertyRef::Bool(&item.has_hover)),
            _ => return None,
        });
    }
    if let Some(item) = item_rc.downcast::<it::FocusScope>() {
        let item = item.as_pin_ref();
        let item = item.get_ref();
        return Some(match property {
            "enabled" => f(&ControlPropertyRef::Bool(&item.enabled)),
            "focus-on-click" => f(&ControlPropertyRef::Bool(&item.focus_on_click)),
            "focus-on-tab-navigation" => {
                f(&ControlPropertyRef::Bool(&item.focus_on_tab_navigation))
            }
            _ => return None,
        });
    }
    if let Some(item) = item_rc.downcast::<it::TextInput>() {
        let item = item.as_pin_ref();
        let item = item.get_ref();
        return Some(match property {
            "text" => f(&ControlPropertyRef::Text(&item.text)),
            "font-family" => f(&ControlPropertyRef::Text(&item.font_family)),
            "color" => f(&ControlPropertyRef::Color(&item.color)),
            "font-size" => f(&ControlPropertyRef::Number(&item.font_size)),
            _ => return None,
        });
    }
    None
}

/// How the application answers for the properties of the component the render
/// thread is drawing.
///
/// The render thread resolves a property name against the built-in items first
/// (`enabled` on a `TouchArea`, `text` on a `TextInput`) and asks here for what
/// is left. Everything a `.slint` file declares around its items — a
/// `CheckBox`'s `checked` — is that.
///
/// The component is handed back as the `Any` that `attach_component` was given,
/// because that is the only thing both sides agree on: the application is what
/// created it, and the render thread only forwards it. Both calls run on the
/// render thread, and the application is expected to answer from the component
/// it is holding there rather than reaching across threads for it.
pub struct ComponentPropertyAccess {
    read: Box<dyn Fn(&dyn std::any::Any, &ItemRc, &str) -> Option<ControlPropertyValue> + Send>,
    write: Box<dyn Fn(&dyn std::any::Any, &ItemRc, &str, &ControlPropertyValue) -> bool + Send>,
}

impl ComponentPropertyAccess {
    /// Build the pair of calls from a single function, for the common case
    /// where reading and writing go through the same table.
    pub fn new(
        read: impl Fn(&dyn std::any::Any, &ItemRc, &str) -> Option<ControlPropertyValue>
        + Send
        + 'static,
        write: impl Fn(&dyn std::any::Any, &ItemRc, &str, &ControlPropertyValue) -> bool
        + Send
        + 'static,
    ) -> Self {
        Self { read: Box::new(read), write: Box::new(write) }
    }

    /// The C pair of calls, wrapped so that a caller holding them can lend the
    /// render thread the same answers [`Self::new`] would.
    ///
    /// This is the shape
    /// [`slint_render_thread_attach_component_with_property_access`] takes, and
    /// it exists for an embedder whose callbacks came from C: a C++ shim, or
    /// another library loaded at run time. A Rust application calls
    /// [`Self::new`] with its closures instead.
    ///
    /// `component` in either call is the address of the `Any` the factory
    /// handed over and `item` the address of an item in the tree being drawn,
    /// both of which outlive the call, so a callee written against the pointer
    /// contract sees the values it would have seen had the application written
    /// it in Rust.
    ///
    /// # Safety
    ///
    /// The two function pointers must be safe to call from the render thread
    /// with the arguments the contract on [`SlintRenderThreadPropertyRead`] and
    /// [`SlintRenderThreadPropertyWrite`] describes.
    pub unsafe fn from_c(
        CSlintRenderThreadPropertyAccess { read, write }: CSlintRenderThreadPropertyAccess,
    ) -> Self {
        let read = read.map(|read| {
            move |component: &dyn std::any::Any, item: &ItemRc, property: &str| {
                let mut out = CSlintControlPropertyValue::from_value(
                    &ControlPropertyValue::Bool(false),
                    std::ptr::null(),
                );
                // A `&str` carries no terminator, and a callee reads it as a C
                // string, so the name is copied into one that has a NUL at the
                // end. It is only ever a property name, so it cannot contain one
                // of its own.
                let name = std::ffi::CString::new(property).unwrap_or_default();
                // SAFETY: the pointers are the render thread's, which outlive
                // this call, and a callee that answers `true` promised the tag
                // names a kind it filled in.
                let answered = unsafe {
                    read(
                        component as *const _ as *const std::os::raw::c_void,
                        item as *const _ as *const std::os::raw::c_void,
                        name.as_ptr(),
                        &mut out,
                    )
                };
                // SAFETY: as above; a text tag also promised a NUL-terminated
                // string that outlives the call that returned -- a buffer the
                // callee dropped on its way out is gone, which is why the
                // promise asks for one that lives on -- and it is copied here
                // before anything else can reuse it.
                answered.then(|| unsafe { out.to_value() }).flatten()
            }
        });
        let write = write.map(|write| {
            move |component: &dyn std::any::Any,
                  item: &ItemRc,
                  property: &str,
                  value: &ControlPropertyValue| {
                // The string is read during the call and never kept, so a local
                // buffer is enough: a `CString` has no address to hand out that
                // stays put once it is moved.
                let name = std::ffi::CString::new(property).unwrap_or_default();
                // The same reasoning as in the read, and the text is only read
                // during the call, so a local buffer is enough: a `CString` has
                // no address to hand out that stays put once it is moved.
                let text = match value {
                    ControlPropertyValue::Text(text) => std::ffi::CString::new(text.as_str()).ok(),
                    _ => None,
                };
                let c_value = CSlintControlPropertyValue::from_value(
                    value,
                    text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
                );
                // SAFETY: as in the read: the pointers are the render thread's,
                // and `name` and `text` are alive across the call.
                unsafe {
                    write(
                        component as *const _ as *const std::os::raw::c_void,
                        item as *const _ as *const std::os::raw::c_void,
                        name.as_ptr(),
                        &c_value,
                    )
                }
            }
        });
        Self::new(
            move |component, item, property| read.and_then(|read| read(component, item, property)),
            move |component, item, property, value| {
                write.is_some_and(|write| write(component, item, property, value))
            },
        )
    }

    /// Read `property` of the component `item` belongs to, or `None` when the
    /// component has no such property.
    ///
    /// `None` is the answer the render thread falls back on, so an application
    /// that simply has no such property returns it rather than failing.
    pub fn read(
        &self,
        component: &dyn std::any::Any,
        item: &ItemRc,
        property: &str,
    ) -> Option<ControlPropertyValue> {
        (self.read)(component, item, property)
    }

    /// Assign `property` of the component `item` belongs to. Returns `false`
    /// when the component has no such property, or the value does not fit it.
    pub fn write(
        &self,
        component: &dyn std::any::Any,
        item: &ItemRc,
        property: &str,
        value: &ControlPropertyValue,
    ) -> bool {
        (self.write)(component, item, property, value)
    }
}

impl core::fmt::Debug for ComponentPropertyAccess {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ComponentPropertyAccess")
    }
}

/// Apply a dynamic, property-name based control property loan onto the
/// render-side mirror item.  Returns `false` if the item, the property, or the
/// value's kind is not known.
fn apply_control_property(
    component: Option<&dyn std::any::Any>,
    access: Option<&ComponentPropertyAccess>,
    item_rc: &ItemRc,
    property: &str,
    value: &ControlPropertyValue,
) -> bool {
    // The component is asked first, and the order matters: a widget's property
    // belongs to the application that declared it, and writing the property of
    // a bare item that happens to share the name can detach a binding the
    // application still relies on. A `false` from the component is it declining
    // the name rather than refusing the write, and the item table then answers
    // for what the application does not describe -- which is most of a standard
    // widget, since `remove_aliases` folds those properties into the items
    // underneath them.
    match (component, access) {
        (Some(component), Some(access)) if access.write(component, item_rc, property, value) => {
            true
        }
        _ => with_control_property(item_rc, property, |property| property.set(value))
            .unwrap_or(false),
    }
}

/// Read a control's property by name.
///
/// This is the other half of [`apply_control_property`], and it exists because
/// the `.slint` side of the tree belongs to the render thread: a Ui thread that
/// needs to know what a control looks like asks here, rather than keeping a
/// second copy of the component to answer the question itself.
fn read_control_property(
    component: Option<&dyn std::any::Any>,
    access: Option<&ComponentPropertyAccess>,
    item_rc: &ItemRc,
    property: &str,
) -> Option<ControlPropertyValue> {
    // Asked in the same order as the write, and for the same reason: a widget's
    // property is the application's, and reading the bare item underneath it
    // answers about a different value. `None` from the component is it declining
    // the name, and the item table answers for what the application leaves out.
    match (component, access) {
        (Some(component), Some(access)) => {
            access.read(component, item_rc, property).or_else(|| {
                with_control_property(item_rc, property, |property| property.get()).flatten()
            })
        }
        _ => with_control_property(item_rc, property, |property| property.get()).flatten(),
    }
}

/// Deliver a click to the tree, the way a pointer would have.
///
/// The UI thread has already said which control the press and the release both
/// landed on. Turning that into the pair of events the tree expects is the
/// render thread's business, and doing it here rather than on the UI thread is
/// what keeps the application's own code in charge: the `TouchArea` fires
/// `clicked`, the `CheckBox` behind it flips `checked` and fires `toggled`, a
/// `FocusScope` with `focus-on-click` takes the focus, and a `Button` looks
/// pressed in between -- all of it decided by the bindings and callbacks the
/// `.slint` file already declares, none of it by a list of widget names in a
/// backend.
///
/// The position is the control's own centre, taken from its geometry rather
/// than from the pointer, so the event describes the control the UI thread named
/// rather than a coordinate the two threads would have to agree on. Both events
/// go through the window, which is also how the item stack is built: the tree
/// finds for itself which items the point is inside, exactly as it would for a
/// real press.
fn activate_control(adapter: &Rc<dyn WindowAdapter>, item_rc: &ItemRc) -> bool {
    let size = item_rc.geometry().size;
    let position = item_rc.map_to_window(LogicalPoint::default()) + size.to_vector() * 0.5;

    for kind in [MouseEventKind::Pressed, MouseEventKind::Released] {
        let event = i_slint_core::platform::InternalEvent::Mouse(match kind {
            MouseEventKind::Pressed => BackendMouseEvent::Pressed {
                position,
                button: PointerEventButton::Left,
                click_count: 1,
                touch_finger_id: 0,
            },
            MouseEventKind::Released => BackendMouseEvent::Released {
                position,
                button: PointerEventButton::Left,
                click_count: 1,
                touch_finger_id: 0,
            },
        });
        adapter.window().dispatch_event(WindowEvent::internal(event));
    }
    // The tree is asked whether it accepted the release; the press is only ever
    // a grab, so the release is where "the control took the click" is decided.
    adapter
        .window()
        .dispatch_event_with_result(WindowEvent::internal(
            i_slint_core::platform::InternalEvent::Mouse(BackendMouseEvent::Exit),
        ))
        .is_ok()
}

/// Which half of the click pair is being built.
enum MouseEventKind {
    Pressed,
    Released,
}

/// Hand a key to the tree that has the focus.
///
/// This is the whole of the render thread's part in typing. What the key means
/// -- an insertion at the cursor, a deletion of a word, a shortcut, an
/// `accepted` handler -- is answered by the item that has the focus, through the
/// same code that answers it when the window has the pointer, so a `TextInput`
/// keeps its cursor, its selection and its IME, and the application's
/// `accepted(text) =>` runs with the value the user actually typed.
fn send_key_to_tree(adapter: &Rc<dyn WindowAdapter>, event: &InternalKeyEvent) -> bool {
    matches!(
        adapter.window().dispatch_event_with_result(WindowEvent::internal(
            i_slint_core::platform::InternalEvent::Key(event.clone()),
        )),
        Ok(i_slint_core::platform::WindowEventDispatchResult::Accepted)
    )
}

/// Create the render-thread channel pair.
pub(crate) fn channel(
    event_loop_proxy: winit::event_loop::EventLoopProxy<crate::SlintEvent>,
) -> (RenderHost, RenderCore) {
    let (tx, rx) = mpsc::channel();
    let host = RenderHost {
        sender: tx,
        event_loop_proxy: Some(event_loop_proxy),
        attached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let core = RenderCore::new(rx, host.clone());
    (host, core)
}

// ---------------------------------------------------------------------------
// Accessor helpers
// ---------------------------------------------------------------------------

/// Obtain the [`RenderHost`] for the current process.
pub fn host() -> Option<RenderHost> {
    GLOBAL_RENDER_HOST.get().cloned()
}

/// Hand `window`'s drawing to the render thread, unless the application already
/// attached a component.
///
/// Called where the UI thread is about to draw, because that is the last moment
/// at which this window's component can still hand the render thread something.
/// A window whose component left no factory behind, such as one a third-party
/// [`ComponentFactory`] builds itself, is drawn here as before, and
/// [`report_window_drawn_by_ui`] says so.
pub(crate) fn take_over_drawing_of(window: &SlintApiWindow) {
    let Some(factory) = WindowInner::from_pub(window).render_factory() else {
        report_window_drawn_by_ui(
            window,
            "its component left no render factory behind, which is what a \
             ComponentFactory that builds the tree itself, or a component that is \
             not the window's root, does",
        );
        return;
    };
    let Some(host) = host() else {
        report_window_drawn_by_ui(window, "no render thread is running");
        return;
    };
    if !cfg!(feature = "renderer-femtovg") {
        // Attaching without a renderer on this thread would claim a
        // render-owned tree that nothing draws.  Leaving the window on the UI
        // thread is honest; the report is what keeps it from being silent.
        report_window_drawn_by_ui(
            window,
            "this build has no renderer on the render thread, because the \
             `renderer-femtovg` feature is off",
        );
        return;
    }
    if host.has_attached_component() {
        if render_thread_owns(window) {
            // The handover is still in flight for this very window: the attach
            // has been sent, and the graphics context comes across a frame or
            // two later.  Until then this thread still presents, which is the one
            // frame it is allowed to draw on the way across.
            return;
        }
        report_window_drawn_by_ui(
            window,
            "the render thread is already drawing another window, and it draws \
             one window at a time",
        );
        return;
    }
    host.attach_component_with_to(window, move || factory(), None);
}

/// The windows this process has already reported as drawn by the UI thread.
///
/// [`std::sync::Mutex::new`] is const, so this needs no lazy initialisation. The
/// `Option` keeps the set empty until the first report.
static UI_DRAWN_REPORTED: std::sync::Mutex<Option<std::collections::HashSet<usize>>> =
    std::sync::Mutex::new(None);

/// Say, once per window, that the UI thread is the one drawing it.
///
/// This fork has the render thread own every control and every pixel, so a
/// window the UI thread draws is a configuration the build cannot honour rather
/// than a fallback to shrug at.  An app that looks render-owned and is not is
/// worse than one that never claimed to be, which is the whole reason this
/// exists.  Remembering the windows keeps a draw loop that runs 60 times a
/// second from repeating the message on every frame.
fn report_window_drawn_by_ui(window: &SlintApiWindow, reason: &str) {
    if ON_RENDER_THREAD.with(|on| on.get()) {
        // The render thread is where a window is supposed to be drawn, and the
        // mirror's own draws come through here too.
        return;
    }
    {
        let mut reported = UI_DRAWN_REPORTED.lock().unwrap_or_else(|e| e.into_inner());
        let reported = reported.get_or_insert_with(Default::default);
        if !reported.insert(window_identity(window)) {
            return;
        }
    }
    eprintln!(
        "dualslint: the UI thread is drawing this window because {reason}. Only the \
         render thread may draw, so this window's controls are not reachable through \
         the render thread's control API."
    );
}

/// Whether the render thread has been told to draw into `window`.
fn render_thread_owns(window: &SlintApiWindow) -> bool {
    let window_adapter = WindowInner::from_pub(window).window_adapter();
    // `Weak::ptr_eq`, not `==` on the raw pointers: a fat pointer carries a
    // vtable, and two trait objects for the same adapter do not have to carry
    // the same one, so comparing them whole answers a question nobody asked.
    let window_adapter = std::rc::Rc::downgrade(&window_adapter);
    SURFACE_OWNER
        .with(|owner| owner.borrow().as_ref().is_some_and(|known| known.ptr_eq(&window_adapter)))
}

/// A number that stands for `window` while it lives.
///
/// The adapter is the one thing every window of this backend has and the render
/// thread keeps hold of, so its address identifies the window well enough for a
/// report that must not repeat itself. An address that a later window gets
/// handed once the old one is gone can only make that window report again, never
/// silence a report that was due.
fn window_identity(window: &SlintApiWindow) -> usize {
    let window_adapter = WindowInner::from_pub(window).window_adapter();
    std::rc::Rc::as_ptr(&window_adapter) as *const () as usize
}

/// Request a window repaint through the render thread.
///
/// This is the replacement for the removed [`Window::request_redraw`] entry
/// point, and it reaches whichever thread draws the window: a window the render
/// thread owns is re-encoded and re-presented there, and a window the UI thread
/// still draws is repainted by the UI thread.  It is safe to call from any
/// thread.
///
/// This is a no-op when the render thread was never started.
pub fn request_redraw() {
    if let Some(host) = GLOBAL_RENDER_HOST.get() {
        host.request_redraw();
    }
}

/// C FFI entry point used by the C++ bindings to request a repaint through
/// the render thread, mirroring [`request_redraw`].
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_request_redraw() {
    request_redraw();
}

// ---------------------------------------------------------------------------
// C ABI for borrowing the render-owned controls
//
// The languages that have no handle type of their own (C++, and anything built
// on the C ABI) reach the same borrow protocol the Rust API offers: identify a
// control, then assign one of its properties.  The property value crosses as
// [`CSlintControlPropertyValue`] rather than a Rust enum so the layout is
// fixed; it is defined next to the enum it mirrors, in the scene crate.
// ---------------------------------------------------------------------------

/// Borrow the control under the logical point `(x, y)`.
///
/// Returns the control id, or `0` when the point is over no control or nothing
/// has been composited yet.  Control ids are never `0`, so `0` is unambiguous
/// as "no control".  This reads the published geometry and does not wait for the
/// render thread, so it is usable from a pointer move.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_control_at(x: f32, y: f32) -> u64 {
    host().and_then(|host| host.control_at(x, y)).unwrap_or(0)
}

/// Like [`slint_render_thread_control_at`], but waits for the render thread to
/// answer from the control tree as it stands now rather than from the last
/// composited frame.  Use this when the answer has to be current, such as
/// resolving a click.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_hit_test(x: f32, y: f32) -> u64 {
    host().and_then(|host| host.hit_test(x, y)).unwrap_or(0)
}

/// Assign one property of a borrowed control, blocking until the render thread
/// confirms it.  Returns whether the property name resolved and the value was
/// applied.
///
/// # Safety
///
/// `property` must be null or point to a NUL-terminated string, and `value.text`
/// must be null or point to a NUL-terminated string, or the call is undefined
/// behaviour.  The ABI is unchanged for C callers; this only records a contract
/// that a C caller cannot be checked against.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slint_render_thread_set_control_property(
    id: u64,
    property: *const std::os::raw::c_char,
    value: CSlintControlPropertyValue,
) -> bool {
    if id == 0 || property.is_null() {
        return false;
    }
    // SAFETY: the caller of this unsafe function promised that `property` is
    // null or a NUL-terminated string, which is checked just above.
    let Some(property) = (unsafe { std::ffi::CStr::from_ptr(property) }).to_str().ok() else {
        return false;
    };
    // SAFETY: the caller of this unsafe function promised that `value.text` is
    // null or a NUL-terminated string.
    let Some(value) = (unsafe { value.to_value() }) else { return false };
    host().is_some_and(|host| host.set_control_property(id, property, value))
}

thread_local! {
    /// Backing store for the string in a value returned by
    /// [`slint_render_thread_get_control_property`].
    ///
    /// A `CString` cannot be moved to a stable address once it is handed out,
    /// so the read keeps its string here and the C caller borrows it until its
    /// next read on the same thread.  One buffer per thread is enough: the
    /// caller has the value in hand before it asks again.
    static LAST_PROPERTY_TEXT: std::cell::RefCell<Option<std::ffi::CString>> =
        const { std::cell::RefCell::new(None) };
}

/// Read one of a borrowed control's properties, blocking until the render
/// thread answers.  Writes the value into `out` and returns whether the id and
/// the property name resolved.
///
/// # Safety
///
/// `property` must be null or point to a NUL-terminated string, and `out` must
/// be null or point to a writable [`CSlintControlPropertyValue`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slint_render_thread_get_control_property(
    id: u64,
    property: *const std::os::raw::c_char,
    out: *mut CSlintControlPropertyValue,
) -> bool {
    if id == 0 || property.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: the caller of this unsafe function promised that `property` is
    // null or a NUL-terminated string, which is checked just above.
    let Some(property) = (unsafe { std::ffi::CStr::from_ptr(property) }).to_str().ok() else {
        return false;
    };
    let Some(value) = host().and_then(|host| host.get_control_property(id, property)) else {
        return false;
    };

    // A string has to outlive the call that reports it, so it goes into the
    // thread-local buffer rather than onto the stack.  A value without a
    // string does not touch the buffer, which leaves any previous borrow
    // intact.
    let text = match &value {
        ControlPropertyValue::Text(text) => {
            let text = std::ffi::CString::new(text.as_str()).unwrap_or_default();
            let ptr = text.as_ptr();
            LAST_PROPERTY_TEXT.with(|slot| *slot.borrow_mut() = Some(text));
            ptr
        }
        _ => std::ptr::null(),
    };
    // SAFETY: the caller promised `out` points to a writable value, checked
    // just above, and nothing above writes through it before this line.
    unsafe { out.write(CSlintControlPropertyValue::from_value(&value, text)) };
    true
}

/// Report a control as hovered and/or pressed on the render thread.
///
/// This is the pointer state the UI thread resolved for itself; a worker that
/// drives a control from its own logic (a gamepad cursor, a scripted
/// highlight) uses it the same way, and neither side is more privileged.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_apply_control_state(id: u64, hovered: bool, pressed: bool) {
    if id == 0 {
        return;
    }
    if let Some(host) = host() {
        host.apply_control_state(id, hovered, pressed);
    }
}

/// Hand the application a component to be instantiated on the render thread,
/// making the render thread the owner of the controls.
///
/// `factory` runs on the render thread and must create the component there.  It
/// returns the address of a heap-allocated `Box<dyn Any>` holding the component
/// -- that is, the result of `Box::into_raw` on that box, not the address of the
/// component itself, because a trait object is a fat pointer and cannot travel
/// through a C ABI.  Ownership of the box passes to the render thread, which
/// keeps the component alive for as long as it draws.  A null `factory` is
/// refused, because the render thread cannot ask for the component later; a
/// factory that returns null has broken its promise and ends the render thread
/// with a message saying so, rather than leaving it to fail later on a
/// component that is not there.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_attach_component(
    factory: Option<extern "C" fn() -> *mut c_void>,
) -> bool {
    let Some(factory) = factory else { return false };
    let Some(host) = host() else { return false };
    host.attach_component(move || {
        // The pointer comes from `factory`, which the caller promised to
        // produce on the calling thread and to hand over.  The render thread is
        // the calling thread, and it takes over the box here.
        let leaked = factory();
        assert!(
            !leaked.is_null(),
            "the component factory passed to slint_render_thread_attach_component returned null, \
             so there is no component for the render thread to own"
        );
        // The pointer is the address of a `Box<dyn Any>` that the caller leaked
        // and handed over, so it is ours to free.
        unsafe { *Box::from_raw(leaked as *mut Box<dyn std::any::Any>) }
    });
    true
}

/// [`slint_render_thread_attach_component`], with a way for the application to
/// answer for the properties that belong to the component rather than to any
/// item in it.
///
/// A widget from a `.slint` file is a group of items plus the bindings between
/// them, so what a caller usually wants to know -- whether a `CheckBox` is
/// `checked` -- is a property of that group and of no item in it. The render
/// thread is a windowing backend and does not know what a `.slint` component
/// is, so `access` is how the application, which does, lends it that knowledge
/// for the component it just handed over.
///
/// Both calls run on the render thread, which expects them to answer from the
/// component the factory created there rather than reaching across threads for
/// it. A null `read` leaves every query to the render thread's own items, and a
/// null `write` refuses every assignment; both are how an application says
/// "nothing of mine answers this".
///
/// # Safety
///
/// The two function pointers must be safe to call from the render thread with
/// the arguments the contract on [`SlintRenderThreadPropertyRead`] and
/// [`SlintRenderThreadPropertyWrite`] describes, and `access` must stay valid
/// for the call, which copies it out before returning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slint_render_thread_attach_component_with_property_access(
    factory: Option<extern "C" fn() -> *mut c_void>,
    access: CSlintRenderThreadPropertyAccess,
) -> bool {
    let Some(factory) = factory else { return false };
    let Some(host) = host() else { return false };
    host.attach_component_with(
        move || {
            // SAFETY: as in `slint_render_thread_attach_component`, the pointer
            // comes from `factory`, which promised to produce it on the render
            // thread and to hand it over.
            let leaked = factory();
            assert!(
                !leaked.is_null(),
                "the component factory passed to slint_render_thread_attach_component returned null, \
                 so there is no component for the render thread to own"
            );
            // SAFETY: the pointer is the address of a `Box<dyn Any>` the caller
            // leaked and handed over, so it is ours to free.
            unsafe { *Box::from_raw(leaked as *mut Box<dyn std::any::Any>) }
        },
        Some(unsafe { ComponentPropertyAccess::from_c(access) }),
    );
    true
}

/// Forward the host's system accent colour to the render thread's mirror
/// context.  Called whenever the winit backend resolves the accent from the
/// OS (xdg-desktop-settings watcher or the winit window adapter); a no-op
/// when the render thread has not been started.
pub fn forward_system_accent(color: Color) {
    if let Some(host) = GLOBAL_RENDER_HOST.get() {
        host.submit_accent(color);
    }
}

/// Access the shared coordinate table.  Returns `None` until the winit
/// backend has been configured (`ensure_render_thread`).  UI thread and
/// workers call this during event processing to hit-test the pointer against
/// the latest geometry that the render thread actually composited.
pub fn coordinate_map() -> Option<Arc<Mutex<PublishedControls>>> {
    GLOBAL_COORDINATE_MAP.get().cloned()
}

/// Replace the shared coordinate table with the given control regions.
/// Called on the render thread when a scene is composited, so the published
/// geometry always reflects what was actually drawn.
/// The pointer state the render thread holds for the render-owned controls.
///
/// The UI thread decides what this should be (it owns the OS event and does the
/// hit-testing) and the render thread applies it, so the state lives here as the
/// applied truth and is republished for the peers to read.
#[derive(Default)]
struct ControlInteraction {
    hovered: HashSet<u64>,
    pressed: HashSet<u64>,
}

impl ControlInteraction {
    fn is_hovered(&self, id: u64) -> bool {
        self.hovered.contains(&id)
    }

    fn is_pressed(&self, id: u64) -> bool {
        self.pressed.contains(&id)
    }

    fn set_hovered(&mut self, id: u64, hovered: bool) {
        if hovered {
            self.hovered.insert(id);
        } else {
            self.hovered.remove(&id);
        }
    }

    fn set_pressed(&mut self, id: u64, pressed: bool) {
        if pressed {
            self.pressed.insert(id);
        } else {
            self.pressed.remove(&id);
        }
    }

    fn clear(&mut self) {
        self.hovered.clear();
        self.pressed.clear();
    }

    /// Forget the controls that the latest encode no longer contains, so that a
    /// control that comes back does not inherit a stale hover or press.
    #[cfg(feature = "renderer-femtovg")]
    fn retain(&mut self, controls: &[ControlRegion]) {
        self.hovered.retain(|id| controls.iter().any(|c| c.id == *id));
        self.pressed.retain(|id| controls.iter().any(|c| c.id == *id));
    }
}

/// Publish the geometry the UI thread hit-tests against, in paint order.
///
/// The table is geometry and pointer state, and nothing else: it is what the UI
/// thread needs to know *where* a control is, so that it can say which one the
/// pointer landed on without knowing anything about how a click is answered.
#[cfg(feature = "renderer-femtovg")]
fn publish_control_coords(controls: &[ControlRegion], interaction: &ControlInteraction) {
    let Some(map) = coordinate_map() else { return };
    let mut map = map.lock().unwrap();
    map.clear();
    for c in controls {
        map.insert(
            c.id,
            ControlCoord {
                x: c.geometry.origin.x,
                y: c.geometry.origin.y,
                width: c.geometry.size.width,
                height: c.geometry.size.height,
                hovered: interaction.is_hovered(c.id),
                pressed: interaction.is_pressed(c.id),
            },
        );
    }
}

/// Returns the raw HWND (Windows only).
#[cfg(target_os = "windows")]
pub fn hwnd() -> Option<isize> {
    GLOBAL_HWND.get().copied()
}

/// There is no native handle to return outside Windows.
#[cfg(not(target_os = "windows"))]
pub fn hwnd() -> Option<isize> {
    None
}

/// Store the HWND.  Called internally by the window adapter.
#[cfg(target_os = "windows")]
pub(crate) fn set_hwnd(hwnd: isize) {
    let _ = GLOBAL_HWND.set(hwnd);
}

// ===========================================================================
// GL Render State — owned entirely by the render thread
// ===========================================================================

/// The GL context and surface the render thread draws into.  Upstream's
/// renderer and this module's own replay path share this one context: a second
/// context on the same native window is rejected by GLX/EGL, and a single
/// context is all the render thread needs.
struct GlObjects {
    context: glutin::context::PossiblyCurrentContext,
    surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
}

/// A `'static` handle on [`GlObjects`] for `set_opengl_context`, which takes
/// ownership of the interface it is handed.  The clone shares the context
/// instead of building a new one, so this module and upstream's renderer draw
/// through the same GL state.
#[cfg(feature = "renderer-femtovg")]
#[derive(Clone)]
struct GlView(Rc<GlObjects>);

#[cfg(feature = "renderer-femtovg")]
impl GlView {
    fn new(gl: &Rc<GlObjects>) -> Self {
        Self(gl.clone())
    }
}

#[cfg(feature = "renderer-femtovg")]
unsafe impl i_slint_renderer_femtovg::opengl::OpenGLInterface for GlView {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use glutin::prelude::*;
        if !self.0.context.is_current() {
            self.0.context.make_current(&self.0.surface).map_err(
                |glutin_error| -> PlatformError {
                    format!("FemtoVG: Error making context current: {glutin_error}").into()
                },
            )?;
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use glutin::prelude::*;
        self.0.surface.swap_buffers(&self.0.context).map_err(|glutin_error| -> PlatformError {
            format!("FemtoVG: Error swapping buffers: {glutin_error}").into()
        })?;
        Ok(())
    }

    fn resize(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.ensure_current()?;
        use glutin::prelude::*;
        self.0.surface.resize(&self.0.context, width, height);
        Ok(())
    }

    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const std::ffi::c_void {
        use glutin::prelude::*;
        self.0.context.display().get_proc_address(name)
    }
}

/// Upstream's femtovg renderer, bound to the render thread's shared GL context
/// and to the render-owned window adapter.  This is the whole point of the
/// split: the render thread draws with upstream's item renderer, not with a
/// private re-implementation of it.
#[cfg(feature = "renderer-femtovg")]
struct UpstreamRenderer {
    renderer:
        i_slint_renderer_femtovg::FemtoVGRenderer<i_slint_renderer_femtovg::opengl::OpenGLBackend>,
}

#[cfg(feature = "renderer-femtovg")]
impl UpstreamRenderer {
    fn new(gl: &Rc<GlObjects>, window_adapter: &Rc<dyn WindowAdapter>) -> Result<Self, String> {
        use i_slint_core::renderer::RendererSealed;
        use i_slint_renderer_femtovg::{FemtoVGOpenGLRendererExt, FemtoVGRendererExt};

        let renderer = i_slint_renderer_femtovg::FemtoVGRenderer::new_suspended();
        renderer
            .set_opengl_context(GlView::new(gl))
            .map_err(|e| format!("set_opengl_context failed: {e}"))?;
        renderer.set_window_adapter(window_adapter);
        Ok(Self { renderer })
    }

    fn render(&self) -> Result<(), PlatformError> {
        self.renderer.render()
    }
}

struct GlRenderState {
    /// The one GL context and surface for this window, shared with upstream's
    /// renderer so both draw through the same GL state.
    gl: Rc<GlObjects>,
    width: u32,
    height: u32,
    /// Upstream's femtovg renderer, used to draw a render-owned component.
    /// Created on first use so the default UI-tree path never pays for a
    /// second GL context.
    #[cfg(feature = "renderer-femtovg")]
    upstream: Option<UpstreamRenderer>,
}

impl GlRenderState {
    fn new(window: Arc<winit::window::Window>, width: u32, height: u32) -> Result<Self, String> {
        use glutin::context::{ContextApi, ContextAttributesBuilder};
        use glutin::prelude::*;
        use glutin::surface::{GlSurface, SurfaceAttributesBuilder, WindowSurface};
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

        let raw_display =
            window.display_handle().map_err(|e| format!("Failed to get display handle: {e}"))?;
        let raw_window =
            window.window_handle().map_err(|e| format!("Failed to get window handle: {e}"))?;

        // Build GL display on the render thread using the window's handles.
        // The `DisplayApiPreference` variants are cfg-gated by glutin per
        // backend, so each platform must select its own.
        #[cfg(target_os = "windows")]
        let display_preference = glutin::display::DisplayApiPreference::EglThenWgl(Some(
            raw_window_handle::RawWindowHandle::from(raw_window.as_raw()),
        ));
        #[cfg(target_os = "macos")]
        let display_preference = glutin::display::DisplayApiPreference::Cgl;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let display_preference = glutin::display::DisplayApiPreference::Egl;

        let gl_display = unsafe {
            glutin::display::Display::new(raw_display.as_raw(), display_preference)
                .map_err(|e| format!("glutin Display::new failed: {e}"))?
        };

        // The window was created for the config that the UI thread's renderer
        // picked, and glutin gave that window that config's visual.  A window
        // surface has to use a config with the same visual, so this thread has
        // to pick the same one: taking whichever config comes first yields a
        // surface whose buffers the driver cannot present from -- the first
        // frame reaches the window and no later frame does.
        #[cfg(feature = "x11")]
        let window_visual: Option<u32> = match raw_window.as_raw() {
            raw_window_handle::RawWindowHandle::Xlib(handle) => Some(handle.visual_id as u32),
            raw_window_handle::RawWindowHandle::Xcb(handle) => {
                Some(handle.visual_id.map(|id| id.get()).unwrap_or(0))
            }
            _ => None,
        };
        #[cfg(not(feature = "x11"))]
        let window_visual: Option<u32> = None;
        let config_template = glutin::config::ConfigTemplateBuilder::new();
        let mut configs = unsafe {
            gl_display
                .find_configs(config_template.build())
                .map_err(|e| format!("glutin find_configs failed: {e}"))?
        };
        let config = match window_visual {
            #[cfg(feature = "x11")]
            Some(visual) => {
                use glutin::platform::x11::X11GlConfigExt as _;
                configs
                    .find(|config| {
                        config.x11_visual().is_some_and(|info| info.visual_id() as u32 == visual)
                    })
                    .ok_or_else(|| format!("No GL config for the window's visual {visual:#x}"))?
            }
            _ => configs.next().ok_or_else(|| "No suitable GL config found".to_string())?,
        };

        let raw_window_handle = raw_window.as_raw();

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(glutin::context::Version {
                major: 2,
                minor: 0,
            })))
            .build(Some(raw_window_handle));

        let not_current_ctx = unsafe {
            gl_display
                .create_context(&config, &context_attributes)
                .or_else(|_| {
                    let fallback = ContextAttributesBuilder::new().build(Some(raw_window_handle));
                    gl_display.create_context(&config, &fallback)
                })
                .map_err(|e| format!("glutin create_context failed: {e}"))?
        };

        let size: winit::dpi::PhysicalSize<u32> = window.surface_size();
        let non_zero_w = NonZeroU32::new(size.width.max(1)).ok_or("Window width is zero")?;
        let non_zero_h = NonZeroU32::new(size.height.max(1)).ok_or("Window height is zero")?;

        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            non_zero_w,
            non_zero_h,
        );

        let surface = unsafe {
            gl_display
                .create_window_surface(&config, &surface_attributes)
                .map_err(|e| format!("glutin create_window_surface failed: {e}"))?
        };

        let context = not_current_ctx
            .make_current(&surface)
            .map_err(|e| format!("make_current failed: {e}"))?;

        // Set vsync
        surface
            .set_swap_interval(
                &context,
                glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
            )
            .ok();

        Ok(Self {
            gl: Rc::new(GlObjects { context, surface }),
            width,
            height,
            #[cfg(feature = "renderer-femtovg")]
            upstream: None,
        })
    }

    /// Create the upstream renderer for the render-owned component, if it does
    /// not exist yet.
    #[cfg(feature = "renderer-femtovg")]
    fn ensure_upstream(
        &mut self,
        window_adapter: &Rc<dyn WindowAdapter>,
    ) -> Result<(), PlatformError> {
        if self.upstream.is_none() {
            self.upstream = Some(UpstreamRenderer::new(&self.gl, window_adapter)?);
        }
        Ok(())
    }

    /// Draw the render-owned component with upstream's own item renderer.
    #[cfg(feature = "renderer-femtovg")]
    fn render_upstream(&mut self) -> Result<(), PlatformError> {
        if let Some(upstream) = &self.upstream {
            upstream.render()?;
        }
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        use glutin::surface::GlSurface;
        self.width = width;
        self.height = height;
        if let Some((nz_w, nz_h)) = NonZeroU32::new(width).zip(NonZeroU32::new(height)) {
            self.gl.surface.resize(&self.gl.context, nz_w, nz_h);
        }
    }
}
