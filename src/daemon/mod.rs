//! Daemon wiring: the calloop loop, its event sources, and the reconciler
//! that keeps every output's surfaces in step with [`State`].

pub mod ipc;
mod scene;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use calloop::EventLoop;
use calloop::channel::{self, Event};
use calloop_wayland_source::WaylandSource;
use crownos_config::Appearance;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface::WlSurface;

use crate::decode;
use crate::gpu::restrict_vulkan_driver;
use crate::wayland::{Content, FrameCallback, OutputSurface, Wayland};
use crownpaper::util::cover::Size;
use crownpaper::util::drm::{kernel_driver, render_node};
use crownpaper::util::memory::use_single_arena;
use ipc::Ipc;
use scene::REST_ZOOM;
use state::{RendererSlot, State};

/// Transition length when the caller does not specify one.
pub const DEFAULT_TRANSITION_SECS: f32 = 1.2;
const FALLBACK_RENDER_NODE: &str = "/dev/dri/renderD128";
/// How soon to look again when the helper has no frame queued yet.
const QUEUE_RETRY: Duration = Duration::from_millis(8);

/// The event loop's shared data. Field order is drop order: outputs (and
/// their swapchains) go before the renderer in `state`, and the Wayland
/// connection outlives both.
pub struct Daemon {
    pub outputs: Vec<OutputSurface>,
    /// When the next video frame should be put on screen.
    video_tick: Option<Instant>,
    pub state: State,
    ipc: Ipc,
    pub wayland: Wayland,
}

pub fn run(image: Option<PathBuf>, duration: f32) -> Result<()> {
    use_single_arena();
    let initial = startup_wallpaper(image)?;
    let mut event_loop: EventLoop<Daemon> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    let (decode_tx, decode_rx) = channel::channel();
    let (renderer_tx, renderer_rx) = channel::channel();
    let (wayland, mut event_queue) = Wayland::connect()?;
    let mut daemon = Daemon {
        outputs: Vec::new(),
        video_tick: None,
        state: State::new(decode_tx, renderer_tx),
        ipc: Ipc::bind()?,
        wayland,
    };
    // Two roundtrips: outputs and dmabuf feedback announce themselves, then
    // their details arrive. Both are needed to size decodes and pick the GPU.
    event_queue.roundtrip(&mut daemon)?;
    event_queue.roundtrip(&mut daemon)?;
    daemon.adopt_compositor_gpu();
    daemon.refresh_screen();

    if let Some(path) = initial {
        match decode::probe(&path) {
            Ok(kind) => daemon.state.request_set(path, kind, duration),
            Err(e) => log::warn!("not showing the startup wallpaper: {e:#}"),
        }
    }

    WaylandSource::new(daemon.wayland.connection.clone(), event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow!("registering the Wayland source: {}", e.error))?;
    handle
        .insert_source(decode_rx, |event, (), daemon| {
            if let Event::Msg(decoded) = event {
                daemon.state.on_decoded(decoded);
            }
        })
        .map_err(|e| anyhow!("registering the decode source: {e}"))?;
    handle
        .insert_source(renderer_rx, |event, (), daemon| {
            if let Event::Msg(built) = event {
                daemon.state.on_renderer(built);
            }
        })
        .map_err(|e| anyhow!("registering the renderer source: {e}"))?;
    ipc::register(&daemon.ipc, &handle)?;

    while !daemon.state.quit {
        daemon.sync();
        let now = Instant::now();
        let timeout = [
            daemon.state.next_deadline(now),
            daemon
                .video_tick
                .map(|tick| tick.saturating_duration_since(now)),
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop
            .dispatch(timeout, &mut daemon)
            .context("dispatching events")?;
    }
    Ok(())
}

impl Daemon {
    pub fn add_output(&mut self, output: WlOutput) {
        if self.outputs.iter().any(|surface| surface.output == output) {
            return;
        }
        log::info!(
            "creating wallpaper surface on {}",
            self.output_name(&output)
        );
        self.outputs.push(OutputSurface::new(&self.wayland, output));
        self.refresh_screen();
    }

    pub fn refresh_output(&mut self, output: &WlOutput) {
        let Some(info) = self.wayland.output_state.info(output) else {
            return;
        };
        let renderer = match &self.state.renderer {
            RendererSlot::Warm(renderer) => Some(renderer),
            _ => None,
        };
        if let Some(surface) = self.outputs.iter_mut().find(|s| &s.output == output) {
            surface.update_info(&info, renderer);
        }
        self.refresh_screen();
    }

    pub fn configure_output(&mut self, layer: &LayerSurface, logical: Size) {
        if let Some(surface) = self.outputs.iter_mut().find(|s| s.is_layer(layer)) {
            surface.configure(&self.wayland, logical, REST_ZOOM);
        }
    }

    pub fn on_buffer_released(&mut self, buffer: &WlBuffer) {
        if !self
            .outputs
            .iter_mut()
            .any(|surface| surface.release(buffer))
        {
            buffer.destroy();
        }
    }

    pub fn on_frame(&mut self, surface: &WlSurface) {
        let now = Instant::now();
        if let Some(index) = self
            .outputs
            .iter()
            .position(|o| o.owns_render_surface(surface))
        {
            if let Some(layer) = self.outputs[index].render_layer() {
                layer.frame_pending = false;
            }
            self.state.scene.advance(now);
            if self.state.scene.is_animating() {
                self.render(index, now);
            }
            return;
        }
        let Some(output) = self.outputs.iter_mut().find(|o| o.wl_surface() == surface) else {
            return;
        };
        output.frame_pending = false;
        if self.video_tick.is_none() {
            self.present_video(now);
        }
    }

    /// Puts the due video frame on every output that is presenting, and
    /// schedules the next one. An output whose last frame callback has not
    /// come back is hidden; with all of them hidden nothing is consumed, so
    /// the helper stops decoding until a callback arrives.
    fn present_video(&mut self, now: Instant) {
        self.video_tick = None;
        let Some(player) = self.state.playback.as_mut() else {
            return;
        };
        if self.state.scene.is_animating() {
            return;
        }
        let presenting = |o: &OutputSurface| o.is_configured() && !o.frame_pending;
        if !self.outputs.iter().any(presenting) {
            return;
        }
        player.take_due(now);
        if let Some(frame) = player.current() {
            let content = Content::Video(Arc::clone(frame));
            for output in self.outputs.iter_mut().filter(|o| presenting(o)) {
                output.show(
                    &self.wayland,
                    content.clone(),
                    REST_ZOOM,
                    FrameCallback::Request,
                );
            }
        }
        self.video_tick = Some(player.next_due(now).unwrap_or(now + QUEUE_RETRY));
    }

    /// Reconciles every output with the state after each batch of events.
    fn sync(&mut self) {
        let now = Instant::now();
        self.state.scene.advance(now);
        if self.state.start_pending(now) {
            log::info!("animation started");
        }
        self.state.settle();

        if self.state.scene.is_animating() {
            self.animate_outputs(now);
        } else {
            self.rest_outputs();
            if self.state.playback.is_none() {
                self.video_tick = None;
            } else if self.video_tick.is_none_or(|tick| tick <= now) {
                self.present_video(now);
            }
        }

        if self.state.drive_renderer(now)
            && self.outputs.iter_mut().all(|o| o.render_layer().is_none())
        {
            self.state.tear_down_renderer();
        }
        for path in self.state.take_changed() {
            self.ipc.emit_changed(&path);
        }
    }

    fn animate_outputs(&mut self, now: Instant) {
        for index in 0..self.outputs.len() {
            let RendererSlot::Warm(renderer) = &mut self.state.renderer else {
                return;
            };
            let output = &mut self.outputs[index];
            if !output.is_configured() {
                continue;
            }
            if let Err(e) = output.ensure_render_layer(&self.wayland, renderer) {
                log::error!("transition surface: {e:#}");
                continue;
            }
            if output
                .render_layer()
                .is_some_and(|layer| !layer.frame_pending)
            {
                self.render(index, now);
            }
        }
    }

    fn rest_outputs(&mut self) {
        let Some(content) = self.state.rest_content() else {
            return;
        };
        let playing = self.state.playback.is_some();
        for output in self.outputs.iter_mut().filter(|o| o.is_configured()) {
            let paced_by_video = playing && matches!(output.showing(), Some(Content::Video(_)));
            if !paced_by_video {
                output.show(
                    &self.wayland,
                    content.clone(),
                    REST_ZOOM,
                    FrameCallback::Skip,
                );
            }
            output.drop_render_layer();
        }
    }

    fn render(&mut self, index: usize, now: Instant) {
        let RendererSlot::Warm(renderer) = &self.state.renderer else {
            return;
        };
        let scene = &self.state.scene;
        let output = &mut self.outputs[index];
        let uniforms = scene.uniforms(&scene.params(now), output.physical());
        let Some(layer) = output.render_layer() else {
            return;
        };
        layer.request_frame(&self.wayland);
        if let Some(target) = layer.target() {
            target.render(
                renderer,
                &uniforms,
                scene.textures(&renderer.placeholder),
                scene.generation(),
            );
        }
    }

    /// Renders and decodes on the compositor's GPU, so buffers import
    /// without a copy and other GPUs stay asleep. Must run before any thread
    /// is spawned.
    fn adopt_compositor_gpu(&mut self) {
        if self.wayland.importable.is_empty() {
            self.wayland.importable = self
                .wayland
                .dmabuf
                .modifiers()
                .iter()
                .map(|format| (format.format, format.modifier))
                .collect();
        }
        if let Some(driver) = self.wayland.main_device.and_then(kernel_driver) {
            // SAFETY: called during startup, before any thread is spawned.
            unsafe { restrict_vulkan_driver(&driver) };
        }
        self.state.importable = Arc::clone(&self.wayland.importable);
        self.state.video_device = self
            .wayland
            .main_device
            .and_then(render_node)
            .or_else(|| Some(PathBuf::from(FALLBACK_RENDER_NODE)));
    }

    fn refresh_screen(&mut self) {
        if let Some(largest) = self
            .outputs
            .iter()
            .map(OutputSurface::physical)
            .max_by_key(|size| u64::from(size.width) * u64::from(size.height))
        {
            self.state.screen = largest;
        }
    }

    fn output_name(&self, output: &WlOutput) -> String {
        self.wayland
            .output_state
            .info(output)
            .and_then(|info| info.name)
            .unwrap_or_else(|| "<unnamed>".into())
    }
}

/// The wallpaper to show at startup, resolved before any Wayland setup so a
/// bad `--image` fails immediately.
///
/// `--image` wins. Without it the daemon follows `appearance.ron`'s
/// `wallpaper`, which crownsettings keeps pointed at whatever crownpaper last
/// reported — so a restarted daemon comes back with the wallpaper it had.
fn startup_wallpaper(image: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match image {
        Some(path) => std::fs::canonicalize(&path)
            .map(Some)
            .with_context(|| format!("resolving initial wallpaper {}", path.display())),
        None => Ok(configured_wallpaper()),
    }
}

/// The `wallpaper` key of `appearance.ron`, if it names a file we can read.
///
/// Unset means "no wallpaper configured yet", and a path that no longer
/// resolves is a warning rather than an error: the daemon still has to come up
/// and serve `set`, and refusing to start over a stale config line would leave
/// the desktop with no way to fix it.
fn configured_wallpaper() -> Option<PathBuf> {
    let configured = crownos_config::load::<Appearance>(Appearance::SECTION).wallpaper;
    if configured.is_empty() {
        return None;
    }
    match std::fs::canonicalize(&configured) {
        Ok(path) => {
            log::info!("startup wallpaper from appearance.ron: {}", path.display());
            Some(path)
        }
        Err(e) => {
            log::warn!("ignoring wallpaper {configured} from appearance.ron: {e}");
            None
        }
    }
}
