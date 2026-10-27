//! PNG, WebP and BMP through the `image` crate.
//!
//! The image is resized in whatever channel layout it decoded to, and only
//! the screen-sized result is expanded to BGRX, so no full-size RGBA copy of
//! the source is ever made.

use std::io::Cursor;

use anyhow::{Context, Result};
use fast_image_resize::PixelType;
use image::{DynamicImage, ImageFormat, ImageReader};

use super::resize::resize;
use crownpaper::shm::ShmImage;
use crownpaper::util::cover::{Size, cover_size};

pub fn decode(bytes: &[u8], format: ImageFormat, screen: Size) -> Result<ShmImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.no_limits();
    let decoded = match reader.decode()? {
        rgb @ DynamicImage::ImageRgb8(_) => rgb,
        rgba @ DynamicImage::ImageRgba8(_) => rgba,
        other => DynamicImage::ImageRgba8(other.to_rgba8()),
    };
    let source = Size::new(decoded.width(), decoded.height());
    let target = cover_size(source, screen);
    let (pixel_type, channels) = match decoded {
        DynamicImage::ImageRgb8(_) => (PixelType::U8x3, 3),
        _ => (PixelType::U8x4, 4),
    };

    let resized;
    let pixels = if source == target {
        decoded.as_bytes()
    } else {
        let mut buffer = vec![0u8; target.width as usize * target.height as usize * channels];
        resize(decoded.as_bytes(), source, &mut buffer, target, pixel_type)?;
        resized = buffer;
        &resized
    };

    let mut image = ShmImage::new(target).context("allocating the image buffer")?;
    pixels
        .chunks_exact(channels)
        .zip(image.pixels_mut().chunks_exact_mut(4))
        .for_each(|(rgb, bgrx)| bgrx.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xFF]));
    Ok(image)
}
