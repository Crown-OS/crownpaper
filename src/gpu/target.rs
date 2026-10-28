//! A wgpu swapchain on one output's transition subsurface.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Context, Result, anyhow};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

use super::context::Renderer;
use super::pipeline::{TARGET_FORMAT, Uniforms};
use super::texture::GpuImage;
use crownpaper::util::cover::Size;

// SAFETY: the fullscreen triangle writes every pixel without blending, so the
// previous contents are never read.
const FULLSCREEN_OVERWRITE: wgpu::LoadOpDontCare = unsafe { wgpu::LoadOpDontCare::enabled() };

pub struct RenderTarget {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    uniforms: wgpu::Buffer,
    /// Rebuilt when the scene generation it was made for goes stale.
    bind_group: Option<(u64, wgpu::BindGroup)>,
}

impl RenderTarget {
    /// # Safety
    ///
    /// `connection` and `wl_surface` must outlive the returned target.
    pub unsafe fn new(
        renderer: &mut Renderer,
        connection: &Connection,
        wl_surface: &WlSurface,
        size: Size,
    ) -> Result<Self> {
        let display = NonNull::new(connection.backend().display_ptr().cast::<c_void>())
            .ok_or_else(|| anyhow!("wl_display pointer is null"))?;
        let window = NonNull::new(wl_surface.id().as_ptr().cast::<c_void>())
            .ok_or_else(|| anyhow!("wl_surface pointer is null"))?;
        // SAFETY: forwarded to the caller.
        let surface = unsafe {
            renderer
                .instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                        display,
                    ))),
                    raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
                })
        }
        .context("creating the Vulkan surface")?;

        let caps = surface.get_capabilities(&renderer.adapter);
        let format = [TARGET_FORMAT, TARGET_FORMAT.remove_srgb_suffix()]
            .into_iter()
            .chain(caps.formats.iter().copied())
            .find(|format| caps.formats.contains(format))
            .ok_or_else(|| anyhow!("surface reports no formats"))?;
        let view_format = format.add_srgb_suffix();
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            wgpu::CompositeAlphaMode::Auto
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: if view_format == format {
                vec![]
            } else {
                vec![view_format]
            },
        };
        surface.configure(&renderer.device, &config);
        renderer.pipeline.for_format(&renderer.device, view_format);
        let uniforms = renderer.pipeline.create_uniform_buffer(&renderer.device);
        Ok(Self {
            surface,
            config,
            view_format,
            uniforms,
            bind_group: None,
        })
    }

    pub fn size(&self) -> Size {
        Size::new(self.config.width, self.config.height)
    }

    pub fn resize(&mut self, device: &wgpu::Device, size: Size) {
        if size == self.size() || size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(device, &self.config);
    }

    /// Draws one frame and presents it, which commits the subsurface.
    pub fn render(
        &mut self,
        renderer: &Renderer,
        uniforms: &Uniforms,
        (from, to): (&GpuImage, &GpuImage),
        generation: u64,
    ) {
        let Some(pipeline) = renderer.pipeline.get(self.view_format) else {
            return;
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&renderer.device, &self.config);
                return;
            }
            other => {
                log::debug!("skipping frame: {other:?}");
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });

        if self
            .bind_group
            .as_ref()
            .is_none_or(|(built_for, _)| *built_for != generation)
        {
            let group =
                renderer
                    .pipeline
                    .create_bind_group(&renderer.device, &self.uniforms, from, to);
            self.bind_group = Some((generation, group));
        }
        renderer
            .queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(uniforms));

        let mut encoder = renderer.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wallpaper"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::DontCare(FULLSCREEN_OVERWRITE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, self.bind_group.as_ref().map(|(_, group)| group), &[]);
            pass.draw(0..3, 0..1);
        }
        renderer.queue.submit([encoder.finish()]);
        frame.present();
    }
}
