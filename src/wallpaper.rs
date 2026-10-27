use std::path::PathBuf;
use std::sync::Arc;

use crate::decode::WallpaperKind;
use crownpaper::shm::ShmImage;

/// A decoded wallpaper: a still, or a video represented by its poster frame.
#[derive(Clone)]
pub struct Wallpaper {
    pub path: PathBuf,
    pub kind: WallpaperKind,
    pub image: Arc<ShmImage>,
}

impl Wallpaper {
    pub fn is_video(&self) -> bool {
        self.kind == WallpaperKind::Video
    }
}
