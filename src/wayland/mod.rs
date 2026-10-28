//! The Wayland side: globals, buffers, and one background surface per output.

mod handlers;
pub mod output;

use std::os::fd::AsFd;
use std::sync::Arc;

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::dmabuf::DmabufState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm;
use wayland_client::{Connection, EventQueue, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

pub use output::{Content, FrameCallback, OutputSurface};

use crate::daemon::Daemon;
use crate::video::{DmabufPlanes, ImportableFormats};
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;

/// User data of `wl_shm` buffers, whose release we track ourselves.
pub struct ShmBufferData;

pub struct Wayland {
    pub connection: Connection,
    pub qh: QueueHandle<Daemon>,
    pub registry: RegistryState,
    pub output_state: OutputState,
    pub compositor: CompositorState,
    pub subcompositor: SubcompositorState,
    pub layer_shell: LayerShell,
    pub shm: Shm,
    pub viewporter: WpViewporter,
    pub dmabuf: DmabufState,
    /// What the compositor can import as dmabufs, from its feedback.
    pub importable: ImportableFormats,
    /// The compositor's GPU, from its dmabuf feedback.
    pub main_device: Option<libc::dev_t>,
    /// Shown until an output has a wallpaper, so the surface maps at once.
    pub black: Arc<ShmImage>,
}

impl Wayland {
    pub fn connect() -> Result<(Self, EventQueue<Daemon>)> {
        let connection = Connection::connect_to_env().context("connecting to Wayland")?;
        let (globals, event_queue) =
            registry_queue_init::<Daemon>(&connection).context("listing Wayland globals")?;
        let qh = event_queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor")?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
                .context("wl_subcompositor")?;
        let dmabuf = DmabufState::new(&globals, &qh);
        if dmabuf.version().is_some_and(|version| version >= 4)
            && let Err(e) = dmabuf.get_default_feedback(&qh)
        {
            log::warn!("dmabuf feedback unavailable: {e}");
        }
        let wayland = Self {
            registry: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            subcompositor,
            layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
            shm: Shm::bind(&globals, &qh).context("wl_shm")?,
            viewporter: globals.bind(&qh, 1..=1, ()).context("wp_viewporter")?,
            compositor,
            dmabuf,
            importable: Arc::new([]),
            main_device: None,
            black: Arc::new(ShmImage::filled(Size::new(1, 1), [0, 0, 0, 0xFF])?),
            qh,
            connection,
        };
        Ok((wayland, event_queue))
    }

    pub fn shm_buffer(&self, image: &ShmImage) -> WlBuffer {
        let size = image.size();
        let pool =
            self.shm
                .wl_shm()
                .create_pool(image.as_fd(), image.byte_len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            size.width as i32,
            size.height as i32,
            image.stride() as i32,
            wl_shm::Format::Xrgb8888,
            &self.qh,
            ShmBufferData,
        );
        pool.destroy();
        buffer
    }

    pub fn dmabuf_buffer(&self, frame: &DmabufPlanes) -> Result<WlBuffer> {
        let params = self.dmabuf.create_params(&self.qh)?;
        for (index, (fd, plane)) in frame.planes.iter().enumerate() {
            params.add(
                fd.as_fd(),
                index as u32,
                plane.offset,
                plane.stride,
                plane.modifier,
            );
        }
        let (buffer, _) = params.create_immed(
            frame.size.width as i32,
            frame.size.height as i32,
            frame.fourcc,
            Flags::empty(),
            &self.qh,
        );
        Ok(buffer)
    }
}
