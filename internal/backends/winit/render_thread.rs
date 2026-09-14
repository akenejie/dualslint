// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// dualslint — 2-thread render separation for the Slint GUI toolkit.
//
// This module is the cross-thread protocol and the render-thread GL driver.
// The UI thread encodes the scene graph into `SceneFrame`s (`snapshot.rs`);
// the render thread replays them against a FemtoVG/GL stack it owns entirely.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use i_slint_core::graphics::{Color, euclid};
use i_slint_core::lengths::{LogicalRect, PhysicalBorderRadius, PhysicalPx};

use crate::winit_compat::WindowSurfaceSizeExt;

/// Physical-pixel geometry aliases (documents that all command payloads are
/// in physical pixels, matching femtovg's coordinate space).
pub type PhysicalLength = euclid::Length<f32, PhysicalPx>;
pub type PhysicalPoint = euclid::Point2D<f32, PhysicalPx>;
pub type PhysicalRect = euclid::Rect<f32, PhysicalPx>;

// ---------------------------------------------------------------------------
// Shared global state
// ---------------------------------------------------------------------------

/// Frame queue: completed frames from the render thread await pickup by the UI
/// thread for optional post-processing (e.g. cursor overlays). For the core
/// split path, the render thread presents directly, so this queue is primarily
/// used for diagnostics / frame timing.
pub(crate) static GLOBAL_FRAME_QUEUE: OnceLock<FrameQueue> = OnceLock::new();

/// Render host — the send-half.  The UI thread and any worker thread holds
/// this to push paint closures or scene snapshots to the render thread.
pub(crate) static GLOBAL_RENDER_HOST: OnceLock<RenderHost> = OnceLock::new();

/// Global HWND (Windows only) stored when the winit window is created.
#[cfg(target_os = "windows")]
pub(crate) static GLOBAL_HWND: OnceLock<isize> = OnceLock::new();

/// Global image sink callback.  Not used in the 2-thread GL path but retained
/// for backward compatibility with the CPU-raster API.
pub(crate) static GLOBAL_IMAGE_SINK: std::sync::Mutex<
    Option<Box<dyn Fn(i_slint_core::graphics::Image) + Send + Sync>>,
> = std::sync::Mutex::new(None);

// ---------------------------------------------------------------------------
// Protocol types (UI thread → render thread)
// ---------------------------------------------------------------------------

/// Frame metadata returned to the UI thread after present (diagnostics).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub(crate) type FrameQueue = Arc<Mutex<VecDeque<Frame>>>;

/// Messages the UI thread sends to the render thread.
pub enum RenderMessage {
    /// Provide the winit window + initial configuration. The render thread
    /// creates the glutin GL context and FemtoVG canvas on its own thread.
    Configure {
        /// The winit window, created on the UI thread.  Render thread uses
        /// its raw display/window handles to bootstrap glutin.
        window: Arc<winit::window::Window>,
        /// Initial physical pixel dimensions.
        width: u32,
        height: u32,
        /// Scale factor for text/transform scaling.
        scale_factor: f64,
    },
    /// The GL surface has been resized.
    Resize {
        width: u32,
        height: u32,
    },
    /// A complete scene snapshot from the UI thread's snapshot encoder.
    RenderScene {
        frame: SceneFrame,
    },
    /// Execute an arbitrary closure on the render thread.
    User(Box<dyn FnOnce() + Send>),
    /// Drop the render-thread GL state (window hidden / context suspended).
    /// The render thread releases its `Arc<winit::window::Window>`, allowing
    /// the UI thread's `suspend` to actually destroy the native window.
    Suspend,
    /// Terminate the render thread.
    Quit,
}

// ---------------------------------------------------------------------------
// Scene snapshot types — the retained-mode scene description
// ---------------------------------------------------------------------------

/// A serialisable paint description, equivalent to femtovg::Paint.
#[derive(Clone, Debug)]
pub enum PaintDesc {
    /// Solid color fill.
    Solid {
        r: u8, g: u8, b: u8, a: u8,
    },
    /// Linear gradient.
    LinearGradient {
        start_x: f32, start_y: f32,
        end_x: f32, end_y: f32,
        stops: Vec<GradientStop>,
    },
    /// Radial gradient.
    RadialGradient {
        cx: f32, cy: f32, radius: f32,
        stops: Vec<GradientStop>,
    },
    /// Texture-mapped paint (image blit).
    ImagePaint {
        texture_key: u64,
        x: f32, y: f32,
        w: f32, h: f32,
        tex_x: f32, tex_y: f32,
        tex_w: f32, tex_h: f32,
        flags: u32,
    },
}

/// A single gradient colour stop.
#[derive(Clone, Debug)]
pub struct GradientStop {
    pub offset: f32,
    pub color: [u8; 4], // RGBA
}

/// One segment of a vector path.
#[derive(Clone, Debug)]
pub enum PathEvent {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

/// A positioned glyph for cross-thread text rendering.
#[derive(Clone, Debug)]
pub struct PositionedGlyph {
    pub x: f32,
    pub y: f32,
    pub id: u16,
}

/// Line cap style for strokes.
#[derive(Clone, Copy, Debug)]
pub enum LineCapDesc {
    Butt,
    Round,
    Square,
}

/// Line join style for strokes.
#[derive(Clone, Copy, Debug)]
pub enum LineJoinDesc {
    Miter,
    Round,
    Bevel,
}

/// Mirrors a control's screen region for the render thread's coordinate table.
#[derive(Clone, Debug)]
pub struct ControlRegion {
    /// Opaque handle combining component pointer + item index.
    pub id: u64,
    pub geometry: LogicalRect,
}

/// Complete serialisable scene for one frame.
#[derive(Clone, Debug)]
pub struct SceneFrame {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    /// Window background as RGBA bytes; cleared before the commands replay.
    /// `None` means the UI thread did not see a solid brush and the commands
    /// carry the full background instead.
    pub background: Option<[u8; 4]>,
    pub commands: Vec<DrawCommand>,
    pub controls: Vec<ControlRegion>,
}

/// A single draw command in the serialised scene.
#[derive(Clone, Debug)]
pub enum DrawCommand {
    // -- Canvas state --
    Save,
    Restore,
    Translate(f32, f32),
    Rotate(f32),
    Scale(f32, f32),
    /// Directly set the global alpha (used after accumulation).
    SetGlobalAlpha(f32),
    /// Intersection clip rect (physical coordinates).
    CombineClip(PhysicalRect),

    // -- Primitives --
    /// Fill a rectangle with a solid or gradient paint (no border radius).
    FillRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        anti_alias: bool,
    },
    /// Fill a rounded rectangle (background).
    FillRoundedRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: PhysicalBorderRadius,
        anti_alias: bool,
    },
    /// Stroke a path (border rectangle).
    StrokePath {
        path: Vec<PathEvent>,
        paint: PaintDesc,
        line_width: f32,
        line_cap: LineCapDesc,
        line_join: LineJoinDesc,
        miter_limit: f32,
        anti_alias: bool,
    },
    /// Fill a path (rounded rect background or explicit path).
    FillPath {
        path: Vec<PathEvent>,
        paint: PaintDesc,
        fill_rule: u8,
        anti_alias: bool,
    },

    // -- Text (glyph-based) --
    /// Draw a glyph run produced by sharedparley on the UI thread.
    DrawGlyphRun {
        font_data: Vec<u8>,
        font_index: u32,
        font_size: f32,
        normalized_coords: Vec<i16>,
        paint: PaintDesc,
        y_offset: f32,
        glyphs: Vec<PositionedGlyph>,
        is_stroke: bool,
    },
    /// Fill a rectangle (underline/strikethrough/cursor) during text drawing.
    FillTextRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: f32,
        border: Option<(PaintDesc, f32)>,
    },

    // -- Images / pixmaps --
    /// Upload a raw RGBA8 pixel buffer to the render thread's texture cache.
    UploadPixmap {
        key: u64,
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Draw a previously-uploaded pixmap at the current canvas position.
    BlitPixmap {
        key: u64,
        /// f32 x, y, w, h, tex_x, tex_y, tex_w, tex_h, flags
        params: [f32; 9],
    },

    // -- Layer (offscreen compositing for opacity / clip-with-radius / layer hint) --
    Layer {
        width: u32,
        height: u32,
        /// Blit origin in physical pixels.
        origin: PhysicalPoint,
        alpha_tint: f32,
        commands: Vec<DrawCommand>,
    },

    // -- Box shadow --
    /// Render a drop-shadow: the render thread creates a small texture, fills
    /// a rounded rect, blurs, and composites.  All params in physical pixels.
    DrawBoxShadow {
        color: Color,
        blur: f32,
        offset_x: f32,
        offset_y: f32,
        width: f32,
        height: f32,
        radius: PhysicalBorderRadius,
    },

    // -- Cached pixmap (from custom widget painting) --
    CachedPixmap {
        key: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// RenderHost — send half (UI thread + workers)
// ---------------------------------------------------------------------------

/// The send half of the render-thread channel.  Clonable and Send-safe.
#[derive(Clone)]
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,
    event_loop_proxy: Option<winit::event_loop::EventLoopProxy<crate::SlintEvent>>,
}

impl RenderHost {
    /// Send an arbitrary closure to execute on the render thread.
    pub fn send_user(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(RenderMessage::User(Box::new(f)));
    }

    /// Ask the render thread to quit.
    pub fn send_quit(&self) {
        let _ = self.sender.send(RenderMessage::Quit);
    }

    /// Signal a redraw (may be a no-op if the render thread already
    /// redraws continuously; here it sends a user-event back to the UI
    /// event loop to schedule the next frame).
    pub fn send_redraw(&self) {
        // In the 2-thread GL path, "redraw" means "request next snapshot
        // from the UI thread".  We signal via the winit event loop proxy.
        if let Some(proxy) = &self.event_loop_proxy {
            let _ = proxy.send_event(crate::SlintEvent(crate::event_loop::CustomEvent::RequestRedraw));
        }
    }

    /// Submit a scene snapshot for rendering.  This is the primary API in
    /// the 2-thread GL path.
    pub fn submit_scene(&self, frame: SceneFrame) {
        let _ = self.sender.send(RenderMessage::RenderScene { frame });
    }

    /// Configure the render thread with the winit window + initial size.
    pub(crate) fn submit_configure(
        &self,
        window: Arc<winit::window::Window>,
        width: u32,
        height: u32,
        scale_factor: f64,
    ) {
        let _ = self
            .sender
            .send(RenderMessage::Configure { window, width, height, scale_factor });
    }

    /// Notify the render thread that the GL surface was resized.
    pub(crate) fn submit_resize(&self, width: u32, height: u32) {
        let _ = self.sender.send(RenderMessage::Resize { width, height });
    }

    /// Ask the render thread to tear down its GL context and release the window.
    pub(crate) fn submit_suspend(&self) {
        let _ = self.sender.send(RenderMessage::Suspend);
    }

    /// Submit a CPU pixel paint closure (legacy API).  Not used in the
    /// 2-thread GL path; retained for backward API compatibility.
    pub fn paint<F>(&self, width: u32, height: u32, f: F)
    where
        F: FnOnce(&mut render_thread_legacy::PixelTarget) + Send + 'static,
    {
        // In the legacy path, this routes through the old channel.
        // For now, wrap into a RenderMessage::User that panics to
        // clearly indicate misuse.
        let _ = (width, height);
        self.send_user(move || {
            let _ = f;
            panic!("dualslint: legacy paint() called in GL thread mode — use submit_scene() instead");
        });
    }
}

// ---------------------------------------------------------------------------
// RenderCore — receive half (runs on the render thread)
// ---------------------------------------------------------------------------

/// The render-thread receive half and event loop driver.
pub(crate) struct RenderCore {
    rx: mpsc::Receiver<RenderMessage>,
    host: RenderHost,
    frame_queue: FrameQueue,
}

impl RenderCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, host: RenderHost, frame_queue: FrameQueue) -> Self {
        Self { rx, host, frame_queue }
    }

    /// Run the render-thread event loop.  Blocks until `Quit`.
    pub(crate) fn run(&mut self) {
        // Render-thread state: GL context + femtovg canvas, created on first
        // `Configure` message.
        let mut gl_state: Option<GlRenderState> = None;

        while let Ok(msg) = self.rx.recv() {
            match msg {
                RenderMessage::Configure { window, width, height, scale_factor } => {
                    match GlRenderState::new(window, width, height, scale_factor) {
                        Ok(state) => {
                            gl_state = Some(state);
                        }
                        Err(e) => {
                            eprintln!("dualslint render thread: GL init failed: {e}");
                        }
                    }
                }
                RenderMessage::Resize { width, height } => {
                    if let Some(state) = &mut gl_state {
                        state.resize(width, height);
                    }
                }
                RenderMessage::Resize { width, height } => {
                    if let Some(state) = &mut gl_state {
                        state.resize(width, height);
                    }
                }
                RenderMessage::RenderScene { frame } => {
                    if let Some(state) = &mut gl_state {
                        state.render_scene(&frame, &self.frame_queue, &self.host);
                    }
                }
                RenderMessage::User(f) => {
                    f();
                }
                RenderMessage::Suspend => {
                    // Drop the GL context + canvas and release the winit window
                    // Arc so the UI thread can destroy the native window.
                    gl_state = None;
                }
                RenderMessage::Quit => break,
            }
        }
    }

    pub(crate) fn host(&self) -> &RenderHost {
        &self.host
    }
}

/// Create the render-thread channel pair.
pub(crate) fn channel(
    event_loop_proxy: winit::event_loop::EventLoopProxy<crate::SlintEvent>,
) -> (RenderHost, RenderCore, FrameQueue) {
    let (tx, rx) = mpsc::channel();
    let frame_queue: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
    let host = RenderHost {
        sender: tx,
        event_loop_proxy: Some(event_loop_proxy),
    };
    let core = RenderCore::new(rx, host.clone(), frame_queue.clone());
    (host, core, frame_queue)
}

// ---------------------------------------------------------------------------
// Accessor helpers
// ---------------------------------------------------------------------------

/// Obtain the [`RenderHost`] for the current process.
pub fn host() -> Option<RenderHost> {
    GLOBAL_RENDER_HOST.get().cloned()
}

/// Register a callback that receives images from the render thread (legacy).
pub fn set_image_sink<F>(sink: F)
where
    F: Fn(i_slint_core::graphics::Image) + Send + Sync + 'static,
{
    if let Ok(mut guard) = GLOBAL_IMAGE_SINK.lock() {
        *guard = Some(Box::new(sink));
    }
}

/// Returns the raw HWND (Windows only).
#[cfg(target_os = "windows")]
pub fn hwnd() -> Option<isize> {
    GLOBAL_HWND.get().copied()
}

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

use std::cell::RefCell;

struct GlRenderState {
    window: Arc<winit::window::Window>,
    glutin_context: glutin::context::PossiblyCurrentContext,
    glutin_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    femtovg_canvas: RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
    femtovg_text_context: femtovg::TextContext,
    width: u32,
    height: u32,
    scale_factor: f32,
    /// Font cache mapping (font-blob hash, index) → femtovg FontId.
    font_cache: RefCell<std::collections::HashMap<(u64, u32), femtovg::FontId>>,
    /// Texture cache for uploaded pixmaps.
    texture_cache: RefCell<std::collections::HashMap<u64, femtovg::ImageId>>,
    /// Layer texture cache keyed by (item ptr, index) → (origin, texture).
    layer_cache: RefCell<std::collections::HashMap<u64, (PhysicalPoint, femtovg::ImageId)>>,
}

impl GlRenderState {
    fn new(
        window: Arc<winit::window::Window>,
        width: u32,
        height: u32,
        scale_factor: f64,
    ) -> Result<Self, String> {
        use glutin::context::{ContextApi, ContextAttributesBuilder};
        use glutin::prelude::*;
        use glutin::surface::{GlSurface, SurfaceAttributesBuilder, WindowSurface};
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

        let raw_display = window.display_handle()
            .map_err(|e| format!("Failed to get display handle: {e}"))?;
        let raw_window = window.window_handle()
            .map_err(|e| format!("Failed to get window handle: {e}"))?;

        // Build GL display on the render thread using the window's handles.
        // The `DisplayApiPreference` variants are cfg-gated by glutin per
        // backend, so each platform must select its own.
        #[cfg(target_os = "windows")]
        let display_preference = glutin::display::DisplayApiPreference::EglThenWgl(
            raw_window_handle::RawWindowHandle::from(raw_window.as_raw())
        );
        #[cfg(target_os = "macos")]
        let display_preference = glutin::display::DisplayApiPreference::Cgl;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let display_preference = glutin::display::DisplayApiPreference::Egl;

        let gl_display = unsafe {
            glutin::display::Display::new(raw_display.as_raw(), display_preference)
                .map_err(|e| format!("glutin Display::new failed: {e}"))?
        };

        let config_template = glutin::config::ConfigTemplateBuilder::new();
        let config = unsafe { gl_display
            .find_configs(config_template.build())
            .map_err(|e| format!("glutin find_configs failed: {e}"))?
            .next()
            .ok_or_else(|| "No suitable GL config found".to_string())? };

        let raw_window_handle = raw_window.as_raw();

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(glutin::context::Version { major: 2, minor: 0 })))
            .build(Some(raw_window_handle));

        let not_current_ctx = unsafe {
            gl_display.create_context(&config, &context_attributes)
                .or_else(|_| {
                    let fallback = ContextAttributesBuilder::new()
                        .build(Some(raw_window_handle));
                    gl_display.create_context(&config, &fallback)
                })
                .map_err(|e| format!("glutin create_context failed: {e}"))?
        };

        let size: winit::dpi::PhysicalSize<u32> = window.surface_size();
        let non_zero_w = NonZeroU32::new(size.width.max(1))
            .ok_or("Window width is zero")?;
        let non_zero_h = NonZeroU32::new(size.height.max(1))
            .ok_or("Window height is zero")?;

        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            non_zero_w,
            non_zero_h,
        );

        let surface = unsafe {
            gl_display.create_window_surface(&config, &surface_attributes)
                .map_err(|e| format!("glutin create_window_surface failed: {e}"))?
        };

        let context = not_current_ctx.make_current(&surface)
            .map_err(|e| format!("make_current failed: {e}"))?;

        // Set vsync
        surface.set_swap_interval(
            &context,
            glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
        ).ok();

        // Build femtovg canvas
        let proc_addr = |name: &std::ffi::CStr| -> *const c_void {
            gl_display.get_proc_address(name)
        };
        let backend = unsafe { femtovg::renderer::OpenGl::new_from_function_cstr(proc_addr) }
            .map_err(|e| format!("femtovg OpenGL init failed: {e}"))?;
        let text_context = femtovg::TextContext::default();
        let mut canvas = femtovg::Canvas::new(backend)
            .map_err(|e| format!("femtovg Canvas::new failed: {e}"))?;
        canvas.set_size(width, height, scale_factor.ceil() as _);

        Ok(Self {
            window,
            glutin_context: context,
            glutin_surface: surface,
            femtovg_canvas: RefCell::new(canvas),
            femtovg_text_context: text_context,
            width,
            height,
            scale_factor: scale_factor as f32,
            font_cache: RefCell::new(std::collections::HashMap::new()),
            texture_cache: RefCell::new(std::collections::HashMap::new()),
            layer_cache: RefCell::new(std::collections::HashMap::new()),
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        use glutin::surface::GlSurface;
        self.width = width;
        self.height = height;
        if let Some(nz_w) = NonZeroU32::new(width) {
            if let Some(nz_h) = NonZeroU32::new(height) {
                self.glutin_surface.resize(&self.glutin_context, nz_w, nz_h);
            }
        }
        self.femtovg_canvas.borrow_mut().set_size(
            width, height, self.scale_factor.ceil() as _,
        );
    }

    fn render_scene(
        &mut self,
        frame: &SceneFrame,
        _frame_queue: &FrameQueue,
        _host: &RenderHost,
    ) {
        use glutin::prelude::*;

        let canvas = &self.femtovg_canvas;

        // Set size and reset
        {
            let mut cv = canvas.borrow_mut();
            cv.set_size(frame.width, frame.height, frame.scale_factor.ceil() as _);
            cv.reset();
        }

        // Clear with the window background (or white when the UI thread
        // serialized the background as commands).
        {
            let mut cv = canvas.borrow_mut();
            let clear = match frame.background {
                Some([r, g, b, a]) => femtovg::Color::rgba(r, g, b, a),
                None => femtovg::Color::rgba(255, 255, 255, 255),
            };
            cv.clear_rect(0, 0, frame.width, frame.height, clear);
        }

        // Replay commands
        for cmd in &frame.commands {
            self.replay_command(canvas, cmd);
        }

        // Flush and present
        let commands = canvas.borrow_mut().flush_to_output(());
        // Submit is a no-op for the OpenGL backend, flush handles it
        let _ = commands;

        // Swap buffers
        if let Err(e) = self.glutin_surface.swap_buffers(&self.glutin_context) {
            eprintln!("dualslint: swap_buffers failed: {e}");
        }
        self.window.pre_present_notify();
    }

    fn replay_command(&self, canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>, cmd: &DrawCommand) {
        match cmd {
            DrawCommand::Save => {
                canvas.borrow_mut().save();
            }
            DrawCommand::Restore => {
                canvas.borrow_mut().restore();
            }
            DrawCommand::Translate(dx, dy) => {
                canvas.borrow_mut().translate(*dx, *dy);
            }
            DrawCommand::Rotate(angle) => {
                canvas.borrow_mut().rotate(angle.to_radians());
            }
            DrawCommand::Scale(sx, sy) => {
                canvas.borrow_mut().scale(*sx, *sy);
            }
            DrawCommand::SetGlobalAlpha(alpha) => {
                canvas.borrow_mut().set_global_alpha(*alpha);
            }
            DrawCommand::CombineClip(rect) => {
                canvas.borrow_mut().intersect_scissor(
                    rect.origin.x, rect.origin.y, rect.size.width, rect.size.height,
                );
            }
            DrawCommand::FillRect { rect, paint, anti_alias } => {
                let paint_f = self.desc_to_paint(paint);
                if let Some(mut p) = paint_f {
                    p.set_anti_alias(*anti_alias);
                    let path = rect_to_femtovg_path(*rect);
                    canvas.borrow_mut().fill_path(&path, &p);
                }
            }
            DrawCommand::FillRoundedRect { rect, paint, radius, anti_alias } => {
                let paint_f = self.desc_to_paint(paint);
                if let Some(mut p) = paint_f {
                    p.set_anti_alias(*anti_alias);
                    let path = rounded_rect_to_femtovg_path(*rect, *radius);
                    canvas.borrow_mut().fill_path(&path, &p);
                }
            }
            DrawCommand::StrokePath { path, paint, line_width, line_cap, line_join, miter_limit, anti_alias } => {
                let paint_f = self.desc_to_paint_stroke(paint);
                if let Some(mut p) = paint_f {
                    p.set_line_width(*line_width);
                    p.set_anti_alias(*anti_alias);
                    p.set_miter_limit(*miter_limit);
                    p.set_line_cap(match line_cap {
                        LineCapDesc::Butt => femtovg::LineCap::Butt,
                        LineCapDesc::Round => femtovg::LineCap::Round,
                        LineCapDesc::Square => femtovg::LineCap::Square,
                    });
                    p.set_line_join(match line_join {
                        LineJoinDesc::Miter => femtovg::LineJoin::Miter,
                        LineJoinDesc::Round => femtovg::LineJoin::Round,
                        LineJoinDesc::Bevel => femtovg::LineJoin::Bevel,
                    });
                    let fp = lyon_path_to_femtovg(path);
                    canvas.borrow_mut().stroke_path(&fp, &p);
                }
            }
            DrawCommand::FillPath { path, paint, fill_rule, anti_alias } => {
                let paint_f = self.desc_to_paint(paint);
                if let Some(mut p) = paint_f {
                    p.set_anti_alias(*anti_alias);
                    p.set_fill_rule(match fill_rule {
                        1 => femtovg::FillRule::EvenOdd,
                        _ => femtovg::FillRule::NonZero,
                    });
                    let fp = lyon_path_to_femtovg(path);
                    canvas.borrow_mut().fill_path(&fp, &p);
                }
            }
            DrawCommand::DrawGlyphRun {
                font_data, font_index, font_size, normalized_coords,
                paint, y_offset, glyphs, is_stroke,
            } => {
                self.replay_glyph_run(canvas, font_data, *font_index, *font_size,
                    normalized_coords, paint, *y_offset, glyphs, *is_stroke);
            }
            DrawCommand::FillTextRect { rect, paint, radius, border } => {
                let paint_f = self.desc_to_paint(paint);
                if let Some(p) = paint_f {
                    let mut path = femtovg::Path::new();
                    if *radius > 0.0 {
                        path.rounded_rect(rect.origin.x, rect.origin.y,
                            rect.size.width, rect.size.height, *radius);
                    } else {
                        path.rect(rect.origin.x, rect.origin.y,
                            rect.size.width, rect.size.height);
                    }
                    canvas.borrow_mut().fill_path(&path, &p);
                    if let Some((border_paint_desc, width)) = border {
                        if let Some(mut bp) = self.desc_to_paint_stroke(border_paint_desc) {
                            bp.set_line_width(*width);
                            canvas.borrow_mut().stroke_path(&path, &bp);
                        }
                    }
                }
            }
            DrawCommand::UploadPixmap { key, pixels, width, height } => {
                self.upload_pixmap(canvas, *key, pixels, *width, *height);
            }
            DrawCommand::BlitPixmap { key, params } => {
                self.blit_pixmap(canvas, *key, params);
            }
            DrawCommand::Layer { width, height, origin, alpha_tint, commands } => {
                self.render_layer(canvas, *width, *height, *origin, *alpha_tint, commands);
            }
            DrawCommand::DrawBoxShadow { color, blur, offset_x, offset_y, width, height, radius } => {
                self.render_box_shadow(canvas, color, *blur, *offset_x, *offset_y,
                    *width, *height, *radius);
            }
            DrawCommand::CachedPixmap { key, width, height, pixels } => {
                if !self.texture_cache.borrow().contains_key(key) {
                    self.upload_pixmap(canvas, *key, pixels, *width, *height);
                }
                self.blit_pixmap(canvas, *key, &[0.0; 9]);
            }
        }
    }

    fn desc_to_paint(&self, desc: &PaintDesc) -> Option<femtovg::Paint> {
        match desc {
            PaintDesc::Solid { r, g, b, a } => {
                Some(femtovg::Paint::color(femtovg::Color::rgba(*r, *g, *b, *a)))
            }
            PaintDesc::LinearGradient { start_x, start_y, end_x, end_y, stops } => {
                let paint = femtovg::Paint::linear_gradient_stops(
                    *start_x, *start_y, *end_x, *end_y,
                    stops.iter().map(|s| {
                        (s.offset, femtovg::Color::rgba(s.color[0], s.color[1], s.color[2], s.color[3]))
                    }),
                );
                Some(paint)
            }
            PaintDesc::RadialGradient { cx, cy, radius, stops } => {
                let paint = femtovg::Paint::radial_gradient_stops(
                    *cx, *cy, 0.0, *radius,
                    stops.iter().map(|s| {
                        (s.offset, femtovg::Color::rgba(s.color[0], s.color[1], s.color[2], s.color[3]))
                    }),
                );
                Some(paint)
            }
            PaintDesc::ImagePaint { .. } => None, // handled separately
        }
    }

    fn desc_to_paint_stroke(&self, desc: &PaintDesc) -> Option<femtovg::Paint> {
        self.desc_to_paint(desc)
    }

    fn get_or_create_font(&self, font_data: &[u8], font_index: u32) -> Option<femtovg::FontId> {
        let hash = blob_hash(font_data);
        let key = (hash, font_index);
        let mut cache = self.font_cache.borrow_mut();
        if let Some(&font_id) = cache.get(&key) {
            return Some(font_id);
        }
        let font_id = self.femtovg_text_context
            .add_shared_font_with_index(font_data.to_vec(), font_index)
            .ok()?;
        cache.insert(key, font_id);
        Some(font_id)
    }

    fn upload_pixmap(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        key: u64,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) {
        use rgb::FromSlice;
        let img = imgref::Img::new(pixels.as_rgba(), width as usize, height as usize);
        if let Ok(image_id) = canvas.borrow_mut().create_image(img, femtovg::ImageFlags::PREMULTIPLIED) {
            self.texture_cache.borrow_mut().insert(key, image_id);
        }
    }

    fn blit_pixmap(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        key: u64,
        params: &[f32; 9],
    ) {
        if let Some(&image_id) = self.texture_cache.borrow().get(&key) {
            let cv = canvas.borrow();
            if cv.image_info(image_id).is_err() {
                return;
            }
            drop(cv);

            let paint = femtovg::Paint::image(
                image_id,
                params[0], params[1], // x, y
                params[2], params[3], // w, h
                params[4], // rotation
                params[5], // opacity
            ).with_anti_alias(false);

            let mut path = femtovg::Path::new();
            path.rect(params[0], params[1], params[2], params[3]);
            canvas.borrow_mut().fill_path(&path, &paint);
        }
    }

    fn render_layer(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        width: u32,
        height: u32,
        origin: PhysicalPoint,
        alpha_tint: f32,
        commands: &[DrawCommand],
    ) {
        // Create an offscreen texture
        let image_id = {
            let mut cv = canvas.borrow_mut();
            match cv.create_image_empty(
                width as usize,
                height as usize,
                femtovg::PixelFormat::Rgba8,
                femtovg::ImageFlags::PREMULTIPLIED | femtovg::ImageFlags::FLIP_Y,
            ) {
                Ok(id) => id,
                Err(_) => return,
            }
        };

        let render_target = femtovg::RenderTarget::Image(image_id);

        // Render children into the layer
        {
            let mut cv = canvas.borrow_mut();
            cv.save();
            cv.set_render_target(render_target);
            cv.reset();
            cv.clear_rect(0, 0, width, height, femtovg::Color::rgba(0, 0, 0, 0));
            // Translate so children are positioned relative to origin
            cv.translate(-origin.x, -origin.y);
        }

        for cmd in commands {
            self.replay_command(canvas, cmd);
        }

        // Restore and blend
        {
            let mut cv = canvas.borrow_mut();
            cv.restore();
        }

        // Blit the layer
        {
            let mut cv = canvas.borrow_mut();
            let paint = femtovg::Paint::image(
                image_id,
                origin.x, origin.y,
                width as f32, height as f32,
                0.0, alpha_tint,
            ).with_anti_alias(false);

            let mut path = femtovg::Path::new();
            path.rect(origin.x, origin.y, width as f32, height as f32);
            cv.fill_path(&path, &paint);
        }

        // Cleanup
        {
            let mut cv = canvas.borrow_mut();
            cv.delete_image(image_id);
        }
    }

    fn render_box_shadow(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        color: &Color,
        blur: f32,
        offset_x: f32,
        offset_y: f32,
        width: f32,
        height: f32,
        radius: PhysicalBorderRadius,
    ) {
        if blur == 0.0 && offset_x == 0.0 && offset_y == 0.0 {
            return;
        }
        let shadow_color = to_femtovg_color(color);
        let shadow_width = width + blur;
        let shadow_height = height + blur;

        let shadow_img_w = shadow_width.ceil() as u32;
        let shadow_img_h = shadow_height.ceil() as u32;
        if shadow_img_w == 0 || shadow_img_h == 0 {
            return;
        }

        // Create shadow texture
        let shadow_img = {
            let mut cv = canvas.borrow_mut();
            match cv.create_image_empty(
                shadow_img_w as usize,
                shadow_img_h as usize,
                femtovg::PixelFormat::Rgba8,
                femtovg::ImageFlags::PREMULTIPLIED,
            ) {
                Ok(id) => id,
                Err(_) => return,
            }
        };

        // Fill shadow rect
        {
            let mut cv = canvas.borrow_mut();
            cv.save();
            cv.set_render_target(femtovg::RenderTarget::Image(shadow_img));
            cv.reset();
            cv.clear_rect(0, 0, shadow_img_w, shadow_img_h,
                          femtovg::Color::rgba(0, 0, 0, 0));

            let shadow_rect = PhysicalRect::new(
                PhysicalPoint::default(),
                euclid::Size2D::new(width, height),
            );
            let path = rounded_rect_to_femtovg_path(shadow_rect, radius);
            cv.fill_path(
                &path,
                &femtovg::Paint::color(femtovg::Color::rgb(255, 255, 255)),
            );
        }

        // Apply blur if needed
        let final_img = if blur > 0.0 {
            let sigma = blur * 0.5;
            let blurred = {
                let mut cv = canvas.borrow_mut();
                let target = cv.create_image_empty(
                    shadow_img_w as usize,
                    shadow_img_h as usize,
                    femtovg::PixelFormat::Rgba8,
                    femtovg::ImageFlags::PREMULTIPLIED,
                );
                match target {
                    Ok(id) => {
                        cv.filter_image(id, femtovg::ImageFilter::GaussianBlur { sigma }, shadow_img);
                        id
                    }
                    Err(_) => {
                        cv.delete_image(shadow_img);
                        return;
                    }
                }
            };
            {
                let mut cv = canvas.borrow_mut();
                cv.delete_image(shadow_img);
            }
            blurred
        } else {
            shadow_img
        };

        // Tint the shadow
        {
            let mut cv = canvas.borrow_mut();
            cv.save();
            cv.global_composite_operation(femtovg::CompositeOperation::SourceIn);
            let mut tint_path = femtovg::Path::new();
            tint_path.rect(0., 0., shadow_img_w as f32, shadow_img_h as f32);
            cv.fill_path(
                &tint_path,
                &femtovg::Paint::color(shadow_color),
            );
            cv.restore();
        }

        // Blit shadow at offset
        {
            let mut cv = canvas.borrow_mut();
            cv.restore();

            let ox = offset_x - blur;
            let oy = offset_y - blur;

            let paint = femtovg::Paint::image(
                final_img,
                ox, oy,
                shadow_img_w as f32, shadow_img_h as f32,
                0.0, 1.0,
            ).with_anti_alias(false);
            let mut path = femtovg::Path::new();
            path.rect(ox, oy, shadow_img_w as f32, shadow_img_h as f32);
            cv.fill_path(&path, &paint);

            cv.delete_image(final_img);
        }
    }

    fn replay_glyph_run(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        font_data: &[u8],
        font_index: u32,
        _font_size: f32,
        normalized_coords: &[i16],
        paint_desc: &PaintDesc,
        y_offset: f32,
        glyphs: &[PositionedGlyph],
        is_stroke: bool,
    ) {
        let femtovg_paint = match self.desc_to_paint(paint_desc) {
            Some(p) => p,
            None => return,
        };
        let Some(font_id) = self.get_or_create_font(font_data, font_index) else { return; };

        let mapped: Vec<femtovg::PositionedGlyph> = glyphs.iter().map(|g| {
            femtovg::PositionedGlyph {
                x: g.x,
                y: g.y + y_offset,
                glyph_id: g.id,
            }
        }).collect();

        let mut cv = canvas.borrow_mut();
        if is_stroke {
            let _ = cv.stroke_glyph_run(
                font_id, normalized_coords, mapped.into_iter(), &femtovg_paint,
            );
        } else {
            let _ = cv.fill_glyph_run(
                font_id, normalized_coords, mapped.into_iter(), &femtovg_paint,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Path conversion helpers
// ---------------------------------------------------------------------------

fn rect_to_femtovg_path(rect: PhysicalRect) -> femtovg::Path {
    let mut p = femtovg::Path::new();
    p.rect(rect.origin.x, rect.origin.y, rect.size.width, rect.size.height);
    p
}

fn rounded_rect_to_femtovg_path(rect: PhysicalRect, radius: PhysicalBorderRadius) -> femtovg::Path {
    let mut p = femtovg::Path::new();
    if let Some(r) = radius.as_uniform() {
        if r > 0.0 {
            p.rounded_rect(
                rect.origin.x, rect.origin.y,
                rect.size.width, rect.size.height,
                r,
            );
        } else {
            p.rect(rect.origin.x, rect.origin.y,
                rect.size.width, rect.size.height);
        }
    } else {
        p.rounded_rect_varying(
            rect.origin.x, rect.origin.y,
            rect.size.width, rect.size.height,
            radius.top_left, radius.top_right,
            radius.bottom_right, radius.bottom_left,
        );
    }
    p
}

fn lyon_path_to_femtovg(events: &[PathEvent]) -> femtovg::Path {
    let mut p = femtovg::Path::new();
    for ev in events {
        match ev {
            PathEvent::MoveTo(x, y) => { p.move_to(*x, *y); }
            PathEvent::LineTo(x, y) => { p.line_to(*x, *y); }
            PathEvent::QuadTo(cx, cy, x, y) => { p.quad_to(*cx, *cy, *x, *y); }
            PathEvent::CubicTo(c1x, c1y, c2x, c2y, x, y) => {
                p.bezier_to(*c1x, *c1y, *c2x, *c2y, *x, *y);
            }
            PathEvent::Close => { p.close(); }
        }
    }
    p
}

fn to_femtovg_color(c: &Color) -> femtovg::Color {
    femtovg::Color::rgba(c.red(), c.green(), c.blue(), c.alpha())
}

fn blob_hash(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Legacy module stub for backward API compat
// ---------------------------------------------------------------------------
pub(crate) mod render_thread_legacy {
    pub struct PixelTarget {
        pub(crate) width: u32,
        pub(crate) height: u32,
        pub(crate) bytes: Vec<u8>,
    }

    impl PixelTarget {
        pub fn new(width: u32, height: u32) -> Result<Self, String> {
            if width == 0 || height == 0 {
                return Err("pixel target must be non-empty".into());
            }
            Ok(Self {
                width,
                height,
                bytes: vec![0u8; (width * height * 4) as usize],
            })
        }
        pub fn width(&self) -> u32 { self.width }
        pub fn height(&self) -> u32 { self.height }
        pub fn bytes_mut(&mut self) -> &mut [u8] { &mut self.bytes }
        pub fn mark_dirty(&mut self, _x: u32, _y: u32, _w: u32, _h: u32) {}
        pub fn mark_whole_dirty(&mut self) {}
        pub fn present(&mut self) {}
    }
}
