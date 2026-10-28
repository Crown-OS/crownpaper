//! Video wallpapers, decoded by the `crownpaper-video` helper process.
//!
//! The daemon never links FFmpeg: opening a video spawns the helper, which
//! replies with a poster still and then streams frames as dmabufs (or memfds
//! when the compositor cannot import the decoder's buffers). Dropping the
//! last handle kills the helper, returning all of FFmpeg's memory.

mod helper;
mod player;

pub use helper::{OpenedVideo, VideoHelper, open};
pub use player::{DmabufPlanes, FrameContent, Player, VideoFrame};

use std::sync::Arc;

/// `(fourcc, modifier)` pairs the compositor can import.
pub type ImportableFormats = Arc<[(u32, u64)]>;
