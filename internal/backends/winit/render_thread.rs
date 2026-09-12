// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// This file is part of the dualslint fork (https://github.com/akenejie/dualslint),
// a fork of slint-ui/slint. This file is new in the fork; where it builds on the
// slint code base, those parts remain under slint's own license terms. The
// fork's modifications and additions are AGPL-3.0-only, Copyright © akenejie.

//! DESIGN REWRITE (fork):
//!
//! Full off-UI-thread rendering. The UI thread owns the winit event loop and
//! the native window handle (HWND); the render thread owns the entire Slint
//! scene graph, its GL context and the presenter.
//!
//! The UI thread forwards winit events and user callbacks over an mpsc channel.
//! On the render thread the application builds a [`SlintContext`] with a
//! [`RenderThreadPlatform`], constructs its window with `new_with_context(ctx)`,
//! registers callbacks and finally calls `ctx.run_event_loop()`. The platform:
//!   1. builds a raw WGL context on the received HWND,
//!   2. constructs a [`FemtoVGRenderer`] bound to that context,
//!   3. hands out a `WindowAdapter` wrapping that renderer,
//!   4. pumps the channel, feeding events into the scene graph and calling
//!      `renderer.render()` whenever a repaint is requested (via any route:
//!      `WindowAdapter::request_redraw`, a forwarded event, or a deferred
//!      closure). The loop blocks on the channel while idle - no busy polling.
//!
//! The UI thread never touches the scene graph nor any GL state; it only
//! produces winit events (no drawing). All drawing happens on the render thread.

use core::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use i_slint_core::api::PhysicalSize;
use i_slint_core::graphics::euclid;
use i_slint_core::graphics::{
    BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, Image, IntSize,
};
use i_slint_core::input::{BackendMouseEvent, InternalKeyEvent, KeyEvent, KeyEventType, TouchPhase};
use i_slint_core::items::PointerEventButton;
use i_slint_core::lengths::LogicalPoint;
use i_slint_core::platform::{EventLoopProxy, Platform, PlatformError, WindowEvent};
use i_slint_core::renderer::RendererSealed;
use i_slint_core::window::{WindowAdapter, WindowInner};
use i_slint_core::InternalToken;
use glow::HasContext;
use i_slint_renderer_femtovg::opengl::{OpenGLBackend, OpenGLInterface};
use i_slint_renderer_femtovg::FemtoVGRenderer;

/// Messages sent from the UI thread to the render thread's event loop.
pub enum RenderMessage {
    /// A raw winit window event to feed into the scene graph.
    Winit(winit::event::WindowEvent),
    /// A user callback scheduled via [`EventLoopProxy::invoke_from_event_loop`].
    User(Box<dyn FnOnce() + Send>),
    /// Request a redraw from any thread (UI or render).
    Redraw,
    /// A deferred pixel-paint closure: run on the render thread with exclusive
    /// `&mut PixelTarget` access to the backing texture. The double buffer is
    /// (re)created to `width` x `height` before the closure runs, so the app
    /// only states the desired size and paints.
    Paint {
        /// Desired double-buffer width in physical pixels.
        width: u32,
        /// Desired double-buffer height in physical pixels.
        height: u32,
        /// The application's paint closure; run on the render thread with the
        /// whole (exclusively borrowed) target.
        f: Box<dyn FnOnce(&mut PixelTarget) + Send>,
    },
    /// Quit the loop.
    Quit,
}

/// The send half, shared with the UI thread (and the event-loop proxy) so winit
/// events and deferred closures can be pushed onto the render thread's loop.
#[derive(Clone)]
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,
}

impl RenderHost {
    /// Forward a raw winit window event to the render thread's scene graph.
    pub fn send_winit(&self, event: winit::event::WindowEvent) {
        let _ = self.sender.send(RenderMessage::Winit(event));
    }

    /// Schedule an arbitrary closure to run on the render thread.
    pub fn send_user(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(RenderMessage::User(Box::new(f)));
    }

    /// Ask the render thread's event loop to quit.
    pub fn send_quit(&self) {
        let _ = self.sender.send(RenderMessage::Quit);
    }

    /// Ask the render thread to repaint.
    pub fn send_redraw(&self) {
        let _ = self.sender.send(RenderMessage::Redraw);
    }

    /// Schedule a pixel-paint closure to run on the render thread with
    /// exclusive `&mut PixelTarget` access. The backing texture is (re)created
    /// to `width` x `height` beforehand if needed. Safe from any thread (e.g. a
    /// computation worker); the closure itself only ever runs on the render
    /// thread, where the double buffer is lent out as a single `&mut` borrow.
    pub fn paint<F>(&self, width: u32, height: u32, f: F)
    where
        F: FnOnce(&mut PixelTarget) + Send + 'static,
    {
        let _ = self
            .sender
            .send(RenderMessage::Paint { width, height, f: Box::new(f) });
    }
}

/// Create the render-thread channel. The UI thread keeps `host` to feed events;
/// the render thread creates a [`RenderThreadPlatform`] from `host` and `rx`.
pub fn channel() -> (RenderHost, mpsc::Receiver<RenderMessage>) {
    let (tx, rx) = mpsc::channel();
    (RenderHost { sender: tx }, rx)
}

/// CPU side of the persistent backing texture. The application rasterizes
/// directly into the pixel buffer inside a [`RenderHost::paint`]
/// closure, marks the affected rectangles dirty, and calls [`Self::present`];
/// the render loop then advertises a new frame (re-binding the image item via
/// the registered sink) and uploads the dirty rectangles to a double-buffered
/// GL texture just before drawing. The buffer is tightly packed RGBA8,
/// row-major, `width * height * 4` bytes, row 0 = top.
///
/// This type is *not* `Send` and lives on the render thread only. Every
/// changing access (`bytes_mut`, `mark_dirty*`, `present`) requires an
/// exclusive `&mut` borrow, so the double buffer is lent to the application for
/// the lifetime of one paint closure; a second (or concurrent) borrow is a
/// compile error.
pub struct PixelTarget {
    gl: Rc<glow::Context>,
    textures: [glow::Texture; 2],
    texture_ids: [NonZeroU32; 2],
    width: u32,
    height: u32,
    /// Number of frames advertised so far; parity picks the alternating texture.
    publish_seq: u32,
    bytes: Vec<u8>,
    dirty: Vec<[u32; 4]>,
    /// Set by `present()` inside a paint closure; consumed by the render loop
    /// right before the next draw to advertise the pending frame.
    presented: bool,
}

impl PixelTarget {
    fn new(gl: Rc<glow::Context>, width: u32, height: u32) -> Result<Self, PlatformError> {
        if width == 0 || height == 0 {
            return Err("render thread: pixel target must be non-empty".into());
        }
        // The GL context is current on the render thread (see make_renderer).
        // Two textures back one byte buffer; the render loop alternates the
        // upload between them so each advertised frame gets its own texture
        // name (required for Slint to re-render the changed image item).
        let textures = [0usize, 1].map(|_| {
            unsafe {
                let t = gl
                    .create_texture()
                    .map_err(|_| "render thread: glCreateTexture failed")?;
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                gl.bind_texture(glow::TEXTURE_2D, None);
                Ok::<glow::Texture, PlatformError>(t)
            }
        });
        let textures = match textures {
            [a, b] => match (a, b) {
                (Ok(a), Ok(b)) => [a, b],
                (Err(e), _) | (_, Err(e)) => return Err(e),
            },
        };
        Ok(Self {
            gl,
            textures,
            texture_ids: [textures[0].0, textures[1].0],
            width,
            height,
            publish_seq: 0,
            bytes: vec![0u8; width as usize * height as usize * 4],
            dirty: Vec::new(),
            presented: false,
        })
    }

    /// Upload the pending dirty rectangles to every GL texture backing this
    /// target. Runs on the render thread with the WGL context current, just
    /// before `render()`.
    ///
    /// Each rectangle is uploaded as its own `glTexSubImage2D` into *all* the
    /// alternating textures, keeping them identical. A caller that paints only
    /// the pixels changing between frames
    /// into `bytes_mut()` and registers the touched rectangles with
    /// `mark_dirty` therefore always lands on a texture that already holds the
    /// full cumulative picture - whichever one the next published frame
    /// advertises to Slint.
    fn upload_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let rects = std::mem::take(&mut self.dirty);

        let bytes = &self.bytes;
        let gl = &self.gl;
        let stride = self.width as usize * 4;
        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, self.width as i32);
            for &[x, y, w, h] in rects.iter() {
                // Slice must span `h` rows of `stride` bytes from `offset`;
                // UNPACK_ROW_LENGTH lets GL stride over the full buffer rows
                // while sourcing only the sub-rectangle starting at `offset`.
                let offset = y as usize * stride + x as usize * 4;
                let len = (h as usize - 1).saturating_mul(stride) + w as usize * 4;
                let src = &bytes[offset..offset + len];
                for target in self.textures {
                    gl.bind_texture(glow::TEXTURE_2D, Some(target));
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        x as i32,
                        y as i32,
                        w as i32,
                        h as i32,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(src)),
                    );
                }
            }
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// The width of the byte buffer/texture in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }
    /// The height of the byte buffer/texture in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Exclusive, direct access to the CPU pixel buffer (RGBA8,
    /// `width * height * 4` bytes, row 0 = top). Available only for the
    /// lifetime of the enclosing `paint` closure; the borrow ends here, so the
    /// buffer can never be held across `mark_dirty` / `present` (a second
    /// borrow is a compile error).
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
    /// Mark `(x, y, w, h)` (pixels) as changed so the render loop re-uploads it.
    pub fn mark_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let x = x.min(self.width);
        let y = y.min(self.height);
        let w = (x + w).min(self.width) - x;
        let h = (y + h).min(self.height) - y;
        if w == 0 || h == 0 {
            return;
        }
        self.dirty.push([x, y, w, h]);
    }
    /// Mark the whole texture as changed.
    pub fn mark_whole_dirty(&mut self) {
        self.mark_dirty(0, 0, self.width, self.height);
    }
    /// Advertise the pixels painted since the last `present()` to the screen.
    /// The item's `Image` is re-bound via the registered sink and the pending
    /// bytes are uploaded right before the next frame is drawn. Requiring
    /// `&mut self` guarantees that no pixel-buffer borrow can outlive the paint
    /// closure.
    pub fn present(&mut self) {
        self.presented = true;
    }
    /// Advertise the newest pending frame to Slint if `present()` was called,
    /// handing the item the freshly-created `Image` via the sink. Runs on the
    /// render thread right before `upload_dirty` + `render()`.
    fn present_pending(&mut self, sink: &Option<Box<dyn Fn(Image) + Send + 'static>>) {
        if !self.presented {
            return;
        }
        self.presented = false;
        let n = self.publish_seq;
        self.publish_seq += 1;
        let image = unsafe {
            BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.texture_ids[n as usize & 1],
                IntSize::new(self.width, self.height),
            )
            .origin(BorrowedOpenGLTextureOrigin::TopLeft)
            .build()
        };
        if let Some(sink) = sink {
            sink(image);
        }
    }
}

impl Drop for PixelTarget {
    fn drop(&mut self) {
        // The WGL context is still current on the render thread when a target is
        // replaced/removed, so the deletion is valid.
        for texture in self.textures {
            unsafe { self.gl.delete_texture(texture) };
        }
    }
}

/// Shared, render-thread-owned state. Both the [`RenderThreadPlatform`] and the
/// [`HwndWindowAdapter`] hold a `Rc` to this so `run_event_loop` can reach the
/// renderer and window that are created during `create_window_adapter`.
struct RenderContext {
    rx: mpsc::Receiver<RenderMessage>,
    host: RenderHost,
    /// Redraw requested by the scene graph (via the adapter).
    redraw: Cell<bool>,
    cursor_pos: Cell<LogicalPoint>,
    pressed: Cell<bool>,
    size: RefCell<PhysicalSize>,
    /// Set by `create_window_adapter` (render thread) so the loop can render.
    renderer: RefCell<Option<Weak<FemtoVGRenderer<OpenGLBackend>>>>,
    window_adapter: RefCell<Option<Weak<dyn WindowAdapter>>>,
/// glow context built on the render thread from the WGL proc-address loader;
    /// used to create/update the persistent [`PixelTarget`] texture.
    glow: RefCell<Option<Rc<glow::Context>>>,
    /// The persistent backing texture, owned by the render thread. The single
    /// owner means every app-side pixel write is a `&mut` borrow routed through
    /// [`RenderMessage::Paint`]; no lock/atomics needed.
    pixel_target: RefCell<Option<PixelTarget>>,
    /// App-registered callback re-binding the image item to a freshly advertised
    /// frame (`ui.set_<image>(img)` on a weak handle).
    image_sink: RefCell<Option<Box<dyn Fn(Image) + Send + 'static>>>,
}

impl RenderContext {
    fn new(host: RenderHost, rx: mpsc::Receiver<RenderMessage>, size: PhysicalSize) -> Self {
        Self {
            rx,
            host,
            redraw: Cell::new(false),
            cursor_pos: Cell::new(LogicalPoint::new(0., 0.)),
            pressed: Cell::new(false),
            size: RefCell::new(size),
            renderer: RefCell::new(None),
            window_adapter: RefCell::new(None),
            glow: RefCell::new(None),
            pixel_target: RefCell::new(None),
            image_sink: RefCell::new(None),
        }
    }
}

/// The WGL context, bound to the HWND. All methods are only ever called while
/// the render thread is current.
#[cfg(target_os = "windows")]
struct WglInterface {
    hdc: isize,
    hgl: isize,
}

#[cfg(target_os = "windows")]
unsafe impl OpenGLInterface for WglInterface {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let r = unsafe {
            windows::Win32::Graphics::OpenGL::wglMakeCurrent(
                windows::Win32::Graphics::Gdi::HDC(self.hdc as *mut _),
                windows::Win32::Graphics::OpenGL::HGLRC(self.hgl as *mut _),
            )
        };
        r?;
        Ok(())
    }
    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let r = unsafe {
            windows::Win32::Graphics::OpenGL::SwapBuffers(
                windows::Win32::Graphics::Gdi::HDC(self.hdc as *mut _),
            )
        };
        r?;
        Ok(())
    }
    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const std::ffi::c_void {
        unsafe {
            let c = std::ffi::CString::new(name.to_bytes()).unwrap_or_default();
            // wglGetProcAddress only resolves extension functions (and GL >= 1.2 core
            // functions on most drivers). Legacy GL 1.1 entry points such as glGetString,
            // glClear and glViewport must be fetched from opengl32.dll instead, otherwise
            // glow's calls panic with "called X but it was not loaded".
            if let Some(f) = windows::Win32::Graphics::OpenGL::wglGetProcAddress(
                windows::core::PCSTR(c.as_ptr() as *const u8),
            ) {
                return f as *const _;
            }
            // Fall back to fetching legacy GL 1.1 functions from opengl32.dll.
            unsafe extern "system" {
                fn GetModuleHandleW(lpmodule: *const u16) -> isize;
                fn GetProcAddress(hmodule: isize, lpname: *const u8) -> *const std::ffi::c_void;
            }
            const OPENGL32_W: &[u16] = &[
                0x6f, 0x70, 0x65, 0x6e, 0x67, 0x6c, 0x33, 0x32, 0x2e, 0x64, 0x6c, 0x6c, 0x00,
            ];
            let dll = GetModuleHandleW(OPENGL32_W.as_ptr());
            if dll == 0 {
                return std::ptr::null();
            }
            let proc = GetProcAddress(dll, c.as_ptr() as *const u8);
            if proc.is_null() {
                std::ptr::null()
            } else {
                proc as *const _
            }
        }
    }
}

pub(crate) struct HwndWindowAdapter {
    window: i_slint_core::api::Window,
    renderer: Rc<FemtoVGRenderer<OpenGLBackend>>,
    ctx: Rc<RenderContext>,
    /// App-registered filter invoked before every winit window event, mirroring
    /// `WinitWindowAdapter::window_event_filter` from the upstream backend.
    pub(crate) window_event_filter:
        Cell<Option<Box<dyn FnMut(&i_slint_core::api::Window, &winit::event::WindowEvent) -> crate::EventResult>>>,
}

impl HwndWindowAdapter {
    fn new(ctx: Rc<RenderContext>, hwnd: isize, _size: PhysicalSize) -> Result<Rc<Self>, PlatformError> {
        let (renderer, gl) = make_renderer(hwnd)?;
        // Keep a glow context around so the render loop can create and update the
        // persistent [`PixelTarget`] texture without going through femtovg.
        *ctx.glow.borrow_mut() = Some(gl);
        let renderer = Rc::new(renderer);
        Ok(Rc::new_cyclic(|weak: &Weak<HwndWindowAdapter>| {
            let window = i_slint_core::api::Window::new(weak.clone() as Weak<dyn WindowAdapter>);
            HwndWindowAdapter {
                window,
                renderer: renderer.clone(),
                ctx: ctx.clone(),
                window_event_filter: Default::default(),
            }
        }))
    }
}

#[cfg(not(target_os = "windows"))]
fn make_renderer(_hwnd: isize) -> Result<(FemtoVGRenderer<OpenGLBackend>, Rc<glow::Context>), PlatformError> {
    Err("render thread: non-windows not yet implemented".into())
}

#[cfg(target_os = "windows")]
fn make_renderer(hwnd: isize) -> Result<(FemtoVGRenderer<OpenGLBackend>, Rc<glow::Context>), PlatformError> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
    use windows::Win32::Graphics::OpenGL::{
        wglCreateContext, wglMakeCurrent, ChoosePixelFormat, SetPixelFormat, PIXELFORMATDESCRIPTOR,
        PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
    };
    let hwnd_raw = HWND(hwnd as *mut _);
    let hdc = unsafe { GetDC(Some(hwnd_raw)) };
    if hdc.is_invalid() {
        return Err("GetDC failed".into());
    }
    unsafe {
        let mut pfd: PIXELFORMATDESCRIPTOR = std::mem::zeroed();
        pfd.nSize = std::mem::size_of_val(&pfd) as u16;
        pfd.nVersion = 1;
        pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
        pfd.iPixelType = PFD_TYPE_RGBA;
        pfd.cColorBits = 32;
        pfd.cDepthBits = 24;
        pfd.cStencilBits = 8;
        pfd.iLayerType = PFD_MAIN_PLANE.0 as u8;
        let pf = ChoosePixelFormat(hdc, &pfd);
        if pf != 0 {
            let _ = SetPixelFormat(hdc, pf, &pfd);
        }
    }
    let hgl = unsafe { wglCreateContext(hdc) }.map_err(|e| {
        unsafe { ReleaseDC(Some(hwnd_raw), hdc) };
        windows_error(e)
    })?;
    unsafe {
        wglMakeCurrent(hdc, hgl).map_err(|e| {
            let _ = windows::Win32::Graphics::OpenGL::wglDeleteContext(hgl);
            let _ = ReleaseDC(Some(hwnd_raw), hdc);
            windows_error(e)
        })?;
    }
    let wgl = WglInterface { hdc: hdc.0 as isize, hgl: hgl.0 as isize };
    // Build an auxiliary glow context that shares the WGL loader. femtovg builds
    // its own privately; this one is used for the persistent PixelTarget texture.
    // The WGL context is current here (set right above) and stays current on this
    // thread for the lifetime of the renderer.
    let gl = unsafe { glow::Context::from_loader_function_cstr(|name| wgl.get_proc_address(name)) };
    let renderer = FemtoVGRenderer::new(wgl)?;
    Ok((renderer, Rc::new(gl)))
}

#[cfg(target_os = "windows")]
fn windows_error(e: windows::core::Error) -> PlatformError {
    PlatformError::from(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
}

impl WindowAdapter for HwndWindowAdapter {
    fn window(&self) -> &i_slint_core::api::Window {
        &self.window
    }
    fn size(&self) -> PhysicalSize {
        *self.ctx.size.borrow()
    }
    fn set_size(&self, _size: i_slint_core::api::WindowSize) {}
    fn renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &*self.renderer
    }
    fn request_redraw(&self) {
        self.ctx.host.send_redraw();
    }
    fn internal(&self, _: InternalToken) -> Option<&dyn i_slint_core::window::WindowAdapterInternal> {
        Some(self)
    }
}

impl i_slint_core::window::WindowAdapterInternal for HwndWindowAdapter {}

/// The [`Platform`] the render thread provides to a `SlintContext`. Construct it
/// on the render thread with the `rx` half from [`channel()`] and the HWND/size
/// handed over from the UI thread, then pass it to `SlintContext::new`.
#[derive(Clone)]
pub struct RenderThreadPlatform {
    ctx: Rc<RenderContext>,
    hwnd: isize,
}

impl RenderThreadPlatform {
    /// `hwnd` is the native window handle created on the UI thread; `size` is
    /// the initial client size in physical pixels; `host` and `rx` come from
    /// [`channel()`].
    pub fn new(
        hwnd: isize,
        size: PhysicalSize,
        host: RenderHost,
        rx: mpsc::Receiver<RenderMessage>,
    ) -> Self {
        Self { ctx: Rc::new(RenderContext::new(host, rx, size)), hwnd }
    }

    /// Register how freshly advertised frames reach the scene graph:
    /// the application calls this once, on the render thread, with a closure
    /// that re-binds the image item - typically
    /// `ui.set_<image-property>(img)` on a weakly-captured component handle.
    /// After that the app only writes pixels and calls `RenderHost::paint`.
    pub fn set_image_sink<F>(&self, sink: F)
    where
        F: Fn(Image) + Send + 'static,
    {
        *self.ctx.image_sink.borrow_mut() = Some(Box::new(sink));
    }

    /// The [`RenderHost`] this platform runs under - can be cloned and used by
    /// other threads (e.g. a rendering worker) to route [`RenderHost::paint`]
    /// frames to the render thread.
    pub fn host(&self) -> RenderHost {
        self.ctx.host.clone()
    }

    /// (Re)create the persistent backing texture if it does not hold the given
    /// size yet. Runs on the render thread (the GL context is current); the
    /// loop calls this right before processing a [`RenderMessage::Paint`].
    fn ensure_pixel_target(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let gl = self.ctx.glow.borrow().clone();
        let Some(gl) = gl else { return };
        let has_size = self
            .ctx
            .pixel_target
            .borrow()
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height);
        if has_size {
            return;
        }
        match PixelTarget::new(gl, width, height) {
            Ok(target) => *self.ctx.pixel_target.borrow_mut() = Some(target),
            Err(e) => eprintln!("render thread: pixel target create: {e}"),
        }
    }
}

impl Platform for RenderThreadPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let size = *self.ctx.size.borrow();
        let adapter = HwndWindowAdapter::new(self.ctx.clone(), self.hwnd, size)?;
        // Wire the window adapter into the renderer's weak slots.
        let dyn_adapter: Rc<dyn WindowAdapter> = adapter.clone();
        adapter.renderer.set_window_adapter(&dyn_adapter);
        *self.ctx.window_adapter.borrow_mut() = Some(Rc::downgrade(&dyn_adapter));
        *self.ctx.renderer.borrow_mut() = Some(Rc::downgrade(&adapter.renderer));
        Ok(adapter)
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        // The scene graph's animation clock (`Instant::duration_since_start()`),
        // timers (`duration_until_next_timer_update()`) and the event-loop proxy
        // are resolved through i-slint-core's thread-local `GLOBAL_CONTEXT`.
        // Register this platform into that context now. Without it the clock
        // reads as `Duration::ZERO`, so property animations such as the widgets'
        // `animate background { duration: 150ms; }` transitions freeze at their
        // initial value and a toggled checkbox never repaints. The registered
        // clone shares this thread's `Rc<RenderContext>`, so the window and
        // renderer created via `MainWindow::new_with_context` remain the ones
        // the loop drives below.
        i_slint_core::context::with_global_context(
            || Ok(Box::new(self.clone()) as Box<dyn Platform + 'static>),
            |_| (),
        )?;

        // Purely reactive message handler: while there is nothing to do the loop
        // blocks on the channel (zero CPU). A repaint happens only when something
        // explicitly asks for one - a `Redraw` message (WM_PAINT-style, via
        // `WindowAdapter::request_redraw`), a winit event that invalidates the
        // scene (handled in `dispatch`), or a deferred closure from any thread
        // calling `request_redraw()`.
        //
        // The only periodic wake-up is the animation clock: property animations
        // (e.g. the widgets' `animate background { duration: 150ms; }` transitions)
        // advance when `update_timers_and_animations()` is called with a fresh time,
        // which only happens when the loop wakes. So while `has_active_animations()`
        // is true we wake at the stock backend's frame interval (16 ms) to tick the
        // clock. This is not a per-frame render poll and it ends by itself: the draw
        // of each frame is still driven by the scene graph's redraw tracker (a clock
        // tick changes the animated property -> the tracker stays dirty and a further
        // call to draw_contents() re-runs it, producing an updated frame), and once
        // the transition reaches its end value the tracker clears and no animation
        // remains -> no wake-up. With no animation and no pending timer the loop
        // sleeps indefinitely (86400 s timeout), exactly as
        // `duration_until_next_timer_update()`'s contract documents: "only go to
        // sleep if has_active_animations() returns false".
        loop {
            let window_adapter = self.window_adapter();
            let has_active_animations = window_adapter
                .as_ref()
                .is_some_and(|w| w.window().has_active_animations());

            let timer_timeout = i_slint_core::platform::duration_until_next_timer_update()
                .unwrap_or(std::time::Duration::from_secs(86400));
            let timeout = if has_active_animations {
                std::time::Duration::from_millis(16).min(timer_timeout)
            } else {
                timer_timeout
            };

            let msg = self.ctx.rx.recv_timeout(timeout);
            match msg {
                Ok(RenderMessage::Winit(ev)) => self.dispatch(&ev),
                Ok(RenderMessage::User(f)) => f(),
                Ok(RenderMessage::Paint { width, height, f }) => {
                    self.ensure_pixel_target(width, height);
                    // Exclusive `&mut` borrow of the double buffer for
                    // the duration of the closure - enforced by the type
                    // system, so no two painters can ever race on the buffer.
                    if let Some(target) = self.ctx.pixel_target.borrow_mut().as_mut() {
                        f(target);
                    }
                    self.ctx.redraw.set(true);
                }
                Ok(RenderMessage::Redraw) => self.ctx.redraw.set(true),
                Ok(RenderMessage::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            i_slint_core::platform::update_timers_and_animations();

            let redraw = self.ctx.redraw.replace(false);
            if redraw {
                // Advertise any presented frame (re-binding the image item via
                // the sink), upload the painted bytes, then draw. Everything
                // runs on this thread with a single `&mut` borrow of the target.
                let sink = self.ctx.image_sink.borrow();
                if let Some(target) = self.ctx.pixel_target.borrow_mut().as_mut() {
                    target.present_pending(&sink);
                    target.upload_dirty();
                }
                drop(sink);
                if let Some(r) = self.ctx.renderer.borrow().as_ref().and_then(|w| w.upgrade()) {
                    if let Err(e) = r.render() {
                        eprintln!("slint render thread: render error: {e}");
                    }
                }
            }
        }
    }

    fn process_events(
        &self,
        _timeout: Option<std::time::Duration>,
        _: InternalToken,
    ) -> Result<std::ops::ControlFlow<()>, PlatformError> {
        Ok(std::ops::ControlFlow::Continue(()))
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        struct Proxy(mpsc::Sender<RenderMessage>);
        impl EventLoopProxy for Proxy {
            fn quit_event_loop(&self) -> Result<(), i_slint_core::api::EventLoopError> {
                self.0
                    .send(RenderMessage::Quit)
                    .map_err(|_| i_slint_core::api::EventLoopError::EventLoopTerminated)
            }
            fn invoke_from_event_loop(
                &self,
                event: Box<dyn FnOnce() + Send>,
            ) -> Result<(), i_slint_core::api::EventLoopError> {
                self.0
                    .send(RenderMessage::User(event))
                    .map_err(|_| i_slint_core::api::EventLoopError::EventLoopTerminated)
            }
        }
        Some(Box::new(Proxy(self.ctx.host.sender.clone())))
    }
}

impl RenderThreadPlatform {
    fn window_adapter(&self) -> Option<Rc<dyn WindowAdapter>> {
        self.ctx.window_adapter.borrow().as_ref().and_then(|w| w.upgrade())
    }

    /// Translate a winit window event into the scene graph, mirroring the
    /// relevant part of `event_loop.rs`.
    fn dispatch(&self, event: &winit::event::WindowEvent) {
        let Some(adapter) = self.window_adapter() else {
            return;
        };
        let window = adapter.window();
        let window_inner = WindowInner::from_pub(window);

        // App-registered winit event filter, mirroring `event_loop.rs`. It runs
        // before the event is fed into the scene graph; `PreventDefault` skips
        // all further handling of this event.
        if let Some(hwnd_adapter) = adapter
            .internal(InternalToken)
            .and_then(|wa| (wa as &dyn core::any::Any).downcast_ref::<HwndWindowAdapter>())
        {
            if let Some(mut window_event_filter) = hwnd_adapter.window_event_filter.take() {
                let event_result = window_event_filter(window, event);
                hwnd_adapter.window_event_filter.set(Some(window_event_filter));

                match event_result {
                    crate::EventResult::PreventDefault => return,
                    crate::EventResult::Propagate => (),
                }
            }
        }

        match event {
            winit::event::WindowEvent::RedrawRequested => {}
            winit::event::WindowEvent::Resized(size) => {
                let size = PhysicalSize::new(size.width, size.height);
                *self.ctx.size.borrow_mut() = size;
                let sf = window_inner.scale_factor();
                let logical = i_slint_core::api::LogicalSize::new(
                    size.width as f32 / sf,
                    size.height as f32 / sf,
                );
                let _ = window.dispatch_event_with_result(WindowEvent::Resized { size: logical });
            }
            winit::event::WindowEvent::CloseRequested => {
                let _ = window.dispatch_event_with_result(WindowEvent::CloseRequested);
            }
            winit::event::WindowEvent::Focused(have_focus) => {
                let _ =
                    window.dispatch_event_with_result(WindowEvent::WindowActiveChanged(*have_focus));
            }
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                let p = position.to_logical(window_inner.scale_factor() as f64);
                self.ctx.cursor_pos.set(euclid::point2(p.x, p.y));
                window.dispatch_event(WindowEvent::internal(BackendMouseEvent::Moved {
                    position: self.ctx.cursor_pos.get(),
                    touch_finger_id: 0,
                }));
                adapter.request_redraw();
            }
            winit::event::WindowEvent::CursorLeft { .. } => {
                if !self.ctx.pressed.replace(false) {
                    window.dispatch_event(WindowEvent::internal(BackendMouseEvent::Exit));
                }
                adapter.request_redraw();
            }
            winit::event::WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(lx, ly) => (lx * 60., ly * 60.),
                    winit::event::MouseScrollDelta::PixelDelta(d) => {
                        let d = d.to_logical(window_inner.scale_factor() as f64);
                        (d.x, d.y)
                    }
                };
                window.dispatch_event(WindowEvent::internal(BackendMouseEvent::Wheel {
                    position: self.ctx.cursor_pos.get(),
                    delta_x: dx,
                    delta_y: dy,
                    phase: TouchPhase::Moved,
                }));
                adapter.request_redraw();
            }
            winit::event::WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => PointerEventButton::Left,
                    winit::event::MouseButton::Right => PointerEventButton::Right,
                    winit::event::MouseButton::Middle => PointerEventButton::Middle,
                    winit::event::MouseButton::Back => PointerEventButton::Back,
                    winit::event::MouseButton::Forward => PointerEventButton::Forward,
                    winit::event::MouseButton::Other(_) => PointerEventButton::Other,
                };
                let ev = match state {
                    winit::event::ElementState::Pressed => {
                        self.ctx.pressed.set(true);
                        BackendMouseEvent::Pressed {
                            position: self.ctx.cursor_pos.get(),
                            button,
                            click_count: 0,
                            touch_finger_id: 0,
                        }
                    }
                    winit::event::ElementState::Released => {
                        self.ctx.pressed.set(false);
                        BackendMouseEvent::Released {
                            position: self.ctx.cursor_pos.get(),
                            button,
                            click_count: 0,
                            touch_finger_id: 0,
                        }
                    }
                };
                window.dispatch_event(WindowEvent::internal(ev));
                adapter.request_redraw();
            }
            winit::event::WindowEvent::KeyboardInput { event: k, .. } => {
                let text = slint_key_text(k);
                if text.is_empty() {
                    return;
                }
                let event_type = match k.state {
                    winit::event::ElementState::Pressed => KeyEventType::KeyPressed,
                    winit::event::ElementState::Released => KeyEventType::KeyReleased,
                };
                let mut key_event = KeyEvent::default();
                key_event.text = text;
                key_event.repeat = k.repeat;
                let ev = InternalKeyEvent {
                    key_event,
                    event_type,
                    ..Default::default()
                };
                window.dispatch_event(WindowEvent::internal(ev));
            }
            winit::event::WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let _ = window.dispatch_event_with_result(WindowEvent::ScaleFactorChanged {
                    scale_factor: *scale_factor as f32,
                });
            }
            _ => {}
        }
    }
}

/// Minimal key-to-text translation. Most app shortcuts are handled on the UI
/// thread via the raw winit `KeyboardInput` event, so a conservative mapping to
/// character text suffices for text-field editing.
///
/// Modifier keys must map to the `key_codes` chars (not empty text): slint's
/// `InternalKeyboardModifierState::state_update` tracks Ctrl/Shift/Alt/Meta from
/// those key events, and `PointerScrollEvent.modifiers` / `KeyEvent.modifiers`
/// are derived from that tracked state. Without this mapping a ctrl+wheel
/// scroll would never report `event.modifiers.control` and ctrl+wheel zoom
/// short-cuts never fire.
fn slint_key_text(event: &winit::event::KeyEvent) -> i_slint_core::SharedString {
    use i_slint_core::input::key_codes;
    use winit::keyboard::{Key, KeyLocation, NamedKey};
    let named = match &event.logical_key {
        Key::Character(s) => return s.as_str().into(),
        Key::Named(n) => *n,
        _ => return event.text.as_ref().map_or_else(|| "".into(), |t| t.as_str().into()),
    };
    let c = match (named, event.location) {
        (NamedKey::Control, KeyLocation::Left) => key_codes::Control,
        (NamedKey::Control, _) => key_codes::ControlR,
        (NamedKey::Shift, KeyLocation::Left) => key_codes::Shift,
        (NamedKey::Shift, _) => key_codes::ShiftR,
        (NamedKey::Super, KeyLocation::Left) => key_codes::Meta,
        (NamedKey::Super, _) => key_codes::MetaR,
        (NamedKey::Alt, KeyLocation::Right) => key_codes::AltGr,
        (NamedKey::Alt, _) => key_codes::Alt,
        _ => return event.text.as_ref().map_or_else(|| "".into(), |t| t.as_str().into()),
    };
    c.into()
}