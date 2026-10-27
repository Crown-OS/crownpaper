//! JPEG through libjpeg-turbo, scaled inside the IDCT.
//!
//! Decoding at 1/2, 1/4 or 1/8 size skips most of the inverse DCT and color
//! conversion work, so a 24 MP photo bound for a 1080p screen costs a
//! fraction of a full decode and never exists at full size in memory.

use anyhow::{Context, Result};
use turbojpeg::{Decompressor, Image, PixelFormat, ScalingFactor};

use super::resize::fit_into_shm;
use crownpaper::shm::{BYTES_PER_PIXEL, ShmImage};
use crownpaper::util::cover::{Size, cover_size};

pub fn decode(jpeg: &[u8], screen: Size) -> Result<ShmImage> {
    let mut decompressor = Decompressor::new()?;
    let header = decompressor.read_header(jpeg)?;
    let source = Size::new(header.width as u32, header.height as u32);
    let target = cover_size(source, screen);
    let factor = smallest_covering_factor(source, target);
    decompressor.set_scaling_factor(factor)?;
    let scaled = header.scaled(factor);
    let scaled_size = Size::new(scaled.width as u32, scaled.height as u32);

    let mut decoded = ShmImage::new(scaled_size).context("allocating the decode buffer")?;
    decompressor.decompress(
        jpeg,
        Image {
            pixels: decoded.pixels_mut(),
            width: scaled.width,
            pitch: scaled.width * BYTES_PER_PIXEL as usize,
            height: scaled.height,
            format: PixelFormat::BGRA,
        },
    )?;
    if scaled_size == target {
        return Ok(decoded);
    }
    fit_into_shm(decoded.pixels(), scaled_size, target)
}

/// The most aggressive DCT scaling that still leaves at least `target`
/// pixels on both axes; the remainder is a cheap resize of a small image.
fn smallest_covering_factor(source: Size, target: Size) -> ScalingFactor {
    Decompressor::supported_scaling_factors()
        .into_iter()
        .filter(|factor| factor.num() <= factor.denom())
        .filter(|factor| {
            factor.scale(source.width as usize) >= target.width as usize
                && factor.scale(source.height as usize) >= target.height as usize
        })
        .min_by(|a, b| (a.num() * b.denom()).cmp(&(b.num() * a.denom())))
        .unwrap_or(ScalingFactor::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_smallest_factor_that_still_covers() {
        let factor = smallest_covering_factor(Size::new(6000, 4000), Size::new(1920, 1280));
        assert_eq!((factor.num(), factor.denom()), (3, 8));
        let factor = smallest_covering_factor(Size::new(8000, 4000), Size::new(1000, 500));
        assert_eq!((factor.num(), factor.denom()), (1, 8));
    }

    #[test]
    fn falls_back_to_full_size() {
        let size = Size::new(1920, 1080);
        let factor = smallest_covering_factor(size, size);
        assert_eq!(factor.num(), factor.denom());
    }
}
