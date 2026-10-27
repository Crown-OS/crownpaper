//! SIMD downscaling straight into shared memory.

use anyhow::{Context, Result};
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;

/// Resizes tightly packed 4-byte pixels from `source` to `target` into a new
/// shared-memory image. Channel order is preserved.
pub fn fit_into_shm(pixels: &[u8], source: Size, target: Size) -> Result<ShmImage> {
    let mut fitted = ShmImage::new(target).context("allocating the resize buffer")?;
    resize(pixels, source, fitted.pixels_mut(), target, PixelType::U8x4)?;
    Ok(fitted)
}

pub fn resize(
    pixels: &[u8],
    source: Size,
    output: &mut [u8],
    target: Size,
    pixel_type: PixelType,
) -> Result<()> {
    let src = ImageRef::new(source.width, source.height, pixels, pixel_type)?;
    let mut dst = Image::from_slice_u8(target.width, target.height, output, pixel_type)?;
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
        .use_alpha(false);
    Resizer::new().resize(&src, &mut dst, &options)?;
    Ok(())
}
