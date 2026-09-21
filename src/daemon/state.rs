//! Shared daemon state.
//!
//! One [`State`] behind an `Rc<RefCell<_>>` connects the IPC handler, the
//! decode-result channel and every per-output surface. Surfaces only *read*
//! the scene (plus advance its clocks); all mutations funnel through the
//! request methods here, which wake the event loop so `needs_redraw` gets
//! polled and queued bus events get flushed.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crownshell::calloop::channel::Sender;
use crownshell::wgpu;

use crate::cache::TextureCache;
use crate::decode::{DecodedImage, Decoder, Priority};
use crate::render::Gpu;
use crate::texture::GpuImage;

const INTRO_DURATION_SECS: f32 = 0.85;
const INTRO_START_SCALE: f32 = 1.15;
/// Resting zoom of the wallpaper. Slightly above 1.0 so the intro's settle
/// curve never dips below full coverage and reveals letterbox.
const INTRO_END_SCALE: f32 = 1.02;
const MIN_TRANSITION_SECS: f32 = 0.05;

/// A wallpaper layer bound to the shader: source path plus uploaded texture.
#[derive(Clone)]
pub struct WallpaperLayer {
    pub path: PathBuf,
    pub image: GpuImage,
}

struct Transition {
    started: Instant,
    duration: f32,
}

/// Per-frame animation parameters, identical for every output so multi-head
/// transitions stay in sync.
pub struct FrameParams {
    pub progress: f32,
    pub intro_scale: f32,
    pub time: f32,
}

/// What is on screen: the incoming (`to`) and, mid-transition, outgoing
/// (`from`) wallpaper, plus the animation clocks they share.
pub struct Scene {
    pub from: Option<WallpaperLayer>,
    pub to: Option<WallpaperLayer>,
    transition: Option<Transition>,
    intro_started: Option<Instant>,
    /// Bumped whenever `from`/`to` change; surfaces rebuild their bind group
    /// when their recorded generation no longer matches.
    pub generation: u64,
    epoch: Instant,
}

impl Scene {
    fn new() -> Self {
        Self {
            from: None,
            to: None,
            transition: None,
            intro_started: None,
            generation: 0,
            epoch: Instant::now(),
        }
    }

    /// First wallpaper: no crossfade, just the launch intro zoom.
    fn set_initial(&mut self, to: WallpaperLayer) {
        self.to = Some(to);
        self.intro_started = Some(Instant::now());
        self.generation += 1;
    }

    fn begin_transition(&mut self, to: WallpaperLayer, duration: f32) {
        self.from = self.to.take();
        self.to = Some(to);
        self.transition = Some(Transition {
            started: Instant::now(),
            duration: duration.max(MIN_TRANSITION_SECS),
        });
        self.intro_started = None;
        self.generation += 1;
    }

    /// Whether `path` is on screen right now and must survive cache eviction.
    pub fn uses(&self, path: &Path) -> bool {
        let matches =
            |layer: &Option<WallpaperLayer>| layer.as_ref().is_some_and(|l| l.path == path);
        matches(&self.from) || matches(&self.to)
    }

    pub fn is_animating(&self) -> bool {
        self.transition.is_some() || self.intro_started.is_some()
    }

    /// Advance the animation clocks and return this frame's parameters.
    ///
    /// Finished animations are retired here: the final frame renders with
    /// `progress == 1.0`, and the outgoing layer is dropped in the same step
    /// so its texture can be reclaimed (the shader skips the fully faded
    /// layer, so the swapped-in placeholder is never sampled visibly).
    pub fn frame(&mut self) -> FrameParams {
        let progress = match &self.transition {
            Some(t) => {
                let p = t.started.elapsed().as_secs_f32() / t.duration;
                if p >= 1.0 {
                    self.transition = None;
                    self.from = None;
                    self.generation += 1;
                    1.0
                } else {
                    p
                }
            }
            None => 1.0,
        };
        let intro_scale = match self.intro_started {
            Some(started) => {
                let p = started.elapsed().as_secs_f32() / INTRO_DURATION_SECS;
                if p >= 1.0 {
                    self.intro_started = None;
                    INTRO_END_SCALE
                } else {
                    INTRO_START_SCALE + (INTRO_END_SCALE - INTRO_START_SCALE) * intro_ease(p)
                }
            }
            None => INTRO_END_SCALE,
        };
        FrameParams {
            progress,
            intro_scale,
            time: self.epoch.elapsed().as_secs_f32(),
        }
    }
}

/// What to do with a path once its decode completes.
#[derive(Default, Clone, Copy)]
struct LoadIntent {
    /// Transition duration, when the wallpaper should be shown on arrival.
    display: Option<f32>,
    /// Keep it pinned in the cache (preload).
    pin: bool,
}

pub struct State {
    pub gpu: Option<Gpu>,
    pub cache: TextureCache,
    pub scene: Scene,
    decoder: Decoder,
    /// Paths currently in the decode queue and what they were requested for.
    in_flight: HashMap<PathBuf, LoadIntent>,
    /// Decodes that finished before the first surface brought the GPU up.
    awaiting_gpu: Vec<(PathBuf, DecodedImage, LoadIntent)>,
    /// `Changed` events not yet emitted on the bus.
    changed: Vec<PathBuf>,
    pub quit: bool,
    /// Bumped whenever surfaces should repaint outside an animation frame.
    pub paint_serial: u64,
    /// Wakes the daemon's flush source so events and redraws happen promptly.
    wake: Sender<()>,
}

impl State {
    pub fn new(decoder: Decoder, wake: Sender<()>) -> Self {
        Self {
            gpu: None,
            cache: TextureCache::default(),
            scene: Scene::new(),
            decoder,
            in_flight: HashMap::new(),
            awaiting_gpu: Vec::new(),
            changed: Vec::new(),
            quit: false,
            paint_serial: 0,
            wake,
        }
    }

    /// Adopt the shared device/queue on first surface setup and flush any
    /// decodes that were waiting for a GPU.
    pub fn ensure_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.gpu.is_some() {
            return;
        }
        self.gpu = Some(Gpu::new(device.clone(), queue.clone()));
        for (path, image, intent) in std::mem::take(&mut self.awaiting_gpu) {
            self.finish_load(path, &image, intent);
        }
    }

    /// `set`: show `path`, transitioning over `duration` seconds. Cache hits
    /// start immediately; misses decode off-thread first.
    pub fn request_set(&mut self, path: PathBuf, duration: f32) {
        self.cancel_superseded_displays(&path);
        if let Some(image) = self.cache.get(&path) {
            self.display(WallpaperLayer { path, image }, duration);
            return;
        }
        match self.in_flight.entry(path.clone()) {
            Entry::Occupied(mut e) => e.get_mut().display = Some(duration),
            Entry::Vacant(e) => {
                e.insert(LoadIntent {
                    display: Some(duration),
                    pin: false,
                });
                self.decoder.enqueue(path, Priority::Display);
            }
        }
    }

    /// `preload`: decode and pin without displaying.
    pub fn request_preload(&mut self, path: PathBuf) {
        if self.cache.pin_resident(&path) {
            return;
        }
        match self.in_flight.entry(path.clone()) {
            Entry::Occupied(mut e) => e.get_mut().pin = true,
            Entry::Vacant(e) => {
                e.insert(LoadIntent {
                    display: None,
                    pin: true,
                });
                self.decoder.enqueue(path, Priority::Preload);
            }
        }
    }

    /// `unload`: release a preloaded wallpaper.
    pub fn request_unload(&mut self, path: &Path) {
        if let Some(intent) = self.in_flight.get_mut(path) {
            intent.pin = false;
        }
        let scene = &self.scene;
        self.cache.unload(path, |p| scene.uses(p));
    }

    pub fn request_quit(&mut self) {
        self.quit = true;
        self.wake();
    }

    pub fn on_decoded(&mut self, path: PathBuf, result: Result<DecodedImage, String>) {
        let Some(intent) = self.in_flight.remove(&path) else {
            return;
        };
        match result {
            Ok(image) => {
                if intent.display.is_none() && !intent.pin {
                    return; // unloaded while decoding
                }
                if self.gpu.is_some() {
                    self.finish_load(path, &image, intent);
                } else {
                    self.awaiting_gpu.push((path, image, intent));
                }
            }
            Err(e) => log::warn!("wallpaper load failed: {e}"),
        }
    }

    /// Only the latest `set` may win: a newer request voids the display
    /// intent of any earlier one still in the decode pipeline, so a slow
    /// decode can never overwrite a wallpaper chosen after it.
    fn cancel_superseded_displays(&mut self, winner: &Path) {
        for (path, intent) in &mut self.in_flight {
            if path != winner {
                intent.display = None;
            }
        }
        self.awaiting_gpu.retain_mut(|(path, _, intent)| {
            if path != winner {
                intent.display = None;
            }
            intent.display.is_some() || intent.pin
        });
    }

    /// Drain the `Changed` events queued since the last flush.
    pub fn take_changed(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.changed)
    }

    fn finish_load(&mut self, path: PathBuf, decoded: &DecodedImage, intent: LoadIntent) {
        let gpu = self.gpu.as_ref().expect("finish_load requires a GPU");
        let image = GpuImage::upload(&gpu.device, &gpu.queue, decoded);
        let scene = &self.scene;
        self.cache
            .insert(path.clone(), image.clone(), intent.pin, |p| scene.uses(p));
        if let Some(duration) = intent.display {
            self.display(WallpaperLayer { path, image }, duration);
        }
    }

    fn display(&mut self, layer: WallpaperLayer, duration: f32) {
        let already_shown = self.scene.to.as_ref().is_some_and(|l| l.path == layer.path);
        if !already_shown {
            if self.scene.to.is_none() {
                self.scene.set_initial(layer.clone());
            } else {
                self.scene.begin_transition(layer.clone(), duration);
            }
            log::info!("wallpaper changed to {}", layer.path.display());
        }
        // Emitted even when redundant: subscribers mirror it into config.
        self.changed.push(layer.path);
        self.request_repaint();
    }

    fn request_repaint(&mut self) {
        self.paint_serial += 1;
        self.wake();
    }

    fn wake(&self) {
        if self.wake.send(()).is_err() {
            log::error!("daemon wake channel closed");
        }
    }
}

/// CPU-side easing for the intro zoom, matching the old launch feel:
/// CSS `cubic-bezier(0.62, 0.0, 0.49, 0.99)` evaluated by inverting
/// `B_x(t) = x` with Newton-Raphson and returning `B_y(t)`.
fn intro_ease(x: f32) -> f32 {
    const X1: f32 = 0.62;
    const Y1: f32 = 0.0;
    const X2: f32 = 0.49;
    const Y2: f32 = 0.99;
    let (ax, bx, cx) = (3.0 * X1, 3.0 * X2 - 6.0 * X1, 3.0 * X1 - 3.0 * X2 + 1.0);
    let (ay, by, cy) = (3.0 * Y1, 3.0 * Y2 - 6.0 * Y1, 3.0 * Y1 - 3.0 * Y2 + 1.0);

    let x = x.clamp(0.0, 1.0);
    let mut t = x;
    for _ in 0..8 {
        let bezier_x = t * (ax + t * (bx + t * cx));
        let derivative = (ax + t * (2.0 * bx + 3.0 * cx * t)).max(1e-6);
        let delta = (bezier_x - x) / derivative;
        t -= delta;
        if delta.abs() < 1e-6 {
            break;
        }
    }
    t = t.clamp(0.0, 1.0);
    t * (ay + t * (by + t * cy))
}
