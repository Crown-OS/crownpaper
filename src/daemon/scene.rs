//! What is on screen and how it animates.
//!
//! The incoming (`to`) and, mid-transition, outgoing (`from`) wallpaper share
//! one clock so every output stays frame-locked. All easing is evaluated here
//! once per frame and handed to the shader as plain uniforms.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gpu::{GpuImage, Renderer, Uniforms};
use crate::wallpaper::Wallpaper;
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::{Size, cover_uv_map};
use crownpaper::util::easing::{CROSSFADE, INTRO, STANDARD};

/// Resting zoom of every wallpaper. Slightly above 1.0 so the intro's settle
/// curve never dips below full coverage.
pub const REST_ZOOM: f64 = 1.02;
const INTRO_START_ZOOM: f32 = 1.15;
const INTRO_DURATION: Duration = Duration::from_millis(850);
const MIN_TRANSITION_SECS: f32 = 0.05;
/// Peak defocus radius, in UV units of the shorter axis.
const MAX_BLUR: f32 = 0.05;
/// Extra zoom of the focus rack at the start (incoming) / end (outgoing).
const RACK_ZOOM: f32 = 0.04;

struct Layer {
    wallpaper: Wallpaper,
    texture: Option<GpuImage>,
}

impl Layer {
    fn new(wallpaper: Wallpaper) -> Self {
        Self {
            wallpaper,
            texture: None,
        }
    }
}

struct Transition {
    started: Instant,
    duration: Duration,
}

pub struct FrameParams {
    blur_a: f32,
    blur_b: f32,
    fade: f32,
    zoom_a: f32,
    zoom_b: f32,
}

#[derive(Default)]
pub struct Scene {
    from: Option<Layer>,
    to: Option<Layer>,
    transition: Option<Transition>,
    intro_started: Option<Instant>,
    /// Bumped whenever the bound textures change.
    generation: u64,
}

impl Scene {
    pub fn current(&self) -> Option<&Wallpaper> {
        self.to.as_ref().map(|layer| &layer.wallpaper)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether `path` is on screen and must survive cache eviction.
    pub fn uses(&self, path: &Path) -> bool {
        [&self.from, &self.to]
            .into_iter()
            .flatten()
            .any(|layer| layer.wallpaper.path == path)
    }

    pub fn is_animating(&self) -> bool {
        self.transition.is_some() || self.intro_started.is_some()
    }

    /// Animates to `wallpaper`: a blur→focus transition, or the launch intro
    /// zoom when nothing was shown before.
    pub fn begin(&mut self, wallpaper: Wallpaper, duration: f32, now: Instant) {
        self.from = self.to.replace(Layer::new(wallpaper));
        if self.from.is_some() {
            self.transition = Some(Transition {
                started: now,
                duration: Duration::from_secs_f32(duration.max(MIN_TRANSITION_SECS)),
            });
            self.intro_started = None;
        } else {
            self.intro_started = Some(now);
        }
        self.generation += 1;
    }

    /// Shows `wallpaper` with no animation, for when no GPU is available.
    pub fn show_instantly(&mut self, wallpaper: Wallpaper) {
        self.from = None;
        self.to = Some(Layer::new(wallpaper));
        self.transition = None;
        self.intro_started = None;
        self.generation += 1;
    }

    /// Swaps the pixels of the current wallpaper, e.g. a video poster for the
    /// frame actually on screen when a transition away from it starts.
    pub fn replace_current_image(&mut self, image: Arc<ShmImage>) {
        if let Some(layer) = &mut self.to {
            layer.wallpaper.image = image;
            layer.texture = None;
            self.generation += 1;
        }
    }

    /// Retires finished animations, dropping the outgoing layer.
    pub fn advance(&mut self, now: Instant) {
        if self
            .transition
            .as_ref()
            .is_some_and(|t| now >= t.started + t.duration)
        {
            self.transition = None;
            self.from = None;
            self.generation += 1;
        }
        if self
            .intro_started
            .is_some_and(|started| now >= started + INTRO_DURATION)
        {
            self.intro_started = None;
        }
    }

    pub fn params(&self, now: Instant) -> FrameParams {
        let t = self.transition.as_ref().map_or(1.0, |transition| {
            (now.saturating_duration_since(transition.started)
                .as_secs_f32()
                / transition.duration.as_secs_f32())
            .clamp(0.0, 1.0)
        });
        let intro = self.intro_started.map_or(REST_ZOOM as f32, |started| {
            let p =
                now.saturating_duration_since(started).as_secs_f32() / INTRO_DURATION.as_secs_f32();
            INTRO_START_ZOOM + (REST_ZOOM as f32 - INTRO_START_ZOOM) * INTRO.ease(p)
        });
        let eased = STANDARD.ease(t);
        FrameParams {
            blur_a: MAX_BLUR * STANDARD.ease(t / 0.7),
            blur_b: MAX_BLUR * (1.0 - STANDARD.ease((t - 0.3) / 0.7)),
            fade: CROSSFADE.ease(eased),
            zoom_a: intro * (1.0 + RACK_ZOOM * eased),
            zoom_b: intro * (1.0 + RACK_ZOOM * (1.0 - eased)),
        }
    }

    pub fn uniforms(&self, params: &FrameParams, screen: Size) -> Uniforms {
        let map = |layer: &Option<Layer>, zoom: f32| {
            layer.as_ref().map_or([1.0, 1.0, 0.0, 0.0], |layer| {
                cover_uv_map(layer.wallpaper.image.size(), screen, f64::from(zoom))
            })
        };
        Uniforms {
            map_a: map(&self.from, params.zoom_a),
            map_b: map(&self.to, params.zoom_b),
            aspect: screen.height as f32 / screen.width.max(1) as f32,
            blur_a: params.blur_a,
            blur_b: params.blur_b,
            fade: params.fade,
        }
    }

    pub fn upload(&mut self, renderer: &Renderer) {
        for layer in [&mut self.from, &mut self.to].into_iter().flatten() {
            if layer.texture.is_none() {
                layer.texture = Some(GpuImage::upload(
                    &renderer.device,
                    &renderer.queue,
                    &layer.wallpaper.image,
                ));
                self.generation += 1;
            }
        }
    }

    pub fn release_textures(&mut self) {
        for layer in [&mut self.from, &mut self.to].into_iter().flatten() {
            layer.texture = None;
        }
        self.generation += 1;
    }

    pub fn textures<'a>(&'a self, placeholder: &'a GpuImage) -> (&'a GpuImage, &'a GpuImage) {
        let texture = |layer: &'a Option<Layer>| {
            layer
                .as_ref()
                .and_then(|l| l.texture.as_ref())
                .unwrap_or(placeholder)
        };
        (texture(&self.from), texture(&self.to))
    }
}
