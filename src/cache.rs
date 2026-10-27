//! Wallpapers pinned by `preload`.
//!
//! Entries are screen-sized images in shared memory, independent of the GPU,
//! so they survive the renderer being torn down. Nothing is cached
//! implicitly: an entry lives from `preload` until `unload`, or — when it is
//! unloaded while on screen — until it leaves the screen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::wallpaper::Wallpaper;

#[derive(Default)]
pub struct WallpaperCache {
    entries: HashMap<PathBuf, Entry>,
}

struct Entry {
    wallpaper: Wallpaper,
    pinned: bool,
}

impl WallpaperCache {
    pub fn get(&self, path: &Path) -> Option<Wallpaper> {
        self.entries.get(path).map(|entry| entry.wallpaper.clone())
    }

    pub fn has_pinned(&self) -> bool {
        self.entries.values().any(|entry| entry.pinned)
    }

    /// Pins a path that is already resident. Returns false when absent.
    pub fn pin_resident(&mut self, path: &Path) -> bool {
        self.entries
            .get_mut(path)
            .map(|entry| entry.pinned = true)
            .is_some()
    }

    pub fn pin(&mut self, wallpaper: Wallpaper) {
        let path = wallpaper.path.clone();
        self.entries.insert(
            path,
            Entry {
                wallpaper,
                pinned: true,
            },
        );
    }

    /// Releases a preloaded entry; one still on screen lingers until
    /// [`Self::prune`] sees it leave.
    pub fn unload(&mut self, path: &Path, on_screen: impl Fn(&Path) -> bool) {
        if on_screen(path) {
            if let Some(entry) = self.entries.get_mut(path) {
                entry.pinned = false;
            }
        } else if self.entries.remove(path).is_some() {
            log::debug!("unloaded {}", path.display());
        }
    }

    pub fn prune(&mut self, on_screen: impl Fn(&Path) -> bool) {
        self.entries
            .retain(|path, entry| entry.pinned || on_screen(path));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crownpaper::shm::ShmImage;
    use crownpaper::util::cover::Size;

    use super::*;
    use crate::decode::WallpaperKind;

    fn wallpaper(path: &str) -> Wallpaper {
        Wallpaper {
            path: PathBuf::from(path),
            kind: WallpaperKind::Video,
            image: Arc::new(ShmImage::new(Size::new(1, 1)).expect("memfd")),
        }
    }

    #[test]
    fn unloading_an_on_screen_entry_defers_until_it_leaves() {
        let mut cache = WallpaperCache::default();
        cache.pin(wallpaper("/a"));
        cache.unload(Path::new("/a"), |_| true);
        assert!(!cache.has_pinned());
        cache.prune(|_| true);
        assert!(cache.get(Path::new("/a")).is_some());
        cache.prune(|_| false);
        assert!(cache.get(Path::new("/a")).is_none());
    }

    #[test]
    fn pinned_entries_survive_pruning() {
        let mut cache = WallpaperCache::default();
        cache.pin(wallpaper("/b"));
        cache.prune(|_| false);
        assert!(cache.get(Path::new("/b")).is_some());
        assert!(cache.pin_resident(Path::new("/b")));
        assert!(!cache.pin_resident(Path::new("/missing")));
    }
}
