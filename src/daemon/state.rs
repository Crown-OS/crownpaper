//! Daemon state: requests, decode results, the scene and the renderer's
//! lifecycle. Everything here is plain data driven from the event loop; the
//! Wayland side reconciles against it in [`super::Daemon::sync`].

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use calloop::channel::Sender;

use super::scene::Scene;
use crate::cache::WallpaperCache;
use crate::decode::{DecodeResult, Decoded, Decoder, Job, Priority, WallpaperKind};
use crate::gpu::Renderer;
use crate::video::{ImportableFormats, Player, VideoHelper};
use crate::wallpaper::Wallpaper;
use crate::wayland::Content;
use crownpaper::util::cover::Size;
use crownpaper::util::memory::trim_heap;

/// How long an idle renderer lingers before it is torn down, so a burst of
/// `set`s does not rebuild the device for every one.
const RENDERER_GRACE: Duration = Duration::from_secs(2);

pub enum RendererSlot {
    Cold,
    Building,
    Warm(Renderer),
}

/// What to do with a path once its decode completes.
#[derive(Clone, Copy)]
struct LoadIntent {
    kind: WallpaperKind,
    /// Transition duration, when the wallpaper should be shown on arrival.
    display: Option<f32>,
    /// Keep it pinned in the cache (preload).
    pin: bool,
}

pub struct State {
    pub scene: Scene,
    pub renderer: RendererSlot,
    pub playback: Option<Player>,
    pub quit: bool,
    /// Largest output in physical pixels; the decode target.
    pub screen: Size,
    pub video_device: Option<PathBuf>,
    pub importable: ImportableFormats,
    cache: WallpaperCache,
    /// Started on first use, so no thread exists before startup is done.
    decoder: Option<Decoder>,
    decoded: Sender<DecodeResult>,
    renderer_ready: Sender<Result<Renderer, String>>,
    renderer_held: bool,
    renderer_failed: bool,
    idle_since: Option<Instant>,
    /// Paths in the decode queue and what they were requested for.
    in_flight: HashMap<PathBuf, LoadIntent>,
    /// Decoded and waiting for the renderer before its transition can start.
    pending_display: Option<(Wallpaper, f32)>,
    /// An opened video waiting for its transition to finish.
    pending_source: Option<(PathBuf, VideoHelper)>,
    /// `Changed` events not yet emitted on the bus.
    changed: Vec<PathBuf>,
}

impl State {
    pub fn new(
        decoded: Sender<DecodeResult>,
        renderer_ready: Sender<Result<Renderer, String>>,
    ) -> Self {
        Self {
            scene: Scene::default(),
            renderer: RendererSlot::Cold,
            playback: None,
            quit: false,
            screen: Size::new(1920, 1080),
            video_device: None,
            importable: Arc::new([]),
            cache: WallpaperCache::default(),
            decoder: None,
            decoded,
            renderer_ready,
            renderer_held: false,
            renderer_failed: false,
            idle_since: None,
            in_flight: HashMap::new(),
            pending_display: None,
            pending_source: None,
            changed: Vec::new(),
        }
    }

    /// `set`: show `path`, transitioning over `duration` seconds.
    pub fn request_set(&mut self, path: PathBuf, kind: WallpaperKind, duration: f32) {
        self.renderer_failed = false;
        self.cancel_superseded(&path);
        if let Some(wallpaper) = self.resident(&path) {
            let needs_source = wallpaper.is_video() && !self.is_playing(&path);
            self.display(wallpaper, duration);
            if !needs_source {
                return;
            }
        }
        match self.in_flight.entry(path.clone()) {
            Entry::Occupied(mut e) => e.get_mut().display = Some(duration),
            Entry::Vacant(e) => {
                e.insert(LoadIntent {
                    kind,
                    display: Some(duration),
                    pin: false,
                });
                self.enqueue(path, kind, Priority::Display);
            }
        }
    }

    /// `preload`: decode and pin without displaying.
    pub fn request_preload(&mut self, path: PathBuf, kind: WallpaperKind) {
        if self.cache.pin_resident(&path) {
            return;
        }
        match self.in_flight.entry(path.clone()) {
            Entry::Occupied(mut e) => e.get_mut().pin = true,
            Entry::Vacant(e) => {
                e.insert(LoadIntent {
                    kind,
                    display: None,
                    pin: true,
                });
                self.enqueue(path, kind, Priority::Preload);
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

    /// `preload_renderer` / `unload_renderer`: keep the GPU warm without
    /// naming a wallpaper.
    pub fn hold_renderer(&mut self, held: bool) {
        self.renderer_held = held;
    }

    pub fn request_quit(&mut self) {
        self.quit = true;
    }

    pub fn on_decoded(&mut self, decoded: DecodeResult) {
        let Some(intent) = self.in_flight.remove(&decoded.path) else {
            return;
        };
        let (image, source) = match decoded.result {
            Ok(Decoded::Still(image)) => (image, None),
            Ok(Decoded::Video(opened)) => (opened.poster, Some(opened.helper)),
            Err(e) => {
                log::warn!("wallpaper load failed: {e}");
                return;
            }
        };
        if intent.display.is_none() && !intent.pin {
            return; // unloaded while decoding
        }
        let wallpaper = self.cache.get(&decoded.path).unwrap_or_else(|| Wallpaper {
            path: decoded.path.clone(),
            kind: intent.kind,
            image: Arc::new(image),
        });
        if intent.pin {
            self.cache.pin(wallpaper.clone());
        }
        if let Some(duration) = intent.display {
            if let Some(source) = source {
                self.pending_source = Some((decoded.path, source));
            }
            self.display(wallpaper, duration);
        }
    }

    pub fn on_renderer(&mut self, built: Result<Renderer, String>) {
        match built {
            Ok(renderer) => self.renderer = RendererSlot::Warm(renderer),
            Err(e) => {
                log::error!("renderer unavailable, wallpapers change without animation: {e}");
                self.renderer = RendererSlot::Cold;
                self.renderer_failed = true;
            }
        }
    }

    /// Starts the queued transition once the renderer is up (or shows it
    /// outright when there is no GPU). Returns whether an animation began.
    pub fn start_pending(&mut self, now: Instant) -> bool {
        if !self.can_start() {
            return false;
        }
        let Some((wallpaper, duration)) = self.pending_display.take() else {
            return false;
        };
        if let Some(mut player) = self.playback.take() {
            match player.snapshot() {
                Ok(Some(still)) => self.scene.replace_current_image(Arc::new(still)),
                Ok(None) => {}
                Err(e) => log::warn!("snapshotting the video: {e:#}"),
            }
        }
        match &self.renderer {
            RendererSlot::Warm(renderer) => {
                self.scene.begin(wallpaper, duration, now);
                self.scene.upload(renderer);
                true
            }
            _ => {
                self.scene.show_instantly(wallpaper);
                false
            }
        }
    }

    pub fn can_start(&self) -> bool {
        self.pending_display.is_some()
            && (self.renderer_failed || matches!(self.renderer, RendererSlot::Warm(_)))
    }

    /// Once the screen is at rest, frees what left it and starts a video.
    pub fn settle(&mut self) {
        if self.scene.is_animating() || self.pending_display.is_some() {
            return;
        }
        let scene = &self.scene;
        self.cache.prune(|path| scene.uses(path));
        if self.playback.is_some() {
            return;
        }
        let Some(current) = self.scene.current().filter(|w| w.is_video()) else {
            return;
        };
        if self
            .pending_source
            .as_ref()
            .is_none_or(|(path, _)| *path != current.path)
        {
            return;
        }
        if let Some((_, helper)) = self.pending_source.take() {
            self.playback = Some(Player::new(helper));
        }
    }

    /// What the layer surfaces show while nothing animates.
    pub fn rest_content(&self) -> Option<Content> {
        if let Some(frame) = self.playback.as_ref().and_then(Player::current) {
            return Some(Content::Video(Arc::clone(frame)));
        }
        self.scene
            .current()
            .map(|wallpaper| Content::Image(Arc::clone(&wallpaper.image)))
    }

    /// Brings the renderer up when something needs it and reports when an
    /// idle one should be torn down.
    #[must_use]
    pub fn drive_renderer(&mut self, now: Instant) -> bool {
        let wanted = self.renderer_wanted();
        if wanted {
            self.idle_since = None;
        }
        match self.renderer {
            RendererSlot::Cold if wanted && !self.renderer_failed => {
                match Renderer::spawn_build(self.renderer_ready.clone()) {
                    Ok(()) => self.renderer = RendererSlot::Building,
                    Err(e) => self.on_renderer(Err(format!("{e:#}"))),
                }
                false
            }
            RendererSlot::Warm(_) if !wanted => {
                now >= *self.idle_since.get_or_insert(now) + RENDERER_GRACE
            }
            _ => false,
        }
    }

    pub fn tear_down_renderer(&mut self) {
        self.scene.release_textures();
        self.renderer = RendererSlot::Cold;
        self.idle_since = None;
        trim_heap();
        log::info!("renderer released");
    }

    /// When the loop must wake on its own to retire an idle renderer.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        self.idle_since
            .map(|since| (since + RENDERER_GRACE).saturating_duration_since(now))
    }

    /// Drain the `Changed` events queued since the last flush.
    pub fn take_changed(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.changed)
    }

    fn renderer_wanted(&self) -> bool {
        self.scene.is_animating()
            || self.pending_display.is_some()
            || self.renderer_held
            || self.cache.has_pinned()
            || self
                .in_flight
                .values()
                .any(|intent| intent.display.is_some())
    }

    /// A decoded copy of `path` already in memory: pinned, or on screen.
    fn resident(&self, path: &Path) -> Option<Wallpaper> {
        self.cache
            .get(path)
            .or_else(|| self.scene.current().filter(|w| w.path == path).cloned())
    }

    fn is_playing(&self, path: &Path) -> bool {
        (self.playback.is_some() && self.scene.current().is_some_and(|w| w.path == path))
            || self.pending_source.as_ref().is_some_and(|(p, _)| p == path)
    }

    fn enqueue(&mut self, path: PathBuf, kind: WallpaperKind, priority: Priority) {
        if self.decoder.is_none() {
            match Decoder::spawn(self.decoded.clone()) {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(e) => {
                    log::error!("{e:#}");
                    return;
                }
            }
        }
        let job = Job {
            path,
            kind,
            screen: self.screen,
            video_device: self.video_device.clone(),
            importable: Arc::clone(&self.importable),
        };
        if let Some(decoder) = &self.decoder {
            decoder.enqueue(job, priority);
        }
    }

    /// Only the latest `set` may win: a newer request voids the display intent
    /// of any earlier one still in the pipeline, so a slow decode can never
    /// overwrite a wallpaper chosen after it.
    fn cancel_superseded(&mut self, winner: &Path) {
        for (path, intent) in &mut self.in_flight {
            if path != winner {
                intent.display = None;
            }
        }
        if self
            .pending_display
            .as_ref()
            .is_some_and(|(w, _)| w.path != winner)
        {
            self.pending_display = None;
        }
        if self
            .pending_source
            .as_ref()
            .is_some_and(|(path, _)| path != winner)
        {
            self.pending_source = None;
        }
    }

    fn display(&mut self, wallpaper: Wallpaper, duration: f32) {
        let already_shown = self.pending_display.is_none()
            && self
                .scene
                .current()
                .is_some_and(|w| w.path == wallpaper.path);
        if !already_shown {
            log::info!("wallpaper changing to {}", wallpaper.path.display());
            self.changed.push(wallpaper.path.clone());
            self.pending_display = Some((wallpaper, duration));
            return;
        }
        // Emitted even when redundant: subscribers mirror it into config.
        self.changed.push(wallpaper.path);
    }
}

#[cfg(test)]
mod tests {
    use crownpaper::shm::ShmImage;

    use super::*;

    fn state() -> State {
        let (decoded, _) = calloop::channel::channel();
        let (renderer_ready, _) = calloop::channel::channel();
        State::new(decoded, renderer_ready)
    }

    fn still(path: &str) -> Wallpaper {
        Wallpaper {
            path: PathBuf::from(path),
            kind: WallpaperKind::Video,
            image: Arc::new(ShmImage::new(Size::new(1, 1)).expect("memfd")),
        }
    }

    #[test]
    fn renderer_is_wanted_only_while_needed() {
        let mut state = state();
        assert!(!state.renderer_wanted());
        state.hold_renderer(true);
        assert!(state.renderer_wanted());
        state.hold_renderer(false);
        assert!(!state.renderer_wanted());
        state.cache.pin(still("/preloaded"));
        assert!(state.renderer_wanted());
        state.request_unload(Path::new("/preloaded"));
        assert!(!state.renderer_wanted());
    }

    #[test]
    fn a_ready_wallpaper_waits_for_the_renderer() {
        let mut state = state();
        state.display(still("/next"), 1.0);
        assert!(state.renderer_wanted());
        assert!(!state.can_start());
        state.renderer_failed = true;
        assert!(state.can_start());
        assert!(!state.start_pending(Instant::now()));
        assert!(
            state
                .scene
                .current()
                .is_some_and(|w| w.path == Path::new("/next"))
        );
        assert!(!state.renderer_wanted());
        assert_eq!(state.take_changed(), [PathBuf::from("/next")]);
    }
}
