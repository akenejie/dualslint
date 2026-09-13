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
//! into a double-buffered GL texture via a context *shared* with the UI thread's
//! femtovg context (wglShareLists on Windows), and hands the resulting
//! [`BorrowedOpenGLTexture`] back to the UI thread for display.
//!
//! The UI thread owns the winit event loop, HWND, scene graph, femtovg renderer
//! and presenter.  It never touches the CPU pixel buffers.  The render thread
//! owns only the pixel data and the shared GL textures; it never processes winit
//! events.

use std::num::NonZeroU32;
use std::sync::mpsc;
use i_slint_core::graphics::{
    BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, Image, IntSize,
};
use glow::HasContext;

/// Messages sent from the UI thread or app workers to the render thread.
pub enum RenderMessage {
    /// A user callback to run on the render thread.
    User(Box<dyn FnOnce() + Send>),
    /// A deferred pixel-paint closure: run on the render thread with exclusive
    /// `&mut PixelTarget` access.  The double buffer is (re)created to
    /// `width` x `height` before the closure runs.
    Paint {
        width: u32,
        height: u32,
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
    /// with exclusive `&mut PixelTarget` access.
    pub fn paint<F>(&self, width: u32, height: u32, f: F)
    where
        F: FnOnce(&mut PixelTarget) + Send + 'static,
    {
        let _ = self
            .sender
            .send(RenderMessage::Paint { width, height, f: Box::new(f) });
    }
}

/// CPU side of the persistent backing texture.  The application rasterizes
/// directly into the pixel buffer inside a [`RenderHost::paint`] closure,
/// marks the affected rectangles dirty, and calls [`Self::present`].
///
/// The render loop then advertises a new frame via the registered sink and
/// uploads the dirty rectangles to a double-buffered GL texture that is
/// *shared* with the UI thread's femtovg context.
///
/// The buffer is tightly packed RGBA8, row-major, `width * height * 4` bytes,
/// row 0 = top.
///
/// This type is *not* `Send` and lives on the render thread only.
pub struct PixelTarget {
    gl: std::rc::Rc<glow::Context>,
    textures: [glow::Texture; 2],
    texture_ids: [NonZeroU32; 2],
    width: u32,
    height: u32,
    publish_seq: u32,
    bytes: Vec<u8>,
    dirty: Vec<[u32; 4]>,
    presented: bool,
}

impl PixelTarget {
    /// Create a new pixel target backed by the given glow context.
    /// The GL context must be *current* on the calling (render) thread and
    /// must share texture space with the UI thread's femtovg context
    /// (e.g. via `wglShareLists`).
    pub(crate) fn new(
        gl: std::rc::Rc<glow::Context>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("render thread: pixel target must be non-empty".into());
        }
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
                Ok::<glow::Texture, String>(t)
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

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
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
    pub fn mark_whole_dirty(&mut self) {
        self.mark_dirty(0, 0, self.width, self.height);
    }
    pub fn present(&mut self) {
        self.presented = true;
    }

    /// Consume the pending frame: upload dirty rects, then build a
    /// [`BorrowedOpenGLTexture`] referencing the current texture and pass it
    /// to the sink.  Returns `true` if a frame was advertised.
    fn present_pending(
        &mut self,
        sink: &Option<Box<dyn Fn(Image) + Send + 'static>>,
    ) -> bool {
        if !self.presented {
            return false;
        }
        self.presented = false;
        self.upload_dirty();
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
        true
    }
}

impl Drop for PixelTarget {
    fn drop(&mut self) {
        for texture in self.textures {
            unsafe { self.gl.delete_texture(texture) };
        }
    }
}

/// The render thread core.  Created by [`channel()`]; the application calls
/// [`RenderHostCore::run`] (blocking) on the render thread to drive the
/// message loop.
pub struct RenderHostCore {
    rx: mpsc::Receiver<RenderMessage>,
    host: RenderHost,
    redraw_request: Option<Box<dyn Fn() + Send + 'static>>,
    image_sink: Option<Box<dyn Fn(Image) + Send + 'static>>,
}

impl RenderHostCore {
    fn new(rx: mpsc::Receiver<RenderMessage>, host: RenderHost) -> Self {
        Self {
            rx,
            host,
            redraw_request: None,
            image_sink: None,
        }
    }

    pub fn set_redraw_request<F>(&mut self, f: F)
    where
        F: Fn() + Send + 'static,
    {
        self.redraw_request = Some(Box::new(f));
    }

    pub fn set_image_sink<F>(&mut self, sink: F)
    where
        F: Fn(Image) + Send + 'static,
    {
        self.image_sink = Some(Box::new(sink));
    }

    pub fn run(&mut self) {
        let mut pixel_target: Option<PixelTarget> = None;
        while let Ok(msg) = self.rx.recv() {
            match msg {
                RenderMessage::User(f) => f(),
                RenderMessage::Paint { width, height, f } => {
                    if let Some(target) = pixel_target.as_mut() {
                        if target.width != width || target.height != height {
                            eprintln!(
                                "render thread: paint size mismatch: target={}x{}, requested={}x{}",
                                target.width, target.height, width, height
                            );
                        }
                        f(target);
                    }
                }
                RenderMessage::Redraw => {
                    let advertised = pixel_target
                        .as_mut()
                        .map(|t| t.present_pending(&self.image_sink))
                        .unwrap_or(false);
                    if advertised {
                        if let Some(cb) = &self.redraw_request {
                            cb();
                        }
                    }
                }
                RenderMessage::Quit => break,
            }
        }
    }

    pub fn host(&self) -> &RenderHost {
        &self.host
    }
}

/// Create the render-thread channel.  The UI thread (or app) keeps the
/// returned [`RenderHost`] to send paint closures and redraw requests;
/// the returned [`RenderHostCore`] must be driven on the render thread
/// via [`RenderHostCore::run`].
pub fn channel() -> (RenderHost, RenderHostCore) {
    let (tx, rx) = mpsc::channel();
    let host = RenderHost { sender: tx };
    let core = RenderHostCore::new(rx, host.clone());
    (host, core)
}
