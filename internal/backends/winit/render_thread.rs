// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// dualslint — 2-thread render separation for the Slint GUI toolkit.
//
// This module is the cross-thread protocol and the render-thread GL driver.
// In the 2-thread split the UI thread encodes the scene graph into
// `SceneFrame`s (`snapshot.rs`); the render thread replays them against a
// FemtoVG/GL stack it owns entirely.
//
// With a render-owned component attached (`RenderHost::attach_component`) the
// screen is drawn *entirely* on the render thread: the app's component is
// instantiated here against a headless window adapter so the upstream Slint
// draw path (including text shaping) runs on this thread.  The render thread
// is then the visual authority — both the UI thread and worker threads are
// equal peers that borrow the published coordinate/state table and request
// changes through `RenderHost::apply_control_state`.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use i_slint_core::api::{PhysicalSize, Window as SlintApiWindow};
use i_slint_core::graphics::{Color, euclid};
use i_slint_core::input::{BackendMouseEvent, PointerEventButton};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::lengths::{
    LogicalLength, LogicalPoint, LogicalRect, PhysicalBorderRadius, PhysicalPx,
};
use i_slint_core::platform::{Platform, PlatformError, WindowEvent};
use i_slint_core::window::{WindowAdapter, WindowInner};

use crate::winit_compat::WindowSurfaceSizeExt;

// Re-exported by the sharedparley module (default i-slint-core feature);
// used only inside this file's render-thread text shaping.
use i_slint_core::textlayout::sharedparley::parley;

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

/// Shared coordinate table between the render thread (writer: publishes the
/// composited controls' geometry + state) and the UI thread / workers
/// (reader: hit-testing during event processing).  Initialised together with
/// the render host in `ensure_render_thread`.
pub(crate) static GLOBAL_COORDINATE_MAP: OnceLock<Arc<Mutex<CoordinateMap>>> = OnceLock::new();

/// Global HWND (Windows only) stored when the winit window is created.
#[cfg(target_os = "windows")]
pub(crate) static GLOBAL_HWND: OnceLock<isize> = OnceLock::new();

/// Global image sink callback.  Not used in the 2-thread GL path but retained
/// for backward compatibility with the CPU-raster API.
pub(crate) static GLOBAL_IMAGE_SINK: std::sync::Mutex<
    Option<Box<dyn Fn(i_slint_core::graphics::Image) + Send + Sync>>,
> = std::sync::Mutex::new(None);

// ---------------------------------------------------------------------------
// Headless window adapter — used when a render-owned component is attached
// ---------------------------------------------------------------------------

thread_local! {
    /// Stores the adapter created by the headless platform during
    /// `Platform::create_window_adapter` so the caller can retrieve it
    /// after `App::new()` wires everything up.
    static HEADLESS_ADAPTER_SLOT: OnceCell<Rc<dyn WindowAdapter>> = OnceCell::new();
}

/// Minimal window adapter for the render-thread component.  The Slint runtime
/// queries it for the window geometry; actual GL compositing is handled by the
/// render loop (`render_scene`).
struct RenderWindowAdapter {
    window: SlintApiWindow,
    size: Cell<PhysicalSize>,
    renderer: crate::renderer::dual::DualCoreRenderer,
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

    fn renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        // The render thread redraws on demand when a scene is submitted or
        // a control state is changed; nothing to do here.
    }
}

/// Headless platform for the render thread.  Only `create_window_adapter` is
/// implemented; the rest is handled by default trait methods.
struct RenderMirrorPlatform;

impl Platform for RenderMirrorPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let adapter: Rc<RenderWindowAdapter> =
            Rc::new_cyclic(|weak: &Weak<RenderWindowAdapter>| {
                let window = SlintApiWindow::new(weak.clone() as Weak<dyn WindowAdapter>);
                RenderWindowAdapter {
                    window,
                    size: Cell::new(PhysicalSize::new(800, 600)),
                    renderer: crate::renderer::dual::DualCoreRenderer::new(),
                }
            });
        let _ = HEADLESS_ADAPTER_SLOT.with(|slot| slot.set(adapter.clone()));
        Ok(adapter)
    }
}

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

/// A value to assign to a borrowed (lent-out) control property.  The render
/// thread converts it to the concrete Slint property type (upstream
/// semantics, including detaching a previous binding) on the mirror tree.
#[derive(Clone, Debug)]
pub enum ControlPropertyValue {
    /// For `Text.text`, `TextInput.text`, ...
    Text(String),
    /// RGBA, for `Rectangle.background`, `Text.color`, ...
    Color { r: u8, g: u8, b: u8, a: u8 },
    /// For `TouchArea.enabled` / `pressed` / `has-hover`, ...
    Bool(bool),
    /// For numeric properties (`opacity`, `width`, ...).
    Number(f32),
}

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
    Resize { width: u32, height: u32 },
    /// A complete scene snapshot from the UI thread's snapshot encoder.
    RenderScene { frame: SceneFrame },
    /// Replace the overlay layer composited on top of the UI scene.  May be
    /// sent from any thread; the render thread re-presents the retained UI
    /// scene together with the new overlay without waiting for the UI thread.
    SetOverlay {
        /// The overlay layer to composite on top of the UI scene.
        overlay: OverlayFrame,
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
    /// Re-encode the render thread's mirror component and re-present it.  An
    /// explicit redraw command; also used implicitly after every property
    /// assignment and control-state change.
    RequestRedraw,
    /// Forward the host's system accent colour (from the OS/xdg settings) to
    /// the mirror context.  The mirror has no OS connection of its own, so
    /// without this its widget palette would fall back to the default accent.
    SetAccent { color: Color },
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
    Solid { r: u8, g: u8, b: u8, a: u8 },
    /// Linear gradient.
    LinearGradient { start_x: f32, start_y: f32, end_x: f32, end_y: f32, stops: Vec<GradientStop> },
    /// Radial gradient.
    RadialGradient { cx: f32, cy: f32, radius: f32, stops: Vec<GradientStop> },
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

/// Logical-coordinate entry in the shared coordinate table.  Published by the
/// render thread as it composites the retained scene; read by the UI thread
/// for hit-testing during event processing (the pointer position is only
/// available on the UI thread, so the map never blocks the render thread).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ControlCoord {
    /// Control's top-left corner in logical pixels.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Current pointer-interaction state, updated by the render thread when
    /// it owns the widget state (see the event-protocol design).
    pub hovered: bool,
    pub pressed: bool,
}

impl ControlCoord {
    /// True when the logical point is inside this control's rectangle.
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// Shared, thread-safe table of control coordinates.  The render thread writes
/// (publish on composite); the UI thread and workers read (hit-test).  Access
/// via [`coordinate_map()`].
pub type CoordinateMap = std::collections::HashMap<u64, ControlCoord>;

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
    /// Deduplicated font payloads referenced by the glyph runs below. The
    /// `blob_id` is `Blob::id()` of the parley font data, stable across
    /// frames, so the render thread can cache the femtovg font id without
    /// re-hashing the font bytes.
    pub fonts: Vec<SceneFont>,
    pub commands: Vec<DrawCommand>,
    pub controls: Vec<ControlRegion>,
}

/// One unique font payload serialised for a frame.
#[derive(Clone, Debug)]
pub struct SceneFont {
    /// Stable `Blob::id()` of the parley font data.
    pub blob_id: u64,
    /// Index of the font in a collection, or 0 for a single font.
    pub font_index: u32,
    pub data: Vec<u8>,
}

/// A compositing layer drawn on top of the UI scene, submitted from any
/// thread without going through the UI thread.
///
/// Commands use the same physical-pixel coordinate space as
/// [`SceneFrame::commands`].  Submitting a new frame replaces the previous
/// overlay; an empty command list removes it.  Whenever such an overlay is
/// submitted (or the window is resized) while the UI thread is busy, the
/// render thread re-composites the last retained UI scene with the overlay
/// and presents it, so drawing does not stall behind the UI thread.
#[derive(Clone, Debug, Default)]
pub struct OverlayFrame {
    /// Font payloads referenced by the glyph runs in `commands`.
    pub fonts: Vec<SceneFont>,
    pub commands: Vec<DrawCommand>,
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
    /// Stroke a rounded rectangle (border stroke for buttons / text boxes).
    StrokeRoundedRect {
        rect: PhysicalRect,
        paint: PaintDesc,
        radius: PhysicalBorderRadius,
        line_width: f32,
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
    /// Draw a glyph run produced by sharedparley on the UI thread. The font
    /// payload is carried once per frame in `SceneFrame::fonts`, referenced
    /// here by its stable `Blob::id()`.
    DrawGlyphRun {
        font_blob_id: u64,
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
    /// Draw a text string that the render thread shapes itself with its own
    /// parley context (system fonts, no UI-thread involvement).  `x`/`y` is
    /// the baseline origin in physical pixels; `font_size` is in physical
    /// pixels; `max_width` wraps the line at the given physical-pixel width
    /// (`None` = single line).  Meant for overlay drawing driven by any app
    /// thread: only the string and geometry travel over the wire.
    DrawText {
        x: f32,
        y: f32,
        text: String,
        font_size: f32,
        paint: PaintDesc,
        max_width: Option<f32>,
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
    pub fn attach_component<F>(&self, factory: F)
    where
        F: FnOnce() -> Box<dyn std::any::Any> + Send + 'static,
    {
        self.attached.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.sender.send(RenderMessage::AttachComponent { factory: Box::new(factory) });
    }

    /// Request the render thread to apply a pointer state to the given control
    /// in its own component and redraw.  Ui thread and workers are equal
    /// peers here: both call this to indicate that a control should be
    /// hovered / pressed.
    pub fn apply_control_state(&self, id: u64, hovered: bool, pressed: bool) {
        let _ = self.sender.send(RenderMessage::ApplyControlState { id, hovered, pressed });
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

    /// Ask the render thread to re-encode its mirror component and re-present
    /// the frame (an explicit redraw command).
    pub fn request_redraw(&self) {
        let _ = self.sender.send(RenderMessage::RequestRedraw);
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

    /// Submit an overlay layer to composite on top of the UI scene.
    ///
    /// This is the UI-independent draw path: callable from any thread (the
    /// host is `Clone + Send + Sync`).  The render thread retains the last UI
    /// scene, so submitting an overlay immediately re-composites and presents
    /// the UI scene plus the new overlay without consulting the UI thread.
    pub fn submit_overlay(&self, overlay: OverlayFrame) {
        let _ = self.sender.send(RenderMessage::SetOverlay { overlay });
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

    /// Forward the host's system accent colour to the mirror context so the
    /// mirror widget palette matches what the host would render (checked
    /// boxes, highlights, ...).  No-op outside the dualslint 2-thread path.
    pub(crate) fn submit_accent(&self, color: Color) {
        let _ = self.sender.send(RenderMessage::SetAccent { color });
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
    /// Master overlay layer; re-applied to a fresh GL state after suspend.
    overlay: OverlayFrame,
}

impl RenderCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, host: RenderHost, frame_queue: FrameQueue) -> Self {
        Self { rx, host, frame_queue, overlay: OverlayFrame::default() }
    }

    /// Run the render-thread event loop.  Blocks until `Quit`.
    pub(crate) fn run(&mut self) {
        // Render-thread state: GL context + femtovg canvas, created on first
        // `Configure` message.
        let mut gl_state: Option<GlRenderState> = None;
        // Mirror state lives only here, on this thread — it would make
        // `RenderCore` non-Send otherwise.
        let mut render_window_adapter: Option<Rc<dyn WindowAdapter>> = None;
        let mut render_controls: HashMap<u64, ControlRegion> = HashMap::new();
        // Render-side item behind each control id, so property loans can
        // assign to the mirror tree item directly.  Stays on this thread.
        let mut render_item_rcs: HashMap<u64, ItemRc> = HashMap::new();
        let mut hovered_controls: HashSet<u64> = HashSet::new();
        let mut pressed_controls: HashSet<u64> = HashSet::new();
        // Keeps the mirror component tree alive for the lifetime of the
        // render thread (the strong handle owns the ItemTree).  Moved in
        // only from `AttachComponent`; never leaves this thread.
        let mut render_component: Option<Box<dyn std::any::Any>> = None;
        // Last system accent forwarded by the UI thread; applied to the
        // mirror context on attach in case the accent update arrives before
        // the mirror component exists.
        let mut accent: Option<Color> = None;

        while let Ok(msg) = self.rx.recv() {
            match msg {
                RenderMessage::Configure { window, width, height, scale_factor } => {
                    match GlRenderState::new(window, width, height, scale_factor) {
                        Ok(state) => {
                            gl_state = Some(state);
                            // If a mirror component was attached before the
                            // GL context existed, present it now.
                            if render_component.is_some() {
                                if let Some(frame) = re_encode_mirror(
                                    &render_window_adapter,
                                    &mut render_controls,
                                    &mut render_item_rcs,
                                ) {
                                    let overlay = &self.overlay;
                                    gl_state.as_mut().unwrap().render_scene(
                                        frame,
                                        overlay,
                                        &self.frame_queue,
                                        &self.host,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("dualslint render thread: GL init failed: {e}");
                        }
                    }
                }
                RenderMessage::Resize { width, height } => {
                    if let Some(state) = &mut gl_state {
                        state.resize(width, height);
                        // Re-composite the retained UI scene plus the overlay
                        // so the window is not stale while the UI thread is
                        // busy or suspended.
                        let overlay = &self.overlay;
                        state.repaint_retained(overlay);
                    }
                }
                RenderMessage::RenderScene { frame } => {
                    if let Some(state) = &mut gl_state {
                        let overlay = &self.overlay;
                        state.render_scene(frame, overlay, &self.frame_queue, &self.host);
                    }
                }
                RenderMessage::SetOverlay { overlay } => {
                    self.overlay = overlay;
                    let overlay = &self.overlay;
                    if let Some(state) = &mut gl_state {
                        state.repaint_retained(overlay);
                    }
                }
                RenderMessage::SetAccent { color } => {
                    accent = Some(color);
                    if let Some(adapter) = &render_window_adapter {
                        let context = WindowInner::from_pub(adapter.window()).context();
                        context.set_accent_color(color);
                    }
                }
                RenderMessage::User(f) => {
                    f();
                }
                RenderMessage::AttachComponent { factory } => {
                    // Seed the headless platform on this thread (ignore
                    // AlreadySet — set_platform succeeds as long as this
                    // thread's GLOBAL_CONTEXT is still free).
                    let _ = i_slint_core::platform::set_platform(Box::new(RenderMirrorPlatform));
                    render_component = Some(factory());
                    render_window_adapter = HEADLESS_ADAPTER_SLOT.with(|slot| slot.get().cloned());
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
                    if let Some(frame) = re_encode_mirror(
                        &render_window_adapter,
                        &mut render_controls,
                        &mut render_item_rcs,
                    ) {
                        if let Some(state) = &mut gl_state {
                            let overlay = &self.overlay;
                            state.render_scene(frame, overlay, &self.frame_queue, &self.host);
                        }
                    }
                }
                RenderMessage::ApplyControlState { id, hovered, pressed } => {
                    let Some(mirror) = &render_window_adapter else {
                        continue;
                    };
                    let Some(region) = render_controls.get(&id).cloned() else {
                        continue;
                    };
                    let geometry = region.geometry;
                    let center = LogicalPoint::from_lengths(
                        LogicalLength::new(geometry.min_x() + geometry.width() / 2.0),
                        LogicalLength::new(geometry.min_y() + geometry.height() / 2.0),
                    );
                    // Hover transition
                    if hovered && hovered_controls.insert(id) {
                        i_slint_core::api::Window::dispatch_event(
                            mirror.window(),
                            WindowEvent::internal(BackendMouseEvent::Moved {
                                position: center,
                                touch_finger_id: 0,
                            }),
                        );
                    } else if !hovered && hovered_controls.remove(&id) {
                        i_slint_core::api::Window::dispatch_event(
                            mirror.window(),
                            WindowEvent::internal(BackendMouseEvent::Moved {
                                position: center,
                                touch_finger_id: 0,
                            }),
                        );
                    }
                    // Press / release transitions: only send an edge so a
                    // held button keeps `pressed` visible across re-encodes.
                    if pressed && pressed_controls.insert(id) {
                        i_slint_core::api::Window::dispatch_event(
                            mirror.window(),
                            WindowEvent::internal(BackendMouseEvent::Pressed {
                                position: center,
                                button: PointerEventButton::Left,
                                click_count: 0,
                                touch_finger_id: 0,
                            }),
                        );
                    } else if !pressed && pressed_controls.remove(&id) {
                        i_slint_core::api::Window::dispatch_event(
                            mirror.window(),
                            WindowEvent::internal(BackendMouseEvent::Released {
                                position: center,
                                button: PointerEventButton::Left,
                                click_count: 0,
                                touch_finger_id: 0,
                            }),
                        );
                    }
                    // Re-encode and present from the mirror tree so the
                    // button state update is visible.
                    if let Some(frame) = re_encode_mirror(
                        &render_window_adapter,
                        &mut render_controls,
                        &mut render_item_rcs,
                    ) {
                        if let Some(state) = &mut gl_state {
                            let overlay = &self.overlay;
                            state.render_scene(frame, overlay, &self.frame_queue, &self.host);
                        }
                    }
                }
                RenderMessage::SetControlProperty { id, property, value, response } => {
                    let ok = render_item_rcs
                        .get(&id)
                        .map(|item_rc| apply_control_property(item_rc, &property, &value))
                        .unwrap_or(false);
                    let _ = response.send(ok);
                    // Re-encode and re-present so the borrowed property change
                    // is visible on screen (also refreshes the id→item map).
                    if ok {
                        if let Some(frame) = re_encode_mirror(
                            &render_window_adapter,
                            &mut render_controls,
                            &mut render_item_rcs,
                        ) {
                            if let Some(state) = &mut gl_state {
                                let overlay = &self.overlay;
                                state.render_scene(frame, overlay, &self.frame_queue, &self.host);
                            }
                        }
                    }
                }
                RenderMessage::HitTest { x, y, response } => {
                    // Smallest containing control wins, so nested elements hit
                    // before their parent rectangle.
                    let hit = render_controls
                        .values()
                        .filter(|r| r.geometry.contains(LogicalPoint::new(x, y)))
                        .min_by_key(|r| {
                            let g = r.geometry;
                            (g.width() * g.height()) as u32
                        })
                        .map(|r| r.id);
                    let _ = response.send(hit);
                }
                RenderMessage::RequestRedraw => {
                    if let Some(frame) = re_encode_mirror(
                        &render_window_adapter,
                        &mut render_controls,
                        &mut render_item_rcs,
                    ) {
                        if let Some(state) = &mut gl_state {
                            let overlay = &self.overlay;
                            state.render_scene(frame, overlay, &self.frame_queue, &self.host);
                        }
                    }
                }
                RenderMessage::Suspend => {
                    // Drop the GL context + canvas and release the winit window
                    // Arc so the UI thread can destroy the native window.
                    gl_state = None;
                    render_window_adapter = None;
                    render_component = None;
                    render_controls.clear();
                    render_item_rcs.clear();
                    hovered_controls.clear();
                    pressed_controls.clear();
                }
                RenderMessage::Quit => break,
            }
        }
    }

    pub(crate) fn host(&self) -> &RenderHost {
        &self.host
    }
}

/// Encode a scene frame from the render-owned mirror component, snapshot the
/// resulting control regions and the render-side item behind each control id.
/// Returns `None` if the adapter is not ready yet.
fn re_encode_mirror(
    render_window_adapter: &Option<Rc<dyn WindowAdapter>>,
    render_controls: &mut HashMap<u64, ControlRegion>,
    render_item_rcs: &mut HashMap<u64, ItemRc>,
) -> Option<SceneFrame> {
    let adapter = render_window_adapter.as_ref()?;
    let adapter: &dyn WindowAdapter = &**adapter;
    let (frame, item_refs) = crate::snapshot::encode_window_scene_full(adapter.window()).ok()?;
    *render_controls = frame.controls.iter().map(|r| (r.id, r.clone())).collect();
    render_item_rcs.clear();
    render_item_rcs.extend(item_refs);
    Some(frame)
}

/// Apply a dynamic, property-name based control property loan onto the
/// render-side mirror item.  Uses the upstream `Property::set` semantics (a
/// previous binding is detached), and clears the compiler's constant flag
/// first so bindings that compile to a literal value stay writable.  Returns
/// `false` if the item or property is not known.
fn apply_control_property(item_rc: &ItemRc, property: &str, value: &ControlPropertyValue) -> bool {
    use i_slint_core::graphics::{Brush, Color};
    fn force_set<T: Clone + PartialEq>(property: &i_slint_core::properties::Property<T>, value: T) {
        property.release_constant();
        property.set(value);
    }
    match value {
        ControlPropertyValue::Text(text) => {
            let text = i_slint_core::SharedString::from(text.as_str());
            if property == "text" {
                if let Some(item) = item_rc.downcast::<i_slint_core::items::ComplexText>() {
                    force_set(&item.as_pin_ref().get_ref().text, text);
                    return true;
                }
                if let Some(item) = item_rc.downcast::<i_slint_core::items::SimpleText>() {
                    force_set(&item.as_pin_ref().get_ref().text, text);
                    return true;
                }
                if let Some(item) = item_rc.downcast::<i_slint_core::items::TextInput>() {
                    force_set(&item.as_pin_ref().get_ref().text, text);
                    return true;
                }
            }
            false
        }
        ControlPropertyValue::Color { r, g, b, a } => {
            let brush = Brush::SolidColor(Color::from_argb_u8(*a, *r, *g, *b));
            if property == "background" {
                if let Some(item) = item_rc.downcast::<i_slint_core::items::Rectangle>() {
                    force_set(&item.as_pin_ref().get_ref().background, brush.clone());
                    return true;
                }
                if let Some(item) = item_rc.downcast::<i_slint_core::items::BasicBorderRectangle>()
                {
                    force_set(&item.as_pin_ref().get_ref().background, brush.clone());
                    return true;
                }
                if let Some(item) = item_rc.downcast::<i_slint_core::items::BorderRectangle>() {
                    force_set(&item.as_pin_ref().get_ref().background, brush);
                    return true;
                }
            }
            if property == "color" {
                if let Some(item) = item_rc.downcast::<i_slint_core::items::ComplexText>() {
                    force_set(&item.as_pin_ref().get_ref().color, brush.clone());
                    return true;
                }
                if let Some(item) = item_rc.downcast::<i_slint_core::items::SimpleText>() {
                    force_set(&item.as_pin_ref().get_ref().color, brush);
                    return true;
                }
            }
            false
        }
        ControlPropertyValue::Bool(value) => {
            if let Some(item) = item_rc.downcast::<i_slint_core::items::TouchArea>() {
                let item = item.as_pin_ref();
                match property {
                    "enabled" => force_set(&item.get_ref().enabled, *value),
                    "pressed" => force_set(&item.get_ref().pressed, *value),
                    "has-hover" => force_set(&item.get_ref().has_hover, *value),
                    _ => return false,
                }
                return true;
            }
            false
        }
        ControlPropertyValue::Number(_) => {
            // No numeric property is wired up yet.
            false
        }
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
        attached: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
pub fn coordinate_map() -> Option<Arc<Mutex<CoordinateMap>>> {
    GLOBAL_COORDINATE_MAP.get().cloned()
}

/// Replace the shared coordinate table with the given control regions.
/// Called on the render thread when a scene is composited, so the published
/// geometry always reflects what was actually drawn.
pub(crate) fn publish_control_coords(controls: &[ControlRegion]) {
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
                hovered: false,
                pressed: false,
            },
        );
    }
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

/// Render-thread-local text shaper for `DrawCommand::DrawText`.  Mirrors the
/// i-slint-core UI-side shaping recipe (build → `break_all_lines` → `align`)
/// against parley's system-font database, so the UI thread and any app thread
/// only pass the string and geometry.  Not `Send`/`Sync`: a single instance is
/// owned by the render thread inside `GlRenderState`.
struct TextShaper {
    fctx: parley::FontContext,
    lctx: parley::LayoutContext,
}

impl TextShaper {
    fn new() -> Self {
        Self { fctx: parley::FontContext::new(), lctx: parley::LayoutContext::new() }
    }
}

struct GlRenderState {
    window: Arc<winit::window::Window>,
    glutin_context: glutin::context::PossiblyCurrentContext,
    glutin_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    femtovg_canvas: RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
    femtovg_text_context: femtovg::TextContext,
    width: u32,
    height: u32,
    scale_factor: f32,
    /// The last UI scene snapshot, retained so an overlay update or a resize
    /// can be re-composited and presented without the UI thread.
    last_frame: Option<SceneFrame>,
    /// Font cache mapping (font blob id, index) → femtovg FontId.
    font_cache: RefCell<std::collections::HashMap<(u64, u32), femtovg::FontId>>,
    /// Texture cache for uploaded pixmaps.
    texture_cache: RefCell<std::collections::HashMap<u64, femtovg::ImageId>>,
    /// Layer texture cache keyed by (item ptr, index) → (origin, texture).
    layer_cache: RefCell<std::collections::HashMap<u64, (PhysicalPoint, femtovg::ImageId)>>,
    /// Render-thread-local parley context for `DrawText`.  Lazily
    /// initialised on first overlay text; only ever used on this thread.
    text_shaper: RefCell<Option<TextShaper>>,
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
        let mut canvas = femtovg::Canvas::new_with_text_context(backend, text_context.clone())
            .map_err(|e| format!("femtovg Canvas::new_with_text_context failed: {e}"))?;
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
            last_frame: None,
            font_cache: RefCell::new(std::collections::HashMap::new()),
            texture_cache: RefCell::new(std::collections::HashMap::new()),
            layer_cache: RefCell::new(std::collections::HashMap::new()),
            text_shaper: RefCell::new(None),
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

    /// Render the UI scene snapshot, retain it for later overlay/resize
    /// re-composites, and present it with the current overlay.
    fn render_scene(
        &mut self,
        frame: SceneFrame,
        overlay: &OverlayFrame,
        _frame_queue: &FrameQueue,
        _host: &RenderHost,
    ) {
        // Publish the controls that are actually part of this composited
        // frame into the shared coordinate table, so the UI thread can
        // hit-test during event processing from the render thread's (the
        // authority's) view without blocking it.
        publish_control_coords(&frame.controls);
        self.last_frame = Some(frame);
        let Some(frame) = &self.last_frame else { return };
        self.composite_and_present(frame, overlay);
    }

    /// Re-composite the retained UI scene with the given overlay and present
    /// it.  Used for overlay updates and resizes that must not depend on the
    /// UI thread.  A no-op when no UI scene has been drawn yet.
    fn repaint_retained(&self, overlay: &OverlayFrame) {
        let Some(frame) = &self.last_frame else { return };
        self.composite_and_present(frame, overlay);
    }

    /// Clear the canvas, replay the UI scene commands followed by the overlay
    /// commands, and present the result.
    fn composite_and_present(&self, frame: &SceneFrame, overlay: &OverlayFrame) {
        use glutin::prelude::*;

        self.register_fonts(&frame.fonts);
        self.register_fonts(&overlay.fonts);

        let canvas = &self.femtovg_canvas;

        // Set size and reset
        {
            let mut cv = canvas.borrow_mut();
            cv.set_size(self.width, self.height, self.scale_factor.ceil() as _);
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
            cv.clear_rect(0, 0, self.width, self.height, clear);
        }

        // Replay the UI scene first, then the overlay on top.
        for cmd in &frame.commands {
            self.replay_command(canvas, cmd);
        }
        for cmd in &overlay.commands {
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
            DrawCommand::StrokeRoundedRect { rect, paint, radius, line_width, anti_alias } => {
                let paint_f = self.desc_to_paint_stroke(paint);
                if let Some(mut p) = paint_f {
                    p.set_line_width(*line_width);
                    p.set_anti_alias(*anti_alias);
                    let path = rounded_rect_to_femtovg_path(*rect, *radius);
                    canvas.borrow_mut().stroke_path(&path, &p);
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
                font_blob_id, font_index, font_size, normalized_coords,
                paint, y_offset, glyphs, is_stroke,
            } => {
                self.replay_glyph_run(canvas, *font_blob_id, *font_index, *font_size,
                    normalized_coords, paint, *y_offset, glyphs, *is_stroke);
            }
            DrawCommand::DrawText { x, y, text, font_size, paint, max_width } => {
                self.replay_text(canvas, *x, *y, text, *font_size, paint, *max_width);
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
                // Skip re-uploading a texture that is already cached; when a
                // retained frame is re-composited (overlay update / resize)
                // the same commands are replayed without re-wasting GPU uploads.
                if !self.texture_cache.borrow().contains_key(key) {
                    self.upload_pixmap(canvas, *key, pixels, *width, *height);
                }
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

    /// Ensure every font in the frame is registered in the femtovg font
    /// context. Keyed by the stable parley blob id, so the same font is only
    /// added once for the lifetime of the render thread.
    fn register_fonts(&self, fonts: &[SceneFont]) {
        let mut cache = self.font_cache.borrow_mut();
        for font in fonts {
            let key = (font.blob_id, font.font_index);
            if cache.contains_key(&key) {
                continue;
            }
            if let Some(font_id) = self
                .femtovg_text_context
                .add_shared_font_with_index(font.data.clone(), font.font_index)
                .ok()
            {
                cache.insert(key, font_id);
            }
        }
    }

    /// Resolve a font id for a glyph run by its stable blob id and index.
    fn get_or_create_font(&self, blob_id: u64, font_index: u32) -> Option<femtovg::FontId> {
        self.font_cache.borrow().get(&(blob_id, font_index)).copied()
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
        font_blob_id: u64,
        font_index: u32,
        font_size: f32,
        normalized_coords: &[i16],
        paint_desc: &PaintDesc,
        y_offset: f32,
        glyphs: &[PositionedGlyph],
        is_stroke: bool,
    ) {
        let mut femtovg_paint = match self.desc_to_paint(paint_desc) {
            Some(p) => p,
            None => return,
        };
        femtovg_paint.set_font_size(font_size);
        let Some(font_id) = self.get_or_create_font(font_blob_id, font_index) else { return; };

        let mapped: Vec<femtovg::PositionedGlyph> = glyphs.iter().map(|g| {
            femtovg::PositionedGlyph {
                x: g.x,
                y: g.y + y_offset,
                glyph_id: g.id,
            }
        }).collect();

        // Pixel-align the canvas during text rendering, mirroring upstream
        // slint's align_canvas_during(): flush a translate-only transform to
        // integer pixels so glyphs rasterize on a crisp pixel grid.
        let original = canvas.borrow().transform();
        let [a, b, c, d, x, y] = original.0;
        let translate_only = (a - 1.0).abs() < 1e-3
            && b.abs() < 1e-3
            && c.abs() < 1e-3
            && (d - 1.0).abs() < 1e-3;
        if translate_only {
            let floored = femtovg::Transform2D::new(
                a.round(), b.round(), c.round(), d.round(), x.round(), y.round(),
            );
            let mut cv = canvas.borrow_mut();
            cv.reset_transform();
            cv.set_transform(&floored);
            if is_stroke {
                let _ = cv.stroke_glyph_run(
                    font_id, normalized_coords, mapped.into_iter(), &femtovg_paint,
                );
            } else {
                let _ = cv.fill_glyph_run(
                    font_id, normalized_coords, mapped.into_iter(), &femtovg_paint,
                );
            }
            cv.reset_transform();
            cv.set_transform(&original);
        } else {
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

    /// Shape and draw a `DrawCommand::DrawText` payload on the render thread.
    /// The string is shaped with the render thread's own parley database
    /// (`TextShaper`), so no UI-thread font table or worker-side shaping is
    /// involved; each frame's `SceneFrame::fonts` stays untouched.  Font data
    /// discovered by parley is registered into the femtoVg font cache on
    /// demand, keyed by `(blob id, font index)` like UI-side glyph runs.
    fn replay_text(
        &self,
        canvas: &RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>,
        x: f32,
        y: f32,
        text: &str,
        font_size: f32,
        paint_desc: &PaintDesc,
        max_width: Option<f32>,
    ) {
        if text.is_empty() {
            return;
        }
        if self.text_shaper.borrow().is_none() {
            *self.text_shaper.borrow_mut() = Some(TextShaper::new());
        }
        let mut shaper_guard = self.text_shaper.borrow_mut();
        let shaper = shaper_guard.as_mut().unwrap();

        let mut layout = {
            let mut builder =
                shaper.lctx.ranged_builder(&mut shaper.fctx, text, self.scale_factor, true);
            builder.push_default(parley::StyleProperty::FontSize(font_size));
            builder.build(text)
        };
        layout.break_all_lines(max_width);
        layout.align(parley::Alignment::Start, parley::AlignmentOptions::default());

        let mut baseline_offset = None;
        let mut seen_fonts = HashSet::new();
        for line in layout.lines() {
            for item in line.items() {
                let parley::PositionedLayoutItem::GlyphRun(run) = item else { continue; };
                let font = run.run().font();
                let blob_id = font.data.id();
                let font_index = font.index;
                if seen_fonts.insert((blob_id, font_index))
                    && !self.font_cache.borrow().contains_key(&(blob_id, font_index))
                {
                    if let Some(font_id) = self
                        .femtovg_text_context
                        .add_shared_font_with_index(font.data.data().to_vec(), font_index)
                        .ok()
                    {
                        self.font_cache.borrow_mut().insert((blob_id, font_index), font_id);
                    }
                }
                let glyphs: Vec<PositionedGlyph> = run.positioned_glyphs().map(|g| {
                    PositionedGlyph { x: x + g.x, y: g.y, id: g.id as u16 }
                }).collect();
                if glyphs.is_empty() {
                    continue;
                }
                // parley positions every glyph of a line at that line's
                // baseline offset (`g.y`, Y-down from the layout origin; all
                // glyphs of one line share it, later lines carry an
                // additional line-height delta).  `y` in DrawCommand::DrawText
                // is the *first* line's baseline, so the first line's `g.y`
                // is folded out of every run; later lines then stack below
                // `y` at their own line delta instead of collapsing onto the
                // first line ("Hello World 12345" wraps at max_width).
                if baseline_offset.is_none() {
                    baseline_offset = Some(glyphs[0].y);
                }
                self.replay_glyph_run(
                    canvas,
                    blob_id,
                    font_index,
                    run.run().font_size(),
                    &[],
                    paint_desc,
                    y - baseline_offset.unwrap(),
                    &glyphs,
                    false,
                );
            }
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
