//! Cheap format sniffing, so `set` can be acknowledged or rejected from a
//! file's first bytes without decoding anything.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use image::ImageFormat;

const HEADER_LEN: usize = 16;
const EBML_MAGIC: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
/// ISO-BMFF brands that are still images rather than video.
const STILL_BRANDS: &[&[u8]] = &[b"avif", b"avis", b"heic", b"heix", b"mif1", b"msf1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallpaperKind {
    Still(StillFormat),
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillFormat {
    Jpeg,
    Raster(ImageFormat),
}

pub fn probe(path: &Path) -> Result<WallpaperKind> {
    let mut header = [0u8; HEADER_LEN];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut header))
        .with_context(|| format!("reading {}", path.display()))?;
    classify(&header[..read]).with_context(|| format!("{}: unsupported format", path.display()))
}

fn classify(header: &[u8]) -> Result<WallpaperKind> {
    if header.starts_with(EBML_MAGIC) {
        return Ok(WallpaperKind::Video);
    }
    if let Some(brand) = header
        .get(4..8)
        .filter(|tag| *tag == b"ftyp")
        .and(header.get(8..12))
    {
        if STILL_BRANDS.contains(&brand) {
            bail!("HEIF/AVIF stills are not supported");
        }
        return Ok(WallpaperKind::Video);
    }
    match image::guess_format(header)? {
        ImageFormat::Jpeg => Ok(WallpaperKind::Still(StillFormat::Jpeg)),
        format @ (ImageFormat::Png | ImageFormat::WebP | ImageFormat::Bmp) => {
            Ok(WallpaperKind::Still(StillFormat::Raster(format)))
        }
        other => bail!("{other:?} is not supported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_stills() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1,
        ];
        assert_eq!(
            classify(&jpeg).ok(),
            Some(WallpaperKind::Still(StillFormat::Jpeg))
        );
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            classify(&png).ok(),
            Some(WallpaperKind::Still(StillFormat::Raster(ImageFormat::Png)))
        );
    }

    #[test]
    fn recognizes_video_containers() {
        let mp4 = *b"\0\0\0\x20ftypisom\0\0\x02\0";
        assert_eq!(classify(&mp4).ok(), Some(WallpaperKind::Video));
        let mov = *b"\0\0\0\x14ftypqt  \0\0\x02\0";
        assert_eq!(classify(&mov).ok(), Some(WallpaperKind::Video));
        let webm = [0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81];
        assert_eq!(classify(&webm).ok(), Some(WallpaperKind::Video));
    }

    #[test]
    fn rejects_heif_and_garbage() {
        assert!(classify(b"\0\0\0\x1cftypavif\0\0\0\0").is_err());
        assert!(classify(b"plain text file!").is_err());
        assert!(classify(&[]).is_err());
    }
}
