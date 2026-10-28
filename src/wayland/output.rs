//! One output's wallpaper surfaces.
//!
//! The layer surface itself holds the resting content — a still in shared
//! memory or the current video frame — cropped and scaled by the compositor
//! through its viewport. Only while something animates does a subsurface on
//! top carry a wgpu swapchain.

use std::sync::Arc;

use anyhow::Result;
use smithay_client_toolkit::output::OutputInfo;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerSurface,
};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use super::Wayland;
use crate::gpu::{RenderTarget, Renderer};
use crate::video::{FrameContent, VideoFrame};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::{Size, cover_crop};

/// What a surface shows, kept alive until the compositor releases it.
#[derive(Clone)]
pub enum Content {
    Image(Arc<ShmImage>),
    Video(Arc<VideoFrame>),
}

impl Content {
    pub fn same_as(&self, other: &Content) -> bool {
        match (self, other) {
            (Content::Image(a), Content::Image(b)) => Arc::ptr_eq(a, b),
            (Content::Video(a), Content::Video(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn size(&self) -> Size {
        match self {
            Content::Image(image) => image.size(),
            Content::Video(frame) => frame.size(),
        }
    }
}

/// Whether a commit also asks the compositor when it wants the next frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameCallback {
    Skip,
    Request,
}

struct Attached {
    buffer: WlBuffer,
    content: Content,
}

pub struct OutputSurface {
    pub output: WlOutput,
    layer: LayerSurface,
    viewport: WpViewport,
    logical: Option<Size>,
    physical: Size,
    attached: Option<Attached>,
    retiring: Vec<Attached>,
    pub frame_pending: bool,
    render: Option<RenderLayer>,
}

impl OutputSurface {
    pub fn new(wayland: &Wayland, output: WlOutput) -> Self {
        let surface = wayland.compositor.create_surface(&wayland.qh);
        let layer = wayland.layer_shell.create_layer_surface(
            &wayland.qh,
            surface,
            Layer::Background,
            Some("crownpaper"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        let empty = wayland
            .compositor
            .wl_compositor()
            .create_region(&wayland.qh, ());
        layer.wl_surface().set_input_region(Some(&empty));
        empty.destroy();
        layer.commit();
        let viewport = wayland
            .viewporter
            .get_viewport(layer.wl_surface(), &wayland.qh, ());
        let physical = wayland
            .output_state
            .info(&output)
            .and_then(|info| physical_size(&info))
            .unwrap_or(Size::new(1, 1));
        Self {
            output,
            layer,
            viewport,
            logical: None,
            physical,
            attached: None,
            retiring: Vec::new(),
            frame_pending: false,
            render: None,
        }
    }

    pub fn wl_surface(&self) -> &WlSurface {
        self.layer.wl_surface()
    }

    pub fn is_layer(&self, layer: &LayerSurface) -> bool {
        &self.layer == layer
    }

    pub fn is_configured(&self) -> bool {
        self.logical.is_some()
    }

    pub fn physical(&self) -> Size {
        self.physical
    }

    pub fn update_info(&mut self, info: &OutputInfo, renderer: Option<&Renderer>) {
        if let Some(physical) = physical_size(info) {
            self.physical = physical;
            if let (Some(target), Some(renderer)) =
                (self.render.as_mut().and_then(RenderLayer::target), renderer)
            {
                target.resize(&renderer.device, physical);
            }
        }
    }

    /// Adopts the compositor's size. The first configure maps the surface
    /// with black so the output never shows through.
    pub fn configure(&mut self, wayland: &Wayland, logical: Size, zoom: f64) {
        let first = self.logical.is_none();
        self.logical = Some(logical);
        if let Some(layer) = &self.render {
            layer
                .viewport
                .set_destination(logical.width as i32, logical.height as i32);
        }
        match self
            .attached
            .as_ref()
            .map(|attached| attached.content.clone())
        {
            Some(content) => {
                self.crop_to(&content, zoom);
                self.wl_surface().commit();
            }
            None if first => self.show(
                wayland,
                Content::Image(Arc::clone(&wayland.black)),
                zoom,
                FrameCallback::Skip,
            ),
            None => {}
        }
    }

    pub fn showing(&self) -> Option<&Content> {
        self.attached.as_ref().map(|attached| &attached.content)
    }

    /// Attaches `content` to the layer surface, cropped to cover the output.
    pub fn show(
        &mut self,
        wayland: &Wayland,
        content: Content,
        zoom: f64,
        callback: FrameCallback,
    ) {
        if self
            .showing()
            .is_some_and(|current| current.same_as(&content))
        {
            return;
        }
        let buffer = match &content {
            Content::Image(image) => wayland.shm_buffer(image),
            Content::Video(frame) => match &frame.content {
                FrameContent::Shm(image) => wayland.shm_buffer(image),
                FrameContent::Dmabuf(dmabuf) => match wayland.dmabuf_buffer(dmabuf) {
                    Ok(buffer) => buffer,
                    Err(e) => {
                        log::warn!("importing a video frame: {e:#}");
                        return;
                    }
                },
            },
        };
        let surface = self.layer.wl_surface();
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        if self.showing().map(Content::size) != Some(content.size()) {
            self.crop_to(&content, zoom);
        }
        if callback == FrameCallback::Request {
            surface.frame(&wayland.qh, surface.clone());
            self.frame_pending = true;
        }
        surface.commit();
        if let Some(previous) = self.attached.replace(Attached { buffer, content }) {
            self.retiring.push(previous);
        }
    }

    fn crop_to(&self, content: &Content, zoom: f64) {
        let Some(logical) = self.logical else {
            return;
        };
        let source = content.size();
        if source == Size::new(1, 1) {
            self.viewport.set_source(-1.0, -1.0, -1.0, -1.0);
        } else {
            let crop = cover_crop(source, logical, zoom);
            self.viewport
                .set_source(crop.x, crop.y, crop.width, crop.height);
        }
        self.viewport
            .set_destination(logical.width as i32, logical.height as i32);
    }

    /// Frees a buffer the compositor is done with. Returns whether it was ours.
    pub fn release(&mut self, buffer: &WlBuffer) -> bool {
        let Some(index) = self
            .retiring
            .iter()
            .position(|retired| &retired.buffer == buffer)
        else {
            return false;
        };
        self.retiring.swap_remove(index).buffer.destroy();
        true
    }

    pub fn render_layer(&mut self) -> Option<&mut RenderLayer> {
        self.render.as_mut()
    }

    pub fn owns_render_surface(&self, surface: &WlSurface) -> bool {
        self.render
            .as_ref()
            .is_some_and(|layer| &layer.surface == surface)
    }

    pub fn ensure_render_layer(
        &mut self,
        wayland: &Wayland,
        renderer: &mut Renderer,
    ) -> Result<()> {
        let Some(logical) = self.logical else {
            return Ok(());
        };
        if self.render.is_some() {
            return Ok(());
        }
        let (subsurface, surface) = wayland
            .subcompositor
            .create_subsurface(self.wl_surface().clone(), &wayland.qh);
        subsurface.set_desync();
        let viewport = wayland.viewporter.get_viewport(&surface, &wayland.qh, ());
        viewport.set_destination(logical.width as i32, logical.height as i32);
        let empty = wayland
            .compositor
            .wl_compositor()
            .create_region(&wayland.qh, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();
        // SAFETY: the RenderLayer owns `surface` and drops the target before
        // destroying it; the connection outlives every output.
        let target =
            unsafe { RenderTarget::new(renderer, &wayland.connection, &surface, self.physical) };
        let layer = RenderLayer {
            target: Some(target?),
            viewport,
            subsurface,
            surface,
            frame_pending: false,
        };
        self.render = Some(layer);
        self.wl_surface().commit();
        Ok(())
    }

    /// Removes the transition subsurface; whatever the layer surface holds
    /// shows through from the next frame on.
    pub fn drop_render_layer(&mut self) {
        if self.render.take().is_some() {
            self.wl_surface().commit();
        }
    }
}

impl Drop for OutputSurface {
    fn drop(&mut self) {
        self.render = None;
        self.viewport.destroy();
        for attached in self
            .attached
            .take()
            .into_iter()
            .chain(self.retiring.drain(..))
        {
            attached.buffer.destroy();
        }
    }
}

pub struct RenderLayer {
    target: Option<RenderTarget>,
    viewport: WpViewport,
    subsurface: WlSubsurface,
    pub surface: WlSurface,
    pub frame_pending: bool,
}

impl RenderLayer {
    pub fn target(&mut self) -> Option<&mut RenderTarget> {
        self.target.as_mut()
    }

    /// Asks for a frame callback; it is committed by the next present.
    pub fn request_frame(&mut self, wayland: &Wayland) {
        self.surface.frame(&wayland.qh, self.surface.clone());
        self.frame_pending = true;
    }
}

impl Drop for RenderLayer {
    fn drop(&mut self) {
        // The swapchain must go before the surface it presents to.
        self.target = None;
        self.viewport.destroy();
        self.subsurface.destroy();
        self.surface.destroy();
    }
}

fn physical_size(info: &OutputInfo) -> Option<Size> {
    let mode = info.modes.iter().find(|mode| mode.current)?;
    let (width, height) = (
        mode.dimensions.0.max(1) as u32,
        mode.dimensions.1.max(1) as u32,
    );
    Some(match info.transform {
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
            Size::new(height, width)
        }
        _ => Size::new(width, height),
    })
}
