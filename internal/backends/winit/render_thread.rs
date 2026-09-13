// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// This file is part of the dualslint fork (https://github.com/akenejie/dualslint),
// a fork of slint-ui/slint. This file is new in the fork; where it builds on the
// slint code base, those parts remain under slint's own license terms. The
// fork's modifications and additions are AGPL-3.0-only, Copyright © akenejie.

//! DESIGN REWRITE X (fork):
//!
//! The scene graph lives on the UI thread (normal slint `ui.run()` semantics).
//! This module provides the render-side CPU raster host: a dedicated thread that
//! receives pixel-paint closures from the application's worker threads, rasterizes
//! into a CPU pixel buffer, and hands the resulting frames back to the UI thread
//! for GL texture upload and display.
//!
//! The UI thread owns the winit event loop, HWND, scene graph, femtovg renderer,
//! GL textures, and presenter.  The render thread owns only the CPU pixel buffers;
//! it never touches GL or processes winit events.

use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::sync::OnceLock;

/// Global frame queue shared between the render thread and the UI thread.
/// Initialized when the backend starts; read by the UI thread's event handler.
pub(crate) static GLOBAL_FRAME_QUEUE: OnceLock<FrameQueue> = OnceLock::new();

/// Global render host handle for apps to call `paint()`.
/// Initialized when the backend starts; accessible via [`host()`].
pub(crate) static GLOBAL_RENDER_HOST: OnceLock<RenderHost> = OnceLock::new();

/// Global HWND (Windows window handle) stored when the winit window is created.
/// Accessible via [`hwnd()`].
#[cfg(target_os = "windows")]
pub(crate) static GLOBAL_HWND: OnceLock<isize> = OnceLock::new();

/// Global image sink callback.  Set by the app; invoked by the UI thread
/// when a new frame is ready from the render thread.
pub(crate) static GLOBAL_IMAGE_SINK: std::sync::Mutex<
    Option<Box<dyn Fn(i_slint_core::graphics::Image) + Send + Sync>>,
> = std::sync::Mutex::new(None);

/// Obtain the [`RenderHost`] for sending paint closures to the render thread.
/// Returns `None` if the dualslint backend has not been started yet.
pub fn host() -> Option<RenderHost> {
    GLOBAL_RENDER_HOST.get().cloned()
}

/// Register a callback that receives images from the render thread.
/// Called by the app to wire render thread output to a Slint `Image` property.
/// Can be called multiple times; the latest callback wins.
pub fn set_image_sink<F>(sink: F)
where
    F: Fn(i_slint_core::graphics::Image) + Send + Sync + 'static,
{
    if let Ok(mut guard) = GLOBAL_IMAGE_SINK.lock() {
        *guard = Some(Box::new(sink));
    }
}

/// Returns the raw HWND (window handle) of the slint window, if available.
/// On non-Windows platforms this always returns `None`.
#[cfg(target_os = "windows")]
pub fn hwnd() -> Option<isize> {
    GLOBAL_HWND.get().copied()
}

/// Store the HWND when the winit window is created.  Called internally by
/// the window adapter; not part of the public API.
#[cfg(target_os = "windows")]
pub(crate) fn set_hwnd(hwnd: isize) {
    let _ = GLOBAL_HWND.set(hwnd);
}

/// Returns the raw HWND (window handle) of the slint window, if available.
/// On non-Windows platforms this always returns `None`.
#[cfg(not(target_os = "windows"))]
pub fn hwnd() -> Option<isize> {
    None
}

/// A completed frame ready for display on the UI thread.
pub struct Frame {
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Raw RGBA8 pixel data (width * height * 4 bytes).
    pub pixels: Vec<u8>,
}

/// Shared frame queue between the render thread and the UI thread.
pub(crate) type FrameQueue = Arc<Mutex<VecDeque<Frame>>>;

/// Messages sent from the UI thread or app workers to the render thread.
pub enum RenderMessage {
    /// A user callback to run on the render thread.
    User(Box<dyn FnOnce() + Send>),
    /// A deferred pixel-paint closure: run on the render thread with exclusive
    /// `&mut PixelTarget` access.  The CPU buffer is (re)created to
    /// `width` x `height` before the closure runs.
    Paint {
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
        /// The paint closure to run on the render thread.
        f: Box<dyn FnOnce(&mut PixelTarget) + Send>,
    },
    /// Request the UI thread to redraw (signals via the registered callback).
    Redraw,
    /// Quit the render thread loop.
    Quit,
}

/// The send half, shared with the UI thread and app workers so paint closures
/// and redraw requests can be pushed onto the render thread.
#[derive(Clone)]
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,
    event_loop_proxy: Option<winit::event_loop::EventLoopProxy<crate::SlintEvent>>,
}

impl RenderHost {
    /// Schedule an arbitrary closure to run on the render thread.
    pub fn send_user(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.sender.send(RenderMessage::User(Box::new(f)));
    }

    /// Ask the render thread to quit.
    pub fn send_quit(&self) {
        let _ = self.sender.send(RenderMessage::Quit);
    }

    /// Ask the render thread to signal a UI-side redraw.
    pub fn send_redraw(&self) {
        let _ = self.sender.send(RenderMessage::Redraw);
    }

    /// Schedule a pixel-paint closure.  The closure runs on the render thread
    /// with exclusive `&mut PixelTarget` access.  When the closure calls
    /// [`PixelTarget::present`], the frame is pushed to the shared frame queue
    /// for the UI thread to pick up and display.
    pub fn paint<F>(&self, width: u32, height: u32, f: F)
    where
        F: FnOnce(&mut PixelTarget) + Send + 'static,
    {
        let _ = self
            .sender
            .send(RenderMessage::Paint { width, height, f: Box::new(f) });
    }
}

/// CPU pixel buffer for the render thread.  The application rasterizes
/// directly into this buffer inside a [`RenderHost::paint`] closure,
/// marks the affected rectangles dirty, and calls [`Self::present`].
///
/// When `present()` is called, the entire buffer is cloned and pushed to the
/// shared frame queue for the UI thread to upload to a GL texture.
///
/// The buffer is tightly packed RGBA8, row-major, `width * height * 4` bytes,
/// row 0 = top.
///
/// This type is *not* `Send` and lives on the render thread only.
pub struct PixelTarget {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    dirty: Vec<[u32; 4]>,
    frame_queue: FrameQueue,
}

impl PixelTarget {
    /// Create a new pixel target with a CPU buffer of the given dimensions.
    pub(crate) fn new(width: u32, height: u32, frame_queue: FrameQueue) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("render thread: pixel target must be non-empty".into());
        }
        Ok(Self {
            width,
            height,
            bytes: vec![0u8; width as usize * height as usize * 4],
            dirty: Vec::new(),
            frame_queue,
        })
    }

    /// Returns the buffer width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }
    /// Returns the buffer height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Returns a mutable reference to the raw RGBA8 pixel buffer.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
    /// Mark a rectangle as dirty for the next upload.
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
    /// Mark the entire buffer as dirty.
    pub fn mark_whole_dirty(&mut self) {
        self.mark_dirty(0, 0, self.width, self.height);
    }

    /// Finalize the current frame and push it to the shared frame queue.
    /// The UI thread will pick it up, upload to a GL texture, and display.
    pub fn present(&mut self) {
        self.dirty.clear();
        let frame = Frame {
            width: self.width,
            height: self.height,
            pixels: self.bytes.clone(),
        };
        self.frame_queue.lock().unwrap().push_back(frame);
    }
}

/// The render thread core.  Created internally by the backend; driven on the
/// render thread via [`RenderHostCore::run`].
pub(crate) struct RenderHostCore {
    rx: mpsc::Receiver<RenderMessage>,
    host: RenderHost,
    frame_queue: FrameQueue,
}

impl RenderHostCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, host: RenderHost, frame_queue: FrameQueue) -> Self {
        Self {
            rx,
            host,
            frame_queue,
        }
    }

    pub(crate) fn run(&mut self) {
        let mut pixel_target: Option<PixelTarget> = None;
        while let Ok(msg) = self.rx.recv() {
            match msg {
                RenderMessage::User(f) => f(),
                RenderMessage::Paint { width, height, f } => {
                    if pixel_target.is_none()
                        || pixel_target.as_ref().is_some_and(|t| t.width != width || t.height != height)
                    {
                        match PixelTarget::new(width, height, self.frame_queue.clone()) {
                            Ok(t) => pixel_target = Some(t),
                            Err(e) => {
                                eprintln!("render thread: pixel target create: {e}");
                                continue;
                            }
                        }
                    }
                    f(pixel_target.as_mut().unwrap());
                    // Notify the UI thread that a new frame is ready.
                    if let Some(proxy) = &self.host.event_loop_proxy {
                        let _ = proxy.send_event(crate::SlintEvent(crate::event_loop::CustomEvent::RenderFrame));
                    }
                }
                RenderMessage::Redraw => {
                    // Redraw is handled by the RenderFrame notification after Paint.
                }
                RenderMessage::Quit => break,
            }
        }
    }

    /// Returns a reference to the [`RenderHost`] for sending messages from
    /// other threads.
    #[allow(dead_code)]
    pub fn host(&self) -> &RenderHost {
        &self.host
    }
}

/// Create the render-thread channel pair.  Returns:
/// - `RenderHost` — the send half for paint closures and control messages
/// - `RenderHostCore` — the receive half + loop driver (runs on the render thread)
/// - `FrameQueue` — the shared queue for completed frames (UI thread reads from this)
pub(crate) fn channel(
    event_loop_proxy: winit::event_loop::EventLoopProxy<crate::SlintEvent>,
) -> (RenderHost, RenderHostCore, FrameQueue) {
    let (tx, rx) = mpsc::channel();
    let frame_queue: FrameQueue = Arc::new(Mutex::new(VecDeque::new()));
    let host = RenderHost {
        sender: tx,
        event_loop_proxy: Some(event_loop_proxy),
    };
    let core = RenderHostCore::new(rx, host.clone(), frame_queue.clone());
    (host, core, frame_queue)
}
