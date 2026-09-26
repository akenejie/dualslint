// Copyright © akenejie <mailto:akenejie@gmail.com>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore DummyDisplayHandle

//! Dedicated render thread for the android-activity backend.
//!
//! The desktop fork renders a mirror component tree on a dedicated GL thread.
//! On Android the renderer is [`SkiaRenderer`] (FemtoVG is disabled on this
//! platform because it cannot load fonts without fontconfig), which is
//! `!Send`: it is created and bound to the `ANativeWindow` on the render
//! thread itself.
//!
//! The flow mirrors the winit fork, minus the scene snapshot encoder:
//!
//! * The UI thread forwards the native window (`Configure`), size changes
//!   (`Resize`) and repaint requests (`RequestRedraw`) over a channel.
//! * The render thread owns the [`SkiaRenderer`] bound to the `ANativeWindow`.
//!   With `aa-06` it rasterises into the window's own pixel buffer
//!   ([`crate::cpu_surface`]); otherwise it goes through a GL surface.
//! * When the application attaches a mirror component (see
//!   [`RenderHost::attach_component`]), the render thread instantiates it
//!   *there* (headless platform + [`SkiaRenderer::render()`]) and is then the
//!   only thread allowed to touch the surface.
//!
//! While a mirror is attached the UI-thread adapter suppresses its own
//! rendering and the screen is produced solely by this thread.

use i_slint_core::api::{PhysicalSize, PlatformError, Window as SlintApiWindow};
use i_slint_core::graphics::RequestedGraphicsAPI;
use i_slint_core::platform::{Platform, WindowAdapter, WindowEvent, WindowEventDispatchResult};
use i_slint_core::renderer::RendererSealed;
use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

#[cfg(feature = "aa-06")]
use crate::android_activity::ndk::native_window::NativeWindow;
#[cfg(feature = "aa-06")]
use crate::cpu_surface::NativeWindowSurface;

/// The window handle the UI thread hands over in
/// [`RenderMessage::Configure`].
///
/// With `aa-06` it is the concrete `ANativeWindow`, so the render thread can
/// lock the buffer and rasterise into it (see [`crate::cpu_surface`]). `aa-05`
/// only offers the erased `raw-window-handle`, and its ndk has no CPU buffer
/// lock, so that platform keeps the GL surface.
#[cfg(feature = "aa-06")]
pub(crate) type ConfigureWindow = Arc<NativeWindow>;
#[cfg(not(feature = "aa-06"))]
pub(crate) type ConfigureWindow = Arc<dyn raw_window_handle::HasWindowHandle + Send + Sync>;

/// The render thread host. Clone to share across threads (UI thread and
/// workers are equal peers).
#[derive(Clone)]
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,
    /// Set to `true` once `AttachComponent` has been sent. The UI-thread
    /// adapter uses this to suppress its own rendering.
    attached: Arc<AtomicBool>,
}

impl RenderHost {
    /// Whether a render-owned component has been attached and is now the
    /// visual authority on the render thread.
    pub fn has_attached_component(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    /// Send the app's component factory to the render thread. `factory`
    /// executes on the render thread and must call the generated `App::new()`
    /// *there* (after seeding a headless platform via `RenderMirrorPlatform`).
    /// The factory is re-runnable: after a pause/suspend cycle it is invoked
    /// again on the next window configuration so the mirror surface can be
    /// re-created on this thread.
    ///
    /// Once attached the UI-thread render path is suppressed and the screen
    /// is produced solely by this thread.
    pub fn attach_component<F>(&self, factory: F)
    where
        F: Fn() -> Box<dyn Any> + Send + 'static,
    {
        self.attached.store(true, Ordering::Relaxed);
        let _ = self.sender.send(RenderMessage::AttachComponent { factory: Box::new(factory) });
    }

    /// Ask the render thread to repaint its mirror component and present the
    /// frame.
    pub fn request_redraw(&self) {
        let _ = self.sender.send(RenderMessage::RequestRedraw);
    }

    /// Hand a translated input event to the render thread and wait for its
    /// verdict.
    ///
    /// Only the UI thread reads the Android input queue, but the control tree
    /// is on the render thread, so the event has to be delivered there for the
    /// hit testing, the focus handling and the item callbacks to run against
    /// the tree that is actually on screen. The verdict tells the UI thread
    /// whether the widget tree consumed the event, which is what Android needs
    /// to decide about its own back gesture and IME handling.
    ///
    /// The round trip costs what dispatching on the UI thread used to cost:
    /// the render thread is a plain receive loop with no other blocking
    /// caller, so it replies as soon as it finishes the frame in flight.
    pub(crate) fn forward_input(&self, event: WindowEvent) -> WindowEventDispatchResult {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = self
            .sender
            .send(RenderMessage::ForwardInput { event: Box::new(event), response: Some(tx) });
        rx.recv().unwrap_or(WindowEventDispatchResult::Rejected)
    }

    /// Hand a translated input event to the render thread without waiting for
    /// an answer. For the events whose outcome the UI thread has no use for.
    pub(crate) fn forward_input_detached(&self, event: WindowEvent) {
        let _ = self
            .sender
            .send(RenderMessage::ForwardInput { event: Box::new(event), response: None });
    }

    /// Configure the render thread with the native window + initial size.
    pub(crate) fn submit_configure(
        &self,
        window: ConfigureWindow,
        width: u32,
        height: u32,
        scale_factor: f32,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) {
        let _ = self.sender.send(RenderMessage::Configure {
            window,
            width,
            height,
            scale_factor,
            requested_graphics_api,
        });
    }

    /// Notify the render thread that the native window surface was resized, or
    /// that its density changed. `scale_factor` lets the mirror layout reflow
    /// even when the size itself did not change.
    pub(crate) fn submit_resize(&self, width: u32, height: u32, scale_factor: f32) {
        let _ = self.sender.send(RenderMessage::Resize { width, height, scale_factor });
    }

    /// Ask the render thread to release the native window surface (activity
    /// paused / window destroyed).
    pub(crate) fn submit_suspend(&self) {
        let _ = self.sender.send(RenderMessage::Suspend);
    }
}

/// Message protocol (UI thread → render thread).
pub(crate) enum RenderMessage {
    Configure {
        window: ConfigureWindow,
        width: u32,
        height: u32,
        scale_factor: f32,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    },
    Resize {
        width: u32,
        height: u32,
        scale_factor: f32,
    },
    AttachComponent {
        factory: Box<dyn Fn() -> Box<dyn Any> + Send>,
    },
    RequestRedraw,
    /// An input event, already translated from the Android input queue by the
    /// UI thread.
    ///
    /// The UI thread is the only side that reads the input queue, but the
    /// control tree lives on the render thread, so the hit testing, the focus
    /// handling and the item callbacks have to run there.
    ForwardInput {
        /// The event, in logical window coordinates.
        event: Box<WindowEvent>,
        /// Android decides whether to run its own default handling (the back
        /// gesture, the IME) from what the widget tree answers, so the sender
        /// asks for the verdict. `None` when the caller has no use for it, as
        /// for a pointer move.
        response: Option<std::sync::mpsc::SyncSender<WindowEventDispatchResult>>,
    },
    Suspend,
}

thread_local! {
    /// The `SkiaRenderer` currently bound to the native window. Created on
    /// the render thread in `Configure` and consumed by
    /// `RenderMirrorPlatform::create_window_adapter`. Cleared on `Suspend`
    /// to release the surface.
    static RENDERER_SLOT: RefCell<Option<Rc<SkiaRenderer>>> = const { RefCell::new(None) };
    /// The adapter created by the headless platform during
    /// `create_window_adapter`, so `RenderCore` can keep it alive after
    /// `App::new()` returns.
    static HEADLESS_ADAPTER_SLOT: RefCell<Option<Rc<RenderWindowAdapter>>> =
        const { RefCell::new(None) };
    /// Set by [`RenderWindowAdapter::request_redraw`] when the core dirties a
    /// rendered property. The render loop consumes it after applying an input
    /// event, so a pointer move that changes nothing costs no re-encode.
    static MIRROR_NEEDS_REDRAW: Cell<bool> = const { Cell::new(false) };
}

/// Minimal window adapter for the render-thread component. The Slint runtime
/// queries it for the window geometry; actual compositing to the
/// `ANativeWindow` is handled by the render loop through the owned
/// [`SkiaRenderer`].
struct RenderWindowAdapter {
    window: SlintApiWindow,
    size: Cell<PhysicalSize>,
    renderer: Rc<SkiaRenderer>,
}

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

    fn renderer(&self) -> &dyn i_slint_core::platform::Renderer {
        &*self.renderer
    }

    fn request_redraw(&self) {
        // The core marks a rendered property dirty. The render loop has no
        // event source of its own to notice that, so record it and repaint
        // once the message that caused it has been handled.
        MIRROR_NEEDS_REDRAW.with(|dirty| dirty.set(true));
    }
}

/// Headless platform for the render thread. Only `create_window_adapter` is
/// implemented; the rest is handled by default trait methods.
struct RenderMirrorPlatform;

impl Platform for RenderMirrorPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let renderer: Rc<SkiaRenderer> = RENDERER_SLOT
            .with(|slot| slot.borrow().as_ref().cloned())
            .ok_or(PlatformError::NoPlatform)?;
        let adapter: Rc<RenderWindowAdapter> =
            Rc::new_cyclic(|weak: &Weak<RenderWindowAdapter>| {
                let window = SlintApiWindow::new(weak.clone() as Weak<dyn WindowAdapter>);
                RenderWindowAdapter {
                    window,
                    size: Cell::new(PhysicalSize::default()),
                    renderer: renderer.clone(),
                }
            });
        let adapter_dyn: Rc<dyn WindowAdapter> = adapter.clone();
        renderer.set_window_adapter(&adapter_dyn);
        HEADLESS_ADAPTER_SLOT.with(|slot| *slot.borrow_mut() = Some(adapter.clone()));
        Ok(adapter_dyn)
    }
}

/// Receive half of the render-thread protocol, runs on the render thread.
struct RenderCore {
    rx: mpsc::Receiver<RenderMessage>,
}

impl RenderCore {
    fn new(rx: mpsc::Receiver<RenderMessage>) -> Self {
        Self { rx }
    }

    fn run(&mut self) {
        // Render-thread state. `factory` is retained across suspend so the
        // mirror can be re-created when the native window comes back.
        let mut factory: Option<Box<dyn Fn() -> Box<dyn Any> + Send>> = None;
        let mut render_component: Option<Box<dyn Any>> = None;
        let mut render_window_adapter: Option<Rc<RenderWindowAdapter>> = None;
        let mut current_size = PhysicalSize::default();
        let mut current_scale_factor = 1.0;

        while let Ok(msg) = self.rx.recv() {
            match msg {
                RenderMessage::Configure {
                    window,
                    width,
                    height,
                    scale_factor,
                    requested_graphics_api,
                } => {
                    let size = PhysicalSize { width, height };
                    current_size = size;
                    current_scale_factor = scale_factor;
                    if let Some(adapter) = &render_window_adapter {
                        // The window came back or changed: keep the mirror
                        // tree and swap the surface it draws into.
                        if let Err(e) =
                            rebind_renderer(&adapter.renderer, window, size, requested_graphics_api)
                        {
                            eprintln!("dualslint render thread: surface re-init failed: {e}");
                        } else {
                            adapter_configured(adapter, size, scale_factor);
                        }
                    } else {
                        // First window: create the renderer on this thread and
                        // bind it to the native window.
                        let renderer =
                            match new_bound_renderer(window, size, requested_graphics_api) {
                                Ok(renderer) => renderer,
                                Err(e) => {
                                    eprintln!("dualslint render thread: surface init failed: {e}");
                                    continue;
                                }
                            };
                        RENDERER_SLOT.with(|slot| *slot.borrow_mut() = Some(renderer));
                        if let Some(f) = &factory {
                            attach_component(
                                f,
                                &mut render_window_adapter,
                                &mut render_component,
                                size,
                                scale_factor,
                            );
                        }
                    }
                }
                RenderMessage::Resize { width, height, scale_factor } => {
                    current_size = PhysicalSize { width, height };
                    current_scale_factor = scale_factor;
                    if let Some(adapter) = &render_window_adapter {
                        adapter_configured(adapter, current_size, current_scale_factor);
                    }
                }
                RenderMessage::AttachComponent { factory: f } => {
                    // Seed the headless platform on this thread (ignore
                    // AlreadySet — set_platform succeeds as long as this
                    // thread's GLOBAL_CONTEXT is still free).
                    let _ = i_slint_core::platform::set_platform(Box::new(RenderMirrorPlatform));
                    factory = Some(f);
                    if render_window_adapter.is_none()
                        && RENDERER_SLOT.with(|s| s.borrow().is_some())
                    {
                        let f = factory.as_ref().unwrap();
                        attach_component(
                            f,
                            &mut render_window_adapter,
                            &mut render_component,
                            current_size,
                            current_scale_factor,
                        );
                    }
                }
                RenderMessage::RequestRedraw => {
                    if let Some(adapter) = &render_window_adapter {
                        present(adapter);
                    }
                    // No mirror attached: the UI thread renders itself
                    // (upstream `SkiaRenderer` path), so there is nothing to
                    // present here.
                }
                RenderMessage::ForwardInput { event, response } => {
                    // The control tree is ours, so the hit testing, the focus
                    // handling and the item callbacks run here rather than on
                    // the UI thread.
                    let mut verdict = WindowEventDispatchResult::Rejected;
                    if let Some(adapter) = &render_window_adapter {
                        verdict = i_slint_core::api::Window::dispatch_event_with_result(
                            adapter.window(),
                            *event,
                        )
                        .unwrap_or(WindowEventDispatchResult::Rejected);
                        // Repaint only when the event actually dirtied a
                        // rendered property, so a pointer move that changes
                        // nothing stays free.
                        if MIRROR_NEEDS_REDRAW.with(|dirty| dirty.replace(false)) {
                            present(adapter);
                        }
                    }
                    if let Some(response) = response {
                        let _ = response.send(verdict);
                    }
                }
                RenderMessage::Suspend => {
                    // Release the surface + context and the mirror tree. The
                    // factory and geometry are retained so the mirror can be
                    // re-created when the native window reappears.
                    RENDERER_SLOT.with(|slot| *slot.borrow_mut() = None);
                    HEADLESS_ADAPTER_SLOT.with(|slot| *slot.borrow_mut() = None);
                    render_window_adapter = None;
                    render_component = None;
                }
            }
        }
    }
}

/// Creates the renderer's surface for the first configured window.
///
/// On `aa-06` the surface rasterises into the window's CPU buffer, so the
/// renderer is built around it directly; the concrete `NativeWindow` cannot be
/// recovered from an erased `HasWindowHandle` later on.
#[cfg(feature = "aa-06")]
fn new_bound_renderer(
    window: ConfigureWindow,
    size: PhysicalSize,
    _requested_graphics_api: Option<RequestedGraphicsAPI>,
) -> Result<Rc<SkiaRenderer>, PlatformError> {
    Ok(Rc::new(SkiaRenderer::new_with_surface(
        &SkiaSharedContext::default(),
        Box::new(NativeWindowSurface::new(window, size)),
    )))
}

/// Points an existing renderer at a reconfigured window.
#[cfg(feature = "aa-06")]
fn rebind_renderer(
    renderer: &Rc<SkiaRenderer>,
    window: ConfigureWindow,
    size: PhysicalSize,
    _requested_graphics_api: Option<RequestedGraphicsAPI>,
) -> Result<(), PlatformError> {
    // Swapping the surface releases the previous one and its caches.
    renderer.set_surface(Box::new(NativeWindowSurface::new(window, size)));
    Ok(())
}

#[cfg(not(feature = "aa-06"))]
fn new_bound_renderer(
    window: ConfigureWindow,
    size: PhysicalSize,
    requested_graphics_api: Option<RequestedGraphicsAPI>,
) -> Result<Rc<SkiaRenderer>, PlatformError> {
    let renderer = new_skia_renderer();
    renderer.set_window_handle(
        window,
        Arc::new(DummyDisplayHandle),
        size,
        requested_graphics_api,
        false,
    )?;
    Ok(renderer)
}

#[cfg(not(feature = "aa-06"))]
fn rebind_renderer(
    renderer: &Rc<SkiaRenderer>,
    window: ConfigureWindow,
    size: PhysicalSize,
    requested_graphics_api: Option<RequestedGraphicsAPI>,
) -> Result<(), PlatformError> {
    renderer.set_window_handle(
        window,
        Arc::new(DummyDisplayHandle),
        size,
        requested_graphics_api,
        false,
    )
}

#[cfg(not(feature = "aa-06"))]
fn new_skia_renderer() -> Rc<SkiaRenderer> {
    #[cfg(not(any(feature = "unstable-wgpu-29", feature = "unstable-wgpu-30")))]
    return Rc::new(SkiaRenderer::default(&SkiaSharedContext::default()));
    #[cfg(all(feature = "unstable-wgpu-29", not(feature = "unstable-wgpu-30")))]
    return Rc::new(SkiaRenderer::default_wgpu_29(&SkiaSharedContext::default()));
    #[cfg(feature = "unstable-wgpu-30")]
    return Rc::new(SkiaRenderer::default_wgpu_30(&SkiaSharedContext::default()));
}

/// Run the mirror factory (the app's `App::new()`) on the render thread, wire
/// the resulting mirror window to the bound renderer, then present.
fn attach_component(
    factory: &(dyn Fn() -> Box<dyn Any> + Send),
    render_window_adapter: &mut Option<Rc<RenderWindowAdapter>>,
    render_component: &mut Option<Box<dyn Any>>,
    size: PhysicalSize,
    scale_factor: f32,
) {
    *render_component = Some(factory());
    *render_window_adapter = HEADLESS_ADAPTER_SLOT.with(|slot| slot.borrow().as_ref().cloned());
    if let Some(adapter) = render_window_adapter {
        adapter_configured(adapter, size, scale_factor);
    }
}

/// Apply the native window geometry to the mirror adapter and present it.
fn adapter_configured(adapter: &RenderWindowAdapter, size: PhysicalSize, scale_factor: f32) {
    adapter.size.set(size);
    adapter.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
    adapter.window.dispatch_event(WindowEvent::Resized { size: size.to_logical(scale_factor) });
    present(adapter);
}

/// Present the mirror frame to the native window surface.
fn present(adapter: &RenderWindowAdapter) {
    if let Err(e) = adapter.renderer.render() {
        eprintln!("dualslint render thread: present failed: {e}");
    }
    // Everything the tree had to show is on screen now, so a dirtiness
    // reported from here on belongs to the next frame.
    MIRROR_NEEDS_REDRAW.with(|dirty| dirty.set(false));
}

/// The render thread host, once started.
static GLOBAL_RENDER_HOST: OnceLock<RenderHost> = OnceLock::new();

/// Start the render thread (a no-op if it is already running). Called by the
/// android backend when the platform is created.
pub(crate) fn ensure_render_thread() {
    GLOBAL_RENDER_HOST.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let host = RenderHost { sender: tx, attached: Arc::new(AtomicBool::new(false)) };
        std::thread::Builder::new()
            .name("slint android render thread".into())
            .spawn(move || {
                let mut core = RenderCore::new(rx);
                core.run();
            })
            .expect("failed to spawn the slint android render thread");
        host
    });
}

/// Access the render thread host. Returns `None` until the android backend
/// has been configured ([`ensure_render_thread`]).
pub fn host() -> Option<RenderHost> {
    GLOBAL_RENDER_HOST.get().cloned()
}

/// The side that currently owns the control tree, for input routing.
///
/// Returns the render host only once an application has attached a
/// render-owned component; until then the UI thread keeps its own tree and
/// keeps handling input the upstream way. The window adapter asks this before
/// every input event instead of testing the flag itself, so the "who owns the
/// controls" decision has a single answer.
pub(crate) fn input_owner() -> Option<&'static RenderHost> {
    let host = GLOBAL_RENDER_HOST.get()?;
    host.has_attached_component().then_some(host)
}

/// Request a window repaint through the render thread.
///
/// This is the replacement for the removed [`Window::request_redraw`] entry
/// point: the render thread owns the mirror component and re-presents the
/// frame when asked, regardless of the UI-side window state. It is safe to
/// call from any thread.
///
/// This is a no-op when the render thread was never started, or when no
/// mirror component is attached (in which case the UI thread renders itself).
pub fn request_redraw() {
    if let Some(host) = GLOBAL_RENDER_HOST.get() {
        host.request_redraw();
    }
}

/// A dummy display handle that's `Send + Sync`. Required by wgpu, but
/// harmless as `raw_window_handle::AndroidDisplayHandle` is an empty struct.
#[cfg(not(feature = "aa-06"))]
struct DummyDisplayHandle;
#[cfg(not(feature = "aa-06"))]
impl raw_window_handle::HasDisplayHandle for DummyDisplayHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}
