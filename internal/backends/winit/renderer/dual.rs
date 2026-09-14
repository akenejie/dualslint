// Copyright © akenejie
// SPDX-License-Identifier: AGPL-3.0-only
//
// dualslint — 2-thread render separation for the Slint GUI toolkit.
//
// `DualThreadRenderer` is the UI-thread companion of the GL render thread.
// It never touches a GL context: it serialises the item tree into a
// `SceneFrame` via the snapshot encoder and forwards it to the render thread,
// which owns the glutin context, the FemtoVG canvas and the swap chain.
//
// The winit `Window` is created on the UI thread (so input and OS events
// keep their home there); its raw handles are handed to the render thread
// through `RenderMessage::Configure`.

use std::rc::{Rc, Weak};
use std::sync::Arc;

use i_slint_core::api::Window as SlintApiWindow;
use i_slint_core::item_rendering::ItemRenderer;
use i_slint_core::lengths::ScaleFactor;
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::{DrawOutcome, Renderer, RendererSealed};
use i_slint_core::window::{WindowAdapter, WindowInner};
use winit::event_loop::ActiveEventLoop;

use crate::SharedBackendData;
use crate::renderer::WinitCompatibleRenderer;
use crate::render_thread::GLOBAL_RENDER_HOST;

/// The core `Renderer` served to the Slint runtime.
///
/// All text measuring goes through the `shared-parley` defaults, which read
/// the font context of the bound Slint context.  `resize` forwards to the
/// render thread so its surface stays in sync with the native window.
pub struct DualCoreRenderer {
    window_adapter: std::cell::RefCell<Option<Rc<dyn WindowAdapter>>>,
}

impl DualCoreRenderer {
    fn new() -> Self {
        Self { window_adapter: Default::default() }
    }
}

impl RendererSealed for DualCoreRenderer {
    fn set_window_adapter(&self, window_adapter: &Rc<dyn WindowAdapter>) {
        *self.window_adapter.borrow_mut() = Some(window_adapter.clone());
    }

    fn window_adapter(&self) -> Option<Rc<dyn WindowAdapter>> {
        self.window_adapter.borrow().clone()
    }

    fn supports_transformations(&self) -> bool {
        true
    }

    fn resize(&self, size: i_slint_core::api::PhysicalSize) -> Result<(), PlatformError> {
        if let Some(host) = GLOBAL_RENDER_HOST.get() {
            host.submit_resize(size.width, size.height);
        }
        Ok(())
    }
}

/// The UI-thread half of the 2-thread split.
pub struct DualThreadRenderer {
    core_renderer: DualCoreRenderer,
}

impl DualThreadRenderer {
    pub fn new_suspended(
        shared_backend_data: &Rc<SharedBackendData>,
    ) -> Result<Box<dyn WinitCompatibleRenderer>, PlatformError> {
        crate::ensure_render_thread(&shared_backend_data.event_loop_proxy.clone());
        Ok(Box::new(Self { core_renderer: DualCoreRenderer::new() }))
    }

    fn encode_scene(&self, window: &SlintApiWindow) -> Result<DrawOutcome, PlatformError> {
        let Some(host) = GLOBAL_RENDER_HOST.get() else {
            // The render thread is not running yet; nothing to draw.
            return Ok(DrawOutcome::Success);
        };

        let window_inner = WindowInner::from_pub(window);
        let scale_factor = ScaleFactor::new(window_inner.scale_factor());
        let window_adapter = window_inner.window_adapter();
        let size = window_adapter.size();
        if size.width == 0 || size.height == 0 {
            return Ok(DrawOutcome::Success);
        }

        let mut encoder = crate::snapshot::SnapshotEncoder::new(
            size.width,
            size.height,
            scale_factor,
            window_adapter.clone(),
        );

        // Window background: a solid color is shipped as the clear color that
        // the render thread fills the back buffer with; any other brush
        // (gradient) is serialised as a full-viewport rectangle.
        if let Some(window_item_rc) = window_inner.window_item_rc() {
            let window_item =
                window_item_rc.downcast::<i_slint_core::items::WindowItem>().unwrap();
            match window_item.as_pin_ref().background() {
                i_slint_core::graphics::Brush::SolidColor(color) => {
                    encoder.set_background(color);
                }
                _ => {
                    encoder.draw_rectangle(
                        window_item.as_pin_ref(),
                        &window_item_rc,
                        i_slint_core::lengths::logical_size_from_api(
                            window.size().to_logical(window_inner.scale_factor()),
                        ),
                        &window_item.as_pin_ref().cached_rendering_data,
                    );
                }
            }
        }

        window_inner.draw_contents(|components, post_render| {
            for (component, origin) in components {
                if let Some(component) = i_slint_core::item_tree::ItemTreeWeak::upgrade(component)
                {
                    i_slint_core::item_rendering::render_component_items(
                        &component,
                        &mut encoder,
                        *origin,
                        &window_adapter,
                    );
                }
            }
            post_render(&mut encoder);
        });

        host.submit_scene(encoder.finish());
        Ok(DrawOutcome::Success)
    }
}

impl WinitCompatibleRenderer for DualThreadRenderer {
    fn render(&self, window: &SlintApiWindow) -> Result<DrawOutcome, PlatformError> {
        self.encode_scene(window)
    }

    fn as_core_renderer(&self) -> &dyn Renderer {
        &self.core_renderer
    }

    fn suspend(&self) -> Result<(), PlatformError> {
        if let Some(host) = GLOBAL_RENDER_HOST.get() {
            host.submit_suspend();
        }
        Ok(())
    }

    fn resume(
        &self,
        active_event_loop: &ActiveEventLoop,
        window_attributes: winit::window::WindowAttributes,
        _window_adapter_weak: Weak<crate::winitwindowadapter::WinitWindowAdapter>,
    ) -> Result<Arc<winit::window::Window>, PlatformError> {
        use crate::winit_compat::WindowSurfaceSizeExt;

        let winit_window =
            Arc::new(active_event_loop.create_window(window_attributes).map_err(
                |winit_os_error| {
                    PlatformError::from(format!(
                        "dualslint: Could not create winit window for GL rendering: {winit_os_error}"
                    ))
                },
            )?);

        #[cfg(target_family = "windows")]
        {
            use winit::platform::windows::WindowExtWindows;
            crate::render_thread::set_hwnd(winit_window.hwnd() as isize);
        }

        let size = winit_window.surface_size();
        let scale_factor = winit_window.scale_factor();

        if let Some(host) = GLOBAL_RENDER_HOST.get() {
            host.submit_configure(winit_window.clone(), size.width, size.height, scale_factor);
        }

        Ok(winit_window)
    }
}