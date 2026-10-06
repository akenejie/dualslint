thread_local! {
    static THREAD_LOCAL_ACCESS: std::cell::RefCell<Option<ThreadLocalAccess>> = const { std::cell::RefCell::new(None) };
}

#[cfg(render_thread_can_draw)]
#[derive(Clone)]
struct ThreadLocalAccess {
    coords: Arc<Mutex<PublishedControls>>,
    item_map: Arc<Mutex<std::collections::HashMap<u64, i_slint_core::item_tree::ItemRc>>>,
}

#[cfg(not(render_thread_can_draw))]
struct ThreadLocalAccess {}

#[cfg(render_thread_can_draw)]
fn with_tl_access<R>(f: impl FnOnce(&ThreadLocalAccess) -> R) -> Option<R> {
    THREAD_LOCAL_ACCESS.with(|a| a.borrow().as_ref().map(f))
}

#[cfg(not(render_thread_can_draw))]
fn with_tl_access<R>(_f: impl FnOnce(&ThreadLocalAccess) -> R) -> Option<R> {
    None
}

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

#[cfg(render_thread_can_draw)]
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::rc::Rc;
#[cfg(render_thread_can_draw)]
use std::rc::Weak;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, mpsc};

#[cfg(render_thread_can_draw)]
use i_slint_core::api::PhysicalSize;
use i_slint_core::api::Window as SlintApiWindow;
use i_slint_core::graphics::Color;
use i_slint_core::input::{BackendMouseEvent, InternalKeyEvent, PointerEventButton};
use i_slint_core::item_tree::{ItemRc, ItemTreeRc};
use i_slint_core::lengths::LogicalPoint;
#[cfg(render_thread_can_draw)]
use i_slint_core::platform::{Clipboard, Platform, PlatformError};
use i_slint_core::platform::{WindowEvent, WindowEventDispatchResult};
use i_slint_core::window::{WindowAdapter, WindowInner};

// Brings the extension traits that give `PossiblyCurrentContext` and
// `Surface<WindowSurface>` their `make_current` / `swap_buffers` / `resize`
// methods into scope for the render-thread GL context, plus `GetGlDisplay` for
// `get_proc_address`.
#[cfg(feature = "renderer-femtovg")]
use glutin::display::GetGlDisplay;

#[cfg(feature = "renderer-femtovg")]
use crate::winit_compat::WindowSurfaceSizeExt;
use crate::winitwindowadapter::WinitWindowAdapter;

// The cross-thread control protocol types and the control geometry encoder live
// in the `i-slint-backend-scene` crate, so other 2-thread backends can reuse
// them.
pub use i_slint_backend_scene::*;

// ---------------------------------------------------------------------------
// Shared global state
// ---------------------------------------------------------------------------

/// The render threads of the process, one per window that a render thread draws.
///
/// A window is a surface, and a surface is composited by one thread and no
/// other: two render threads sharing a window would each need the other's
/// graphics context. So the render threads are per window, and this is where
/// the ones that exist are listed, because an API without a window argument --
/// `request_redraw`, the system accent colour -- is a request about all of them.
static GLOBAL_HOSTS: Mutex<Vec<RenderHost>> = Mutex::new(Vec::new());

/// The window the user is looking at, which is the window an API without a
/// window argument has to mean.
///
/// `control_at(x, y)` takes coordinates in a window, and a window is what makes
/// them mean anything, so the window-less API is about the one the keyboard
/// goes to. Recorded by the UI thread when a window takes focus.
static ACTIVE_HOST: Mutex<Option<RenderHost>> = Mutex::new(None);

/// Global HWND (Windows only) stored when the winit window is created.
#[cfg(target_os = "windows")]
pub(crate) static GLOBAL_HWND: OnceLock<isize> = OnceLock::new();

// ---------------------------------------------------------------------------
// Headless window adapter — used when a render-owned component is attached
// ---------------------------------------------------------------------------

#[cfg(render_thread_can_draw)]
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
#[cfg(render_thread_can_draw)]
struct MirrorRenderer {
    window_adapter: RefCell<Option<Rc<dyn WindowAdapter>>>,
}

#[cfg(render_thread_can_draw)]
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
#[cfg(render_thread_can_draw)]
struct RenderWindowAdapter {
    window: SlintApiWindow,
    size: Cell<PhysicalSize>,
    /// Set by the tree this adapter hosts when its properties change.  The
    /// render loop reads it to decide that the tree owes the screen a frame.
    frame_request: Rc<Cell<bool>>,
    renderer: MirrorRenderer,
}

#[cfg(render_thread_can_draw)]
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
#[cfg(render_thread_can_draw)]
struct RenderMirrorPlatform {
    clipboard: RefCell<crate::clipboard::ClipboardPair>,
    /// Handed to the adapter that hosts the mirror tree, so the tree's redraw
    /// requests reach the render loop.
    frame_request: Rc<Cell<bool>>,
}

#[cfg(render_thread_can_draw)]
impl RenderMirrorPlatform {
    fn new(frame_request: Rc<Cell<bool>>) -> Self {
        Self { clipboard: RefCell::new(crate::clipboard::create_clipboard()), frame_request }
    }
}

#[cfg(render_thread_can_draw)]
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
    /// The UI thread resolved a press to a control: this control is held, and
    /// with the press it also has the focus.  A release is what makes it a click.
    PressControl {
        /// The control the pointer went down on.
        id: u64,
        /// Which button went down, so that a right press stays a right press.
        button: PointerEventButton,
        /// Where the pointer went down, in the window's logical coordinates.
        ///
        /// A drag is a distance between two points, so the tree has to be given
        /// the point the user pressed on: an event placed in the middle of the
        /// control would make every drag start from a place the pointer was
        /// never at.
        position: LogicalPoint,
    },
    /// The pointer moved while a press was held: a drag.
    ///
    /// Named by position and not by control, because the point of a drag is
    /// often to leave the control -- a `Flickable` follows the pointer past its
    /// own edge -- and the tree's own grab is what decides which item keeps
    /// receiving the moves.
    MoveControl {
        /// Where the pointer is now, in the window's logical coordinates.
        position: LogicalPoint,
        /// Which finger, for a touch: a drag is one finger's story, and the tree
        /// tells two fingers apart by this.
        finger_id: i32,
    },
    /// The UI thread resolved a release to a control: the hold is over.
    ReleaseControl {
        /// The control the pointer came up on.
        id: u64,
        /// Which button came up, to end the press it began.
        button: PointerEventButton,
        /// Where the pointer came up, in the window's logical coordinates.
        ///
        /// A click is a press and a release at one point, so where the release
        /// landed is what decides between the two.
        position: LogicalPoint,
    },
    /// The UI thread resolved a wheel event to a control: this control is the
    /// one under the pointer, so it is the one the wheel is for.
    ScrollControl {
        /// The control the wheel belongs to.
        id: u64,
        /// Where the wheel was turned, in the window's logical coordinates.
        position: LogicalPoint,
        /// Horizontal wheel movement, in logical pixels.
        delta_x: f32,
        /// Vertical wheel movement, in logical pixels.
        delta_y: f32,
    },
    /// The same for a trackpad pinch, which a zoomable area answers and
    /// everything else ignores.
    PinchControl {
        /// The control under the pointer.
        id: u64,
        /// Where the fingers were, in the window's logical coordinates.
        position: LogicalPoint,
        /// How far the fingers moved apart.
        delta: f32,
    },
    /// The same for a trackpad rotation.
    RotateControl {
        /// The control under the pointer.
        id: u64,
        /// Where the fingers were, in the window's logical coordinates.
        position: LogicalPoint,
        /// How far the fingers rotated, in radians.
        delta: f32,
    },
    /// Run a call against the tree this thread draws.
    ///
    /// The pointer event above arrives from the thread that talks to the OS,
    /// and so does this: the application calls a callback from wherever it
    /// lives, and a backend that took the tree over answers that call on the
    /// tree the user is looking at. The tree is the argument rather than a
    /// window or a property name, because the call is the application's own and
    /// only the tree it wrote can carry it out.
    RunOnScreenTree {
        /// The call, to run once against the tree being drawn.
        task: Box<dyn FnOnce(&ItemTreeRc) + Send + 'static>,
        /// Reply channel, closed once the call has returned: the value it
        /// produced travels back on the caller's own channel.
        done: std::sync::mpsc::SyncSender<()>,
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
    /// The UI thread is about to be asked to draw, and asks first whether this
    /// thread is the one that will draw that window.
    ///
    /// The answer is what keeps the UI thread from drawing: it waits for it
    /// instead of painting the frames that would otherwise race the handover.
    AwaitGraphics {
        /// The winit window the UI thread is about to be asked about.
        window_id: winit::window::WindowId,
        /// Reply channel carrying whether this thread draws that window.
        response: std::sync::mpsc::SyncSender<bool>,
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
    ///
    /// It also tells a call that is already here: this thread's event loop is the
    /// one that would run it, so a call made from inside a callback of the tree it
    /// draws must run right here rather than be posted to a loop that cannot read
    /// it while it is the one making the call.
    static ON_RENDER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the calling thread is the render thread.
pub(crate) fn on_render_thread() -> bool {
    ON_RENDER_THREAD.with(|on| on.get())
}

/// `window` as the one a render thread presents into, and its native window if
/// it already has one.
///
/// The identity outlives the native window, because a window that is hidden and
/// shown again comes back as a new native window, while the adapter behind it
/// is the same object.
fn name_surface_owner(window: &SlintApiWindow) -> Option<(usize, winit::window::WindowId)> {
    let window_adapter = WindowInner::from_pub(window).window_adapter();
    let (identity, window_id) = window_adapter
        .internal(i_slint_core::InternalToken)
        .and_then(|wa| (wa as &dyn std::any::Any).downcast_ref::<WinitWindowAdapter>())
        .map(|adapter| {
            (
                std::rc::Rc::as_ptr(&window_adapter) as *const () as usize,
                adapter.winit_window().map(|w| w.id()),
            )
        })?;
    Some((identity, window_id?))
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
    /// The window this render thread presents into, as an address that stands
    /// for the window's adapter while it lives.
    ///
    /// Per host, because a render thread presents into exactly one window and
    /// the record is what tells a second window that this thread is not its
    /// renderer. An address a later window is given once the old one is gone can
    /// only make that window think it owns this thread, which costs it a frame,
    /// never correctness.
    surface: Arc<Mutex<Option<usize>>>,
    /// The controls this render thread composited, published for the UI thread
    /// and for workers to hit-test against.
    ///
    /// One table per render thread, because a control id names an item in one
    /// tree: with one process-wide table two windows would publish ids that
    /// mean different items, and a pointer in the second window would be
    /// resolved against the first window's layout.
    coords: Arc<Mutex<PublishedControls>>,
}

impl Clone for RenderHost {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            event_loop_proxy: self.event_loop_proxy.clone(),
            attached: self.attached.clone(),
            surface: self.surface.clone(),
            coords: self.coords.clone(),
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
        if let Some((identity, window_id)) = name_surface_owner(window) {
            self.set_surface(identity);
            let _ = self.sender.send(RenderMessage::SetSurfaceOwner { window_id });
        }
        self.attached.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .sender
            .send(RenderMessage::AttachComponent { factory: Box::new(factory), property_access });
    }

    /// Record which window this render thread presents into.
    fn set_surface(&self, identity: usize) {
        if let Ok(mut surface) = self.surface.lock() {
            *surface = Some(identity);
        }
    }

    /// Whether this render thread presents into this window.
    ///
    /// A render thread presents into one window, so the first window that names
    /// it is the one it takes, and any later window is a window whose rendering
    /// has to be a render thread of its own.
    fn owns(&self, identity: usize) -> bool {
        let mut surface = self.surface.lock().expect("the surface record is not poisoned");
        if surface.is_none() {
            *surface = Some(identity);
        }
        *surface == Some(identity)
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

    /// Tell the tree that this control has the press.
    ///
    /// This is the grab and the focus, and nothing else: whether the release
    /// that comes next is a click is decided by the tree, because the tree is
    /// what knows whether the pointer moved away in the meantime.
    pub fn press_control(
        &self,
        id: u64,
        button: PointerEventButton,
        position: LogicalPoint,
    ) -> bool {
        self.sender.send(RenderMessage::PressControl { id, button, position }).is_ok()
    }

    /// Tell the tree that the pointer moved while the press was held.
    ///
    /// Sent for as long as the pointer is down, which is the whole of a drag: the
    /// UI thread reports where the pointer is, and the tree decides what a
    /// distance between two points means -- a slider's value, a `Flickable`'s
    /// scroll position, a `TouchArea`'s `moved`.
    pub fn move_control(&self, position: LogicalPoint, finger_id: i32) -> bool {
        self.sender.send(RenderMessage::MoveControl { position, finger_id }).is_ok()
    }

    /// Tell the tree that the press on this control is over.
    pub fn release_control(
        &self,
        id: u64,
        button: PointerEventButton,
        position: LogicalPoint,
    ) -> bool {
        self.sender.send(RenderMessage::ReleaseControl { id, button, position }).is_ok()
    }

    /// Tell the tree that the wheel belongs to this control.
    ///
    /// Which control it is, the UI thread decided from the published geometry;
    /// what scrolling it means is the tree's, because that is a property of the
    /// control rather than of the event.
    pub fn scroll_control(
        &self,
        id: u64,
        position: LogicalPoint,
        delta_x: f32,
        delta_y: f32,
    ) -> bool {
        self.sender.send(RenderMessage::ScrollControl { id, position, delta_x, delta_y }).is_ok()
    }

    /// Tell the tree that a pinch belongs to this control.
    pub fn pinch_control(&self, id: u64, position: LogicalPoint, delta: f32) -> bool {
        self.sender.send(RenderMessage::PinchControl { id, position, delta }).is_ok()
    }

    /// Tell the tree that a rotation belongs to this control.
    pub fn rotate_control(&self, id: u64, position: LogicalPoint, delta: f32) -> bool {
        self.sender.send(RenderMessage::RotateControl { id, position, delta }).is_ok()
    }

    /// Run a call against the tree this thread draws, and wait for it.
    ///
    /// Unlike a pointer event, a callback answers with a value -- a function
    /// returns one -- so the caller blocks until the tree has had its say. That
    /// is what an application means when it calls one: it wants the answer of
    /// the tree on screen, and a render thread that is between frames can give
    /// it.
    ///
    /// `Ok` means the tree ran the call. `Err` hands the call back because it
    /// was not taken -- there is no tree attached, or this thread is gone -- so
    /// that the caller can run it against the tree it holds. A call that was
    /// taken and then lost with the thread reports `Ok`, and the caller's reply
    /// channel closes, which hands back a default instead of hanging.
    pub fn run_on_screen_tree(
        &self,
        task: Box<dyn FnOnce(&ItemTreeRc) + Send + 'static>,
    ) -> Result<(), Box<dyn FnOnce(&ItemTreeRc) + Send + 'static>> {
        if !self.attached.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(task);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        match self.sender.send(RenderMessage::RunOnScreenTree { task, done: tx }) {
            Ok(()) => {
                let _ = rx.recv().is_ok();
                Ok(())
            }
            // A send that fails means the render thread is gone, and it takes the
            // call with it. The call comes back in the error, so the caller can
            // still run it rather than drop it.
            Err(std::sync::mpsc::SendError(message)) => {
                let RenderMessage::RunOnScreenTree { task, .. } = message else {
                    unreachable!("only one message is ever sent from here")
                };
                Err(task)
            }
        }
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
    /// Every window is this thread's to draw, so the request goes nowhere else:
    /// the UI thread has no drawing to do and is not told about frames.
    pub fn request_redraw(&self) {
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
        if on_render_thread() {
            if let Some(r) = with_tl_access(|a| a.coords.lock().unwrap().control_at(x, y)) {
                return r;
            }
        }
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
        if on_render_thread() {
            if let Some(r) = with_tl_access(|a| a.coords.lock().unwrap().control_at(x, y)) {
                return r;
            }
        }
        self.coords.lock().unwrap().control_at(x, y)
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
        // can tell: it presents into one window, the first one to name it, and a
        // window it does not present into is a window this thread must not draw.
        let is_surface = self.owns(std::rc::Weak::as_ptr(&adapter) as *const () as usize);
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

    /// Wait until the render thread is ready to take a window's graphics.
    ///
    /// A window has one OpenGL context, not one per thread, so the render
    /// thread cannot have a drawing context while the UI thread's renderer
    /// holds one.  The UI thread asks this question instead of drawing the
    /// frames that race the handover: it is about to do the one thing it must
    /// never do, and waiting for the answer is what keeps it from doing it.
    ///
    /// Returns whether this thread is drawing that window -- or is about to --
    /// which is what makes releasing the graphics safe.
    pub(crate) fn await_graphics(&self, window_id: winit::window::WindowId) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        if self.sender.send(RenderMessage::AwaitGraphics { window_id, response: tx }).is_err() {
            return false;
        }
        // No timeout: the render thread is a thread of this process and answers
        // without needing anything from the UI thread's event loop, so there is
        // nothing this wait depends on but the render thread itself.
        rx.recv().unwrap_or(false)
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
    /// The table this thread publishes into. It is the same table the UI thread
    /// reads, reached from the other end through the [`RenderHost`].
    coords: Arc<Mutex<PublishedControls>>,
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
    #[cfg(render_thread_can_draw)]
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
    draw: Option<DrawTarget>,
    /// Set while the UI thread still owns the owner's graphics, which keeps this
    /// thread from creating a context of its own until it answers.
    handover_pending: bool,
}

impl SurfaceState {
    fn new() -> Self {
        Self { configured: Vec::new(), owner: None, draw: None, handover_pending: false }
    }

    /// Remember a window and its size, without touching its graphics yet.
    fn configure(
        &mut self,
        window: Arc<winit::window::Window>,
        width: u32,
        height: u32,
        #[cfg_attr(not(render_thread_can_draw), allow(unused_variables))] scale_factor: f32,
    ) {
        let window_id = window.id();
        let surface = RenderSurface {
            window,
            width,
            height,
            #[cfg(render_thread_can_draw)]
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
    #[cfg(render_thread_can_draw)]
    fn scale_factor(&self) -> f32 {
        self.surface().map_or(1., |surface| surface.scale_factor)
    }

    /// Whether the surface's graphics are on their way from the UI thread.  The
    /// loop waits to be spoken to rather than on the clock until they arrive.
    #[cfg(render_thread_can_draw)]
    fn handover_pending(&self) -> bool {
        self.handover_pending
    }

    fn obtain(&mut self) -> Option<&mut DrawTarget> {
        // A resize that arrived while the handover was in flight has not been
        // given to the context yet; this is where it takes effect.
        let wanted = self.surface().map(|surface| (surface.width, surface.height));
        if let (Some(draw), Some((width, height))) = (self.draw.as_mut(), wanted)
            && (draw.width() != width || draw.height() != height)
        {
            draw.resize(width, height);
        }
        self.draw.as_mut()
    }

    /// Record that the UI thread released the window's graphics, and create the
    /// context now that the window has none.  Returns the state to present
    /// with, or `None` if the release names a window this thread does not draw
    /// into, or the context cannot be created.
    fn released(&mut self, window_id: winit::window::WindowId) -> Option<&mut DrawTarget> {
        if !self.owns(window_id) {
            // Another window's UI renderer let go of its context. That is not an
            // answer to a question this thread asked, so the handover is still
            // open for the window that was asked about.
            return None;
        }
        self.handover_pending = false;
        if self.draw.is_some() {
            return self.draw.as_mut();
        }
        let surface = self.surface()?;
        match DrawTarget::new(surface.window.clone(), surface.width, surface.height) {
            Ok(state) => {
                self.draw = Some(state);
                self.draw.as_mut()
            }
            Err(e) => {
                eprintln!("dualslint render thread: no way to present this window: {e}");
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
        self.draw = None;
        self.handover_pending = false;
    }
}

impl RenderCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, coords: Arc<Mutex<PublishedControls>>) -> Self {
        Self { rx, coords }
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
        #[cfg_attr(not(render_thread_can_draw), allow(unused_mut))]
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
        #[cfg_attr(not(render_thread_can_draw), allow(unused_mut))]
        let mut render_component: Option<Box<dyn std::any::Any>> = None;
        // How to ask the application about the properties of that component.
        // Filled in by the attach handler, which only the GPU renderers run.
        #[cfg_attr(not(render_thread_can_draw), allow(unused_mut))]
        let mut render_property_access: Option<ComponentPropertyAccess> = None;
        // Last system accent forwarded by the UI thread; applied to the
        // mirror context on attach in case the accent update arrives before
        // the mirror component exists.
        // Read back in the mirror's attach, which only the GPU renderers do.
        #[cfg_attr(not(render_thread_can_draw), allow(unused_variables))]
        let mut accent: Option<Color> = None;
        // Set by the mirror tree when its own properties change, so the loop
        // knows the tree owes the screen a frame without the UI thread having to
        // ask for it.  A fresh cell per attachment: the flag belongs to the tree
        // that set it, not to this thread.
        #[cfg(render_thread_can_draw)]
        let mut frame_request: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        loop {
            // How long this thread may sleep.  A running animation is not a
            // timer, so while one plays the wait is capped at a frame interval;
            // otherwise the answer is when the tree's next timer is due.  This
            // is the shape the android-activity loop uses.
            #[cfg(render_thread_can_draw)]
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
            #[cfg(not(render_thread_can_draw))]
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
                    #[cfg_attr(not(render_thread_can_draw), allow(unused_variables))]
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
                            #[cfg(render_thread_can_draw)]
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
                    RenderMessage::AwaitGraphics { window_id, response } => {
                        // This thread draws a window when it is the one drawing
                        // into it, and it draws it once the UI thread has let go
                        // of the graphics.  Saying so here -- and marking the
                        // handover as the thing being waited for -- is what lets
                        // the UI thread release them without ever painting.
                        let draws = surface.owns(window_id);
                        if draws && surface.draw.is_none() {
                            surface.handover_pending = true;
                        }
                        let _ = response.send(draws);
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
                        #[cfg(render_thread_can_draw)]
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
                    #[cfg_attr(not(render_thread_can_draw), allow(unused_variables))]
                    RenderMessage::AttachComponent { factory, property_access } => {
                        // The factory runs *after* the headless platform is seeded:
                        // it instantiates the app's component, which needs this
                        // thread's `GLOBAL_CONTEXT` to already be claimed.
                        #[cfg(render_thread_can_draw)]
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
                            self.coords.lock().unwrap().clear();
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
                    RenderMessage::PressControl { id, button, position } => {
                        present |= send_pointer_event_to_control(
                            render_window_adapter.as_ref(),
                            render_controls.get(&id),
                            MouseEventKind::Pressed,
                            button,
                            position,
                        );
                    }
                    RenderMessage::MoveControl { position, finger_id } => {
                        present |= send_pointer_move_to_tree(
                            render_window_adapter.as_ref(),
                            position,
                            finger_id,
                        );
                    }
                    RenderMessage::ReleaseControl { id, button, position } => {
                        present |= send_pointer_event_to_control(
                            render_window_adapter.as_ref(),
                            render_controls.get(&id),
                            MouseEventKind::Released,
                            button,
                            position,
                        );
                    }
                    RenderMessage::ScrollControl { id, position, delta_x, delta_y } => {
                        present |= send_gesture_to_control(
                            render_window_adapter.as_ref(),
                            render_controls.get(&id),
                            Gesture::Wheel { delta_x, delta_y },
                            position,
                        );
                    }
                    RenderMessage::PinchControl { id, position, delta } => {
                        present |= send_gesture_to_control(
                            render_window_adapter.as_ref(),
                            render_controls.get(&id),
                            Gesture::Pinch { delta },
                            position,
                        );
                    }
                    RenderMessage::RotateControl { id, position, delta } => {
                        present |= send_gesture_to_control(
                            render_window_adapter.as_ref(),
                            render_controls.get(&id),
                            Gesture::Rotation { delta },
                            position,
                        );
                    }
                    RenderMessage::ActivateControl { id, response } => {
                        // A press and a release at the same point, which is all a
                        // click is before the tree has had its say about it.
                        let activated = render_window_adapter
                            .as_ref()
                            .zip(render_controls.get(&id))
                            .is_some_and(|(adapter, region)| activate_control(adapter, region));
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
                    RenderMessage::RunOnScreenTree { task, done } => {
                        // The call runs here because the tree it belongs to is
                        // here: the mirror's window is a headless one, and its
                        // component is the one the user is looking at. A call that
                        // changes a property or opens a window therefore does it
                        // to the tree that is drawn, and a frame follows for
                        // whatever it changed.
                        if let Some(tree) = render_window_adapter.as_ref().and_then(|adapter| {
                            WindowInner::from_pub(adapter.window()).try_component()
                        }) {
                            task(&tree);
                            present = true;
                        }
                        // The caller's channel closes whether the tree was there
                        // or not, so a caller that is waiting for a value learns
                        // the difference as a default rather than as a hang.
                        let _ = done.send(());
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
                        // Repaint what this thread draws, which is every window:
                        // there is no other thread left that draws.
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
            #[cfg(render_thread_can_draw)]
            let (frame_requested, animating_now) =
                (frame_request.replace(false), animating(&render_window_adapter));
            #[cfg(not(render_thread_can_draw))]
            let (frame_requested, animating_now) = (false, false);
            // Only a window this thread has a tree for is drawn here.  Without
            // one the render thread stays out of the way, and its context must
            // never be taken from a window the UI thread draws itself.
            if render_component.is_some()
                && (present || frame_requested || animating_now)
                && let Some(state) = surface.obtain()
            {
                present_render_owned(
                    state,
                    &self.coords,
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
#[cfg(render_thread_can_draw)]
#[allow(clippy::too_many_arguments)]
fn publish_mirror_controls(
    coords: &Arc<Mutex<PublishedControls>>,
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
    publish_control_coords(coords, &table.controls, interaction);
    // Set thread-local access so in-thread calls don't deadlock
    #[cfg(render_thread_can_draw)]
    {
        let item_map: Arc<Mutex<std::collections::HashMap<u64, i_slint_core::item_tree::ItemRc>>> =
            Arc::new(Mutex::new(render_item_rcs.clone()));
        let tl = ThreadLocalAccess { coords: coords.clone(), item_map };
        set_tl_access(Some(tl));
    }
    true
}

/// How long the render thread waits between frames while an animation plays.
/// It has no vsync source of its own, so this stands in for the display's
/// refresh interval; the android-activity loop uses the same fallback.
#[cfg(render_thread_can_draw)]
const RENDER_FRAME_INTERVAL_MS: u64 = 10;

/// Whether the render-owned tree is animating, which is what tells the render
/// thread that the screen is owed a frame without a message to say so.
#[cfg(render_thread_can_draw)]
fn animating(adapter: &Option<Rc<dyn WindowAdapter>>) -> bool {
    adapter.as_ref().is_some_and(|adapter| adapter.window().has_active_animations())
}

/// Tell the headless adapter that hosts the render-owned component about the
/// real window geometry.  Without this its layout stays at the placeholder
/// size from `create_window_adapter`, and the tree is laid out and clipped
/// for a window that is not the one being presented into.
#[cfg(render_thread_can_draw)]
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
#[cfg(render_thread_can_draw)]
#[allow(clippy::too_many_arguments)]
fn present_render_owned(
    state: &mut DrawTarget,
    coords: &Arc<Mutex<PublishedControls>>,
    render_window_adapter: &Option<Rc<dyn WindowAdapter>>,
    render_controls: &mut HashMap<u64, ControlRegion>,
    render_control_order: &mut Vec<u64>,
    render_item_rcs: &mut HashMap<u64, ItemRc>,
    interaction: &mut ControlInteraction,
) {
    if !publish_mirror_controls(
        coords,
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
    if let Err(e) = state.present(adapter) {
        eprintln!("dualslint render thread: present failed: {e}");
    }
}

/// The same present on a build with no upstream item renderer for the render
/// thread to drive.  A component is only ever attached where one exists, so
/// this has nothing to draw; it is here so that every build presents a message
/// the same way instead of each handler remembering the feature list.
#[cfg(not(render_thread_can_draw))]
#[allow(clippy::too_many_arguments)]
fn present_render_owned(
    _state: &mut DrawTarget,
    _coords: &Arc<Mutex<PublishedControls>>,
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
/// The position is the control's own centre, taken from its geometry, because
/// the pointer is not part of this path: what arrives here is a key, so there is
/// no place on screen the user pointed at and the control's middle is as good a
/// point as any. Both events go through the window, which is also how the item
/// stack is built: the tree finds for itself which items the point is inside,
/// exactly as it would for a real press.
fn activate_control(adapter: &Rc<dyn WindowAdapter>, region: &ControlRegion) -> bool {
    let position = control_centre(region);

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

/// A gesture that the UI thread has already attributed to a control.
///
/// Which control it belongs to is a question about geometry, and the UI thread
/// answered it from the geometry the render thread published. What the gesture
/// *does* is a question about the control, so it is asked here, on the thread
/// that holds the control.
enum Gesture {
    Wheel { delta_x: f32, delta_y: f32 },
    Pinch { delta: f32 },
    Rotation { delta: f32 },
}

/// Hand a gesture to the tree at the control the UI thread named.
///
/// The event is placed at the control's centre because the tree hit-tests the
/// point it is given, and the control the UI thread picked is the one that has
/// to be under it.
fn send_gesture_to_control(
    adapter: Option<&Rc<dyn WindowAdapter>>,
    region: Option<&ControlRegion>,
    gesture: Gesture,
    position: LogicalPoint,
) -> bool {
    let Some((adapter, _region)) = adapter.zip(region) else {
        return false;
    };
    let event = i_slint_core::platform::InternalEvent::Mouse(match gesture {
        Gesture::Wheel { delta_x, delta_y } => BackendMouseEvent::Wheel {
            position,
            delta_x,
            delta_y,
            phase: i_slint_core::input::TouchPhase::Moved,
        },
        Gesture::Pinch { delta } => BackendMouseEvent::PinchGesture {
            position,
            delta,
            phase: i_slint_core::input::TouchPhase::Moved,
        },
        Gesture::Rotation { delta } => BackendMouseEvent::RotationGesture {
            position,
            delta,
            phase: i_slint_core::input::TouchPhase::Moved,
        },
    });
    matches!(
        adapter.window().dispatch_event_with_result(WindowEvent::internal(event)),
        Ok(WindowEventDispatchResult::Accepted)
    )
}

/// Which part of a click is being built: the press, or the release that ends it.
enum MouseEventKind {
    Pressed,
    Released,
}

/// The middle of a control, in the coordinates of the window that published it.
///
/// Used for the events that have no pointer of their own -- the click a key
/// stands for -- so that the point handed to the tree is inside the control the
/// UI thread named.
fn control_centre(region: &ControlRegion) -> LogicalPoint {
    region.geometry.origin + region.geometry.size.to_vector() * 0.5
}

/// Hand one half of a click to the control the UI thread named.
///
/// The position is the one the pointer is at, in the coordinates of the window
/// the UI thread heard about, which is the space the tree hit-tests in -- a popup
/// included, because the popup's geometry is published in the window's
/// coordinates too.
///
/// The control is only asked for as a check that it is still there: the frame
/// that published it and the frame that dispatches to it are frames apart, and a
/// control that has since gone should not be given an event.
fn send_pointer_event_to_control(
    adapter: Option<&Rc<dyn WindowAdapter>>,
    region: Option<&ControlRegion>,
    kind: MouseEventKind,
    button: PointerEventButton,
    position: LogicalPoint,
) -> bool {
    if adapter.is_none() || region.is_none() {
        return false;
    }
    let adapter = adapter.expect("checked above");

    let event = i_slint_core::platform::InternalEvent::Mouse(match kind {
        MouseEventKind::Pressed => {
            BackendMouseEvent::Pressed { position, button, click_count: 1, touch_finger_id: 0 }
        }
        MouseEventKind::Released => {
            BackendMouseEvent::Released { position, button, click_count: 1, touch_finger_id: 0 }
        }
    });
    matches!(
        adapter.window().dispatch_event_with_result(WindowEvent::internal(event)),
        Ok(WindowEventDispatchResult::Accepted)
    )
}

/// Hand a move to the tree, for as long as a press is held.
///
/// No control is named: a drag that leaves the control it started on is
/// ordinary, and the tree is what knows which item grabbed the pointer and how
/// far it has travelled.
fn send_pointer_move_to_tree(
    adapter: Option<&Rc<dyn WindowAdapter>>,
    position: LogicalPoint,
    finger_id: i32,
) -> bool {
    let Some(adapter) = adapter else {
        return false;
    };
    let event = i_slint_core::platform::InternalEvent::Mouse(BackendMouseEvent::Moved {
        position,
        touch_finger_id: finger_id,
    });
    matches!(
        adapter.window().dispatch_event_with_result(WindowEvent::internal(event)),
        Ok(WindowEventDispatchResult::Accepted)
    )
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
    let coords = Arc::new(Mutex::new(PublishedControls::default()));
    let host = RenderHost {
        sender: tx,
        event_loop_proxy: Some(event_loop_proxy),
        attached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        surface: Arc::new(Mutex::new(None)),
        coords: coords.clone(),
    };
    let core = RenderCore::new(rx, coords);
    (host, core)
}

// ---------------------------------------------------------------------------
// Accessor helpers
// ---------------------------------------------------------------------------

/// Every render thread of this process, for the APIs that are not about one
/// window.
///
/// A worker thread has no window of its own to name, so `request_redraw` means
/// every window that a render thread draws, and that is what this is for.
pub fn hosts() -> Vec<RenderHost> {
    GLOBAL_HOSTS.lock().map(|hosts| hosts.clone()).unwrap_or_default()
}

/// The render thread of the window the user is looking at.
///
/// A process with one window has one answer; a process with several has the
/// focused one, which is the only window whose coordinates a caller who named
/// none can have meant.
pub fn active_host() -> Option<RenderHost> {
    if let Ok(active) = ACTIVE_HOST.lock()
        && let Some(host) = active.as_ref()
    {
        return Some(host.clone());
    }
    let hosts = hosts();
    (hosts.len() == 1).then(|| hosts[0].clone())
}

/// Record which window the user is looking at. Called by the UI thread, which
/// is the thread that hears about focus.
pub(crate) fn set_active_host(host: &RenderHost) {
    if let Ok(mut active) = ACTIVE_HOST.lock() {
        *active = Some(host.clone());
    }
}

/// Start a render thread for one window and remember it.
///
/// Called by the window adapter that owns the surface, because a render thread
/// is only worth starting for a window that exists: the window itself arrives
/// later as [`RenderMessage::Configure`], and until then the thread waits.
pub(crate) fn create_host(
    event_loop_proxy: &winit::event_loop::EventLoopProxy<crate::SlintEvent>,
) -> RenderHost {
    let (host, mut core) = channel(event_loop_proxy.clone());
    if let Ok(mut hosts) = GLOBAL_HOSTS.lock() {
        hosts.push(host.clone());
    }
    std::thread::Builder::new().name("slint-render".into()).spawn(move || core.run()).ok();
    host
}

/// Hand `window`'s drawing to the render thread, and say whether the render
/// thread is the one drawing it.
///
/// Called where the UI thread is about to be asked to draw, because that is the
/// last moment at which this window's component can still hand the render thread
/// something.  A window the render thread will not take is not drawn by the UI
/// thread either: the UI thread does not draw, so
/// [`report_window_no_one_draws`] names the reason instead of a blank window
/// staying unexplained.
pub(crate) fn take_over_drawing_of(window: &SlintApiWindow, host: Option<&RenderHost>) -> bool {
    let Some(host) = host else {
        report_window_no_one_draws(window, "this window has no render thread to draw it");
        return false;
    };
    let Some(factory) = WindowInner::from_pub(window).render_factory() else {
        report_window_no_one_draws(
            window,
            "its component left no render factory behind, which is what a \
             ComponentFactory that builds the tree itself, or a component that is \
             not the window's root, does",
        );
        return false;
    };
    if !cfg!(render_thread_can_draw) {
        report_window_no_one_draws(
            window,
            "this build has no renderer on the render thread, because no renderer \
             feature that can draw off the UI thread is enabled",
        );
        return false;
    }
    if !host.has_attached_component() {
        host.attach_component_with_to(window, move || factory(), None);
    } else if !host.owns(window_identity(window)) {
        report_window_no_one_draws(
            window,
            "this window's render thread is already presenting into another \
             window, and a render thread presents into one window at a time",
        );
        return false;
    }
    true
}

/// The windows this process has already reported as drawn by nobody.
///
/// [`std::sync::Mutex::new`] is const, so this needs no lazy initialisation. The
/// `Option` keeps the set empty until the first report.
static UNDRAWN_REPORTED: std::sync::Mutex<Option<std::collections::HashSet<usize>>> =
    std::sync::Mutex::new(None);

/// Say, once per window, that nothing is drawing it.
///
/// This fork has the render thread own every control and every pixel, and the UI
/// thread draws nothing at all, so a window the render thread refuses is a
/// window that stays blank.  A blank window with a reason is the honest outcome;
/// a blank window without one is the bug this report exists to prevent, because
/// the symptom -- an application that shows nothing -- names neither the
/// missing renderer nor the missing factory.  Remembering the windows keeps a
/// draw loop that runs 60 times a second from repeating the message every frame.
pub(crate) fn report_window_no_one_draws(window: &SlintApiWindow, reason: &str) {
    if ON_RENDER_THREAD.with(|on| on.get()) {
        // The render thread is where a window is supposed to be drawn, and the
        // mirror's own draws come through here too.
        return;
    }
    {
        let mut reported = UNDRAWN_REPORTED.lock().unwrap_or_else(|e| e.into_inner());
        let reported = reported.get_or_insert_with(Default::default);
        if !reported.insert(window_identity(window)) {
            return;
        }
    }
    eprintln!(
        "dualslint: nothing is drawing this window because {reason}. The render \
         thread draws every window and the UI thread draws none, so this window \
         will stay blank, and its controls are not reachable through the render \
         thread's control API."
    );
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
    for host in hosts() {
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

/// Borrow the control under the logical point `(x, y)`, using the active host.
///
/// Returns the control id, or `0` when the point is over no control or nothing
/// has been composited yet.  Control ids are never `0`, so `0` is unambiguous
/// as "no control".  This reads the published geometry and does not wait for the
/// render thread, so it is usable from a pointer move.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_control_at(x: f32, y: f32) -> u64 {
    active_host().and_then(|host| host.control_at(x, y)).unwrap_or(0)
}

/// Like [`slint_render_thread_control_at`] but uses the active host and waits
/// answer from the control tree as it stands now rather than from the last
/// composited frame.  Use this when the answer has to be current, such as
/// resolving a click.
#[unsafe(no_mangle)]
pub extern "C" fn slint_render_thread_hit_test(x: f32, y: f32) -> u64 {
    active_host().and_then(|host| host.hit_test(x, y)).unwrap_or(0)
}

/// Assign one property on the active host's tree, blocking until
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
    active_host().is_some_and(|host| host.set_control_property(id, property, value))
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

/// Read one property on the active host's tree, blocking until
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
    let Some(value) = active_host().and_then(|host| host.get_control_property(id, property)) else {
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
    if let Some(host) = active_host() {
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
    let Some(host) = active_host() else { return false };
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
    let Some(host) = active_host() else { return false };
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
    for host in hosts() {
        host.submit_accent(color);
    }
}

/// The controls the given render thread last composited.
///
/// UI thread and workers hit-test against this during event processing, so it
/// is the geometry the render thread actually drew rather than what the layout
/// would say. It belongs to one render thread, because a control id names an
/// item in one tree.
pub fn coordinate_map(host: &RenderHost) -> Arc<Mutex<PublishedControls>> {
    host.coords.clone()
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
    #[cfg(render_thread_can_draw)]
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
#[cfg(render_thread_can_draw)]
fn publish_control_coords(
    map: &Arc<Mutex<PublishedControls>>,
    controls: &[ControlRegion],
    interaction: &ControlInteraction,
) {
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
#[cfg(feature = "renderer-femtovg")]
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

/// How the render thread puts a frame of the render-owned tree on screen.
///
/// A build picks one at compile time, and the window picks one when its graphics
/// are released: a machine with a GPU presents through GL, and a build without
/// one presents through a buffer it copies in.  Both are this thread's work --
/// the point of the enum is that there is no third case in which the UI thread
/// presents, because nothing outside this enum can draw at all.
#[cfg(render_thread_can_draw)]
enum DrawTarget {
    #[cfg(feature = "renderer-femtovg")]
    Gl(GlRenderState),
    #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
    Software(SoftwareRenderState),
}

#[cfg(render_thread_can_draw)]
impl DrawTarget {
    /// Take a window's presentation over.
    fn new(window: Arc<winit::window::Window>, width: u32, height: u32) -> Result<Self, String> {
        // A build with both renderers prefers the GPU: the software path exists
        // for a build that has nothing else, not as a fallback for a frame.
        #[cfg(feature = "renderer-femtovg")]
        {
            GlRenderState::new(window, width, height).map(Self::Gl)
        }
        #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
        {
            SoftwareRenderState::new(window, width, height).map(Self::Software)
        }
    }

    fn width(&self) -> u32 {
        match self {
            #[cfg(feature = "renderer-femtovg")]
            Self::Gl(state) => state.width,
            #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
            Self::Software(state) => state.width,
        }
    }

    fn height(&self) -> u32 {
        match self {
            #[cfg(feature = "renderer-femtovg")]
            Self::Gl(state) => state.height,
            #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
            Self::Software(state) => state.height,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        match self {
            #[cfg(feature = "renderer-femtovg")]
            Self::Gl(state) => state.resize(width, height),
            #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
            Self::Software(state) => state.resize(width, height),
        }
    }

    /// Draw the render-owned component and put it on screen.
    #[cfg_attr(not(feature = "renderer-femtovg"), allow(unused_variables))]
    fn present(&mut self, window_adapter: &Rc<dyn WindowAdapter>) -> Result<(), PlatformError> {
        match self {
            #[cfg(feature = "renderer-femtovg")]
            Self::Gl(state) => {
                state.ensure_upstream(window_adapter)?;
                state.render_upstream()
            }
            #[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
            Self::Software(state) => state.present(window_adapter),
        }
    }
}

/// The render thread presenting through a pixel buffer.
///
/// This is the same software renderer the UI thread used to present with, on the
/// other side of the handover: the frames are drawn here and copied to the
/// window from here, so a build without a GPU is render-owned like any other.
#[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
struct SoftwareRenderState {
    renderer: i_slint_renderer_software::SoftwareRenderer,
    context: softbuffer::Context<Arc<winit::window::Window>>,
    surface: softbuffer::Surface<Arc<winit::window::Window>, Arc<winit::window::Window>>,
    width: u32,
    height: u32,
}

#[cfg(all(not(feature = "renderer-femtovg"), feature = "renderer-software"))]
impl SoftwareRenderState {
    fn new(window: Arc<winit::window::Window>, width: u32, height: u32) -> Result<Self, String> {
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| format!("softbuffer context: {e}"))?;
        let surface = softbuffer::Surface::new(&context, window)
            .map_err(|e| format!("softbuffer surface: {e}"))?;
        Ok(Self {
            renderer: i_slint_renderer_software::SoftwareRenderer::new(),
            context,
            surface,
            width,
            height,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    fn present(&mut self, window_adapter: &Rc<dyn WindowAdapter>) -> Result<(), PlatformError> {
        use i_slint_core::renderer::RendererSealed as _;
        if self.renderer.window_adapter().is_none() {
            // The renderer asks the platform how big the window is rather than
            // taking it from the tree, so it needs to be told about the window it
            // is drawing into before it can draw anything.
            self.renderer.set_window_adapter(window_adapter);
        }
        let Some((width, height)) = NonZeroU32::new(self.width).zip(NonZeroU32::new(self.height))
        else {
            return Ok(());
        };
        self.surface.resize(width, height).map_err(|e| format!("softbuffer resize: {e}"))?;
        let winit_window = self.surface.window().clone();
        let mut buffer =
            self.surface.buffer_mut().map_err(|e| format!("softbuffer buffer: {e}"))?;
        self.renderer
            .set_repaint_buffer_type(i_slint_renderer_software::RepaintBufferType::NewBuffer);
        let damage = self
            .renderer
            .render(
                bytemuck::cast_slice_mut::<u32, crate::renderer::sw::SoftBufferPixel>(
                    &mut buffer[..],
                ),
                width.get() as usize,
            )
            .iter()
            .filter_map(|(pos, size)| {
                Some(softbuffer::Rect {
                    x: pos.x as u32,
                    y: pos.y as u32,
                    width: NonZeroU32::new(size.width)?,
                    height: NonZeroU32::new(size.height)?,
                })
            })
            .collect::<Vec<_>>();
        if !damage.is_empty() {
            winit_window.pre_present_notify();
            buffer.present_with_damage(&damage).map_err(|e| format!("softbuffer present: {e}"))?;
        }
        let _ = &self.context;
        Ok(())
    }
}

#[cfg(feature = "renderer-femtovg")]
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

#[cfg(feature = "renderer-femtovg")]
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

#[cfg(render_thread_can_draw)]
fn set_tl_access(access: Option<ThreadLocalAccess>) {
    THREAD_LOCAL_ACCESS.with(|a| *a.borrow_mut() = access);
}
#[cfg(not(render_thread_can_draw))]
fn set_tl_access(_access: Option<ThreadLocalAccess>) {}
