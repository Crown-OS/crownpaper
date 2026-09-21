//! Path-keyed cache of uploaded wallpaper textures.
//!
//! Two retention classes share one map: entries used implicitly by `set`
//! ride an LRU list capped at [`IMPLICIT_CAPACITY`], while explicitly
//! preloaded entries are *pinned* — exempt from the cap until `unload`
//! releases them. Callers pass a `protected` predicate (the wallpapers
//! currently on screen) that eviction always honors.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::texture::GpuImage;

/// How many unpinned wallpapers stay resident for instant re-`set`.
const IMPLICIT_CAPACITY: usize = 6;

#[derive(Default)]
pub struct TextureCache {
    entries: HashMap<PathBuf, Entry>,
    /// Recency order of the unpinned entries; the front is evicted first.
    lru: VecDeque<PathBuf>,
}

struct Entry {
    image: GpuImage,
    pinned: bool,
}

impl TextureCache {
    /// Look up a resident texture, refreshing its recency.
    pub fn get(&mut self, path: &Path) -> Option<GpuImage> {
        let entry = self.entries.get(path)?;
        let image = entry.image.clone();
        if !entry.pinned {
            self.touch(path);
        }
        Some(image)
    }

    /// Pin a path that is already resident. Returns false when absent.
    pub fn pin_resident(&mut self, path: &Path) -> bool {
        let Some(entry) = self.entries.get_mut(path) else {
            return false;
        };
        if !entry.pinned {
            entry.pinned = true;
            self.lru.retain(|p| p != path);
        }
        true
    }

    pub fn insert(
        &mut self,
        path: PathBuf,
        image: GpuImage,
        pinned: bool,
        protected: impl Fn(&Path) -> bool,
    ) {
        if let Some(entry) = self.entries.get_mut(&path) {
            entry.image = image;
            if pinned && !entry.pinned {
                entry.pinned = true;
                self.lru.retain(|p| p != &path);
            } else if !entry.pinned {
                self.touch(&path);
            }
            return;
        }
        self.entries.insert(path.clone(), Entry { image, pinned });
        if !pinned {
            self.lru.push_back(path);
            self.enforce_cap(&protected);
        }
    }

    /// Release a preloaded entry. Entries still on screen are only demoted to
    /// the implicit LRU so they age out once they leave the screen.
    pub fn unload(&mut self, path: &Path, protected: impl Fn(&Path) -> bool) {
        let Some(entry) = self.entries.get_mut(path) else {
            return;
        };
        if protected(path) {
            if entry.pinned {
                entry.pinned = false;
                self.lru.push_back(path.to_path_buf());
                self.enforce_cap(&protected);
            }
            return;
        }
        self.entries.remove(path);
        self.lru.retain(|p| p != path);
        log::debug!("unloaded {}", path.display());
    }

    fn touch(&mut self, path: &Path) {
        if let Some(pos) = self.lru.iter().position(|p| p == path) {
            let recent = self.lru.remove(pos).expect("position came from iter");
            self.lru.push_back(recent);
        }
    }

    fn enforce_cap(&mut self, protected: &impl Fn(&Path) -> bool) {
        while self.lru.len() > IMPLICIT_CAPACITY {
            let Some(pos) = self.lru.iter().position(|p| !protected(p)) else {
                return; // everything over cap is on screen; try again later
            };
            let victim = self.lru.remove(pos).expect("position came from iter");
            self.entries.remove(&victim);
            log::debug!("evicted {}", victim.display());
        }
    }
}
