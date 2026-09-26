// Copyright © akenejie <mailto:akenejie@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-only

//! CPU presentation for the android-activity backend.
//!
//! The render thread rasterises straight into the window's pixel buffer
//! instead of going through a GL/wgpu surface: it locks the `ANativeWindow`,
//! wraps the locked bytes in a Skia raster surface, draws the frame, and drops
//! the lock, which posts it to the compositor.  Locking is a platform-level
//! handshake, so this stays on the render thread — the UI thread only ever
//! hands over the window and asks for a redraw.
//!
//! Unlike a GPU surface this path cannot report a buffer age or damage
//! rectangles, so every frame is a full redraw.

use i_slint_core::api::{PhysicalSize as PhysicalWindowSize, Window};
use i_slint_core::graphics::RequestedGraphicsAPI;
use i_slint_core::partial_renderer::DirtyRegion;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::DrawOutcome;
use i_slint_renderer_skia::{SkiaSharedContext, Surface, skia_safe};
use std::any::Any;
use std::cell::RefCell;
use std::sync::Arc;

use crate::android_activity::ndk::hardware_buffer_format::HardwareBufferFormat;
use crate::android_activity::ndk::native_window::NativeWindow;

/// A [`Surface`] that rasterises into the native window's CPU buffer.
pub struct NativeWindowSurface {
    window: Arc<NativeWindow>,
    size: RefCell<PhysicalWindowSize>,
}

impl NativeWindowSurface {
    /// `window` is shared, not owned: the activity keeps the window alive and
    /// the render thread only borrows it for the duration of a frame.
    pub fn new(window: Arc<NativeWindow>, size: PhysicalWindowSize) -> Self {
        Self { window, size: RefCell::new(size) }
    }
}

impl Surface for NativeWindowSurface {
    fn new(
        _shared_context: &SkiaSharedContext,
        _window_handle: Arc<dyn raw_window_handle::HasWindowHandle + Sync + Send>,
        _display_handle: Arc<dyn raw_window_handle::HasDisplayHandle + Sync + Send>,
        _size: PhysicalWindowSize,
        _requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, PlatformError> {
        // Only reached through `SkiaRenderer`'s default surface lookup, which
        // this backend bypasses: it needs the concrete `NativeWindow` to lock,
        // and that cannot be recovered from an erased `HasWindowHandle`.
        Err("the android native window surface needs a concrete NativeWindow".into())
    }

    fn name(&self) -> &'static str {
        "android-native-window"
    }

    fn render(
        &self,
        _window: &Window,
        _size: PhysicalWindowSize,
        render_callback: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        pre_present_callback: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<DrawOutcome, PlatformError> {
        // Locking the whole buffer: the compositor copies it out on unlock, so
        // the UI thread must not read the window while this is held.
        let mut buffer =
            self.window.lock(None).map_err(|e| format!("failed to lock the native window: {e}"))?;

        let (width, height) = (buffer.width(), buffer.height());
        let (color_type, alpha_type) = skia_format(buffer.format())?;
        let bytes_per_pixel = buffer
            .format()
            .bytes_per_pixel()
            .ok_or("the native window buffer has no known byte size per pixel")?;
        // `stride` counts pixels, and a line can be padded beyond `width`.
        let row_bytes = buffer.stride() * bytes_per_pixel;

        let info = skia_safe::ImageInfo::new(
            (width as i32, height as i32),
            color_type,
            alpha_type,
            skia_safe::ColorSpace::new_srgb(),
        );
        let pixels: &mut [u8] = match buffer.bytes() {
            // Every bit pattern is a valid `u8`, so the uninitialised buffer
            // can be used as-is; Skia overwrites the visible pixels.
            Some(bytes) => {
                let (ptr, len) = (bytes.as_mut_ptr().cast::<u8>(), bytes.len());
                unsafe { std::slice::from_raw_parts_mut(ptr, len) }
            }
            None => return Err("failed to access the native window buffer".into()),
        };
        let mut surface = skia_safe::surfaces::wrap_pixels(&info, pixels, Some(row_bytes), None)
            .ok_or("failed to wrap the native window buffer as a Skia raster surface")?;

        {
            let canvas = surface.canvas();
            // Buffer age 0: there is no way to ask the compositor which parts
            // of the previous frame survived, so redraw everything. A raster
            // surface writes straight into `pixels`, so there is nothing to
            // flush.
            let _dirty_region = render_callback(canvas, None, 0);
        }
        drop(surface);

        if let Some(callback) = pre_present_callback.borrow_mut().as_mut() {
            callback();
        }

        // Dropping the guard unlocks and posts the buffer.
        Ok(DrawOutcome::Success)
    }

    fn resize_event(&self, size: PhysicalWindowSize) -> Result<(), PlatformError> {
        *self.size.borrow_mut() = size;
        Ok(())
    }

    fn bits_per_pixel(&self) -> Result<u8, PlatformError> {
        Ok(32)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Maps a native window buffer format onto the matching Skia color type.
fn skia_format(
    format: HardwareBufferFormat,
) -> Result<(skia_safe::ColorType, skia_safe::AlphaType), PlatformError> {
    Ok(match format {
        HardwareBufferFormat::R8G8B8A8_UNORM => {
            (skia_safe::ColorType::RGBA8888, skia_safe::AlphaType::Premul)
        }
        // Same byte order, but the fourth byte is padding rather than alpha.
        HardwareBufferFormat::R8G8B8X8_UNORM => {
            (skia_safe::ColorType::RGBA8888, skia_safe::AlphaType::Opaque)
        }
        other => return Err(format!("unsupported native window buffer format {other:?}").into()),
    })
}
