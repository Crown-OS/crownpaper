//! SCTK event plumbing into the daemon.

use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_dmabuf, delegate_layer, delegate_output, delegate_registry,
    delegate_shm, delegate_subcompositor, registry_handlers,
};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use super::ShmBufferData;
use crate::daemon::Daemon;
use crownpaper::util::cover::Size;

impl CompositorHandler for Daemon {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        self.on_frame(surface);
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for Daemon {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.wayland.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.add_output(output);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.refresh_output(&output);
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.outputs.retain(|surface| surface.output != output);
    }
}

impl LayerShellHandler for Daemon {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.outputs.retain(|surface| !surface.is_layer(layer));
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (width, height) = configure.new_size;
        self.configure_output(layer, Size::new(width.max(1), height.max(1)));
    }
}

impl ShmHandler for Daemon {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wayland.shm
    }
}

impl DmabufHandler for Daemon {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.wayland.dmabuf
    }

    fn dmabuf_feedback(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        let table = feedback.format_table();
        self.wayland.importable = feedback
            .tranches()
            .iter()
            .flat_map(|tranche| tranche.formats.iter())
            .filter_map(|&index| table.get(usize::from(index)))
            .map(|format| (format.format, format.modifier))
            .collect();
        self.wayland.main_device = Some(feedback.main_device());
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxBufferParamsV1,
        _: WlBuffer,
    ) {
    }

    fn failed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpLinuxBufferParamsV1) {
        log::warn!("the compositor rejected a video frame buffer");
    }

    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, buffer: &WlBuffer) {
        self.on_buffer_released(buffer);
    }
}

impl Dispatch<WlBuffer, ShmBufferData> for Daemon {
    fn event(
        daemon: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &ShmBufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            daemon.on_buffer_released(buffer);
        }
    }
}

impl ProvidesRegistryState for Daemon {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wayland.registry
    }

    registry_handlers![OutputState];
}

delegate_compositor!(Daemon);
delegate_subcompositor!(Daemon);
delegate_output!(Daemon);
delegate_layer!(Daemon);
delegate_shm!(Daemon);
delegate_dmabuf!(Daemon);
delegate_registry!(Daemon);
delegate_noop!(Daemon: ignore WpViewporter);
delegate_noop!(Daemon: ignore WpViewport);
delegate_noop!(Daemon: ignore WlShmPool);
delegate_noop!(Daemon: ignore WlRegion);
