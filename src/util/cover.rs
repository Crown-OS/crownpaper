//! Cover-fit geometry shared by the decoder, the shader uniforms and the
//! compositor-side viewport crop, so all three frame an image identically.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    fn as_f64(self) -> (f64, f64) {
        (f64::from(self.width.max(1)), f64::from(self.height.max(1)))
    }
}

/// Region of an image that fills a screen, in image pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The smallest size, aspect preserved, at which `image` still covers
/// `screen`. Never larger than the image itself.
pub fn cover_size(image: Size, screen: Size) -> Size {
    let (iw, ih) = image.as_f64();
    let (sw, sh) = screen.as_f64();
    let scale = (sw / iw).max(sh / ih).min(1.0);
    Size::new(
        ((iw * scale).round() as u32).max(1),
        ((ih * scale).round() as u32).max(1),
    )
}

/// The centered part of `image` that covers `screen` at `zoom` (>= 1 crops
/// further in).
pub fn cover_crop(image: Size, screen: Size, zoom: f64) -> Crop {
    let (iw, ih) = image.as_f64();
    let (sw, sh) = screen.as_f64();
    let scale = (sw / iw).max(sh / ih) * zoom.max(1e-4);
    let (width, height) = (sw / scale, sh / scale);
    Crop {
        x: (iw - width) * 0.5,
        y: (ih - height) * 0.5,
        width,
        height,
    }
}

/// [`cover_crop`] in normalized texture coordinates, as the shader's
/// `uv * scale + offset` mapping.
pub fn cover_uv_map(image: Size, screen: Size, zoom: f64) -> [f32; 4] {
    let (iw, ih) = image.as_f64();
    let crop = cover_crop(image, screen, zoom);
    [
        (crop.width / iw) as f32,
        (crop.height / ih) as f32,
        (crop.x / iw) as f32,
        (crop.y / ih) as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_size_shrinks_to_the_binding_axis() {
        let screen = Size::new(1920, 1080);
        assert_eq!(
            cover_size(Size::new(6000, 4000), screen),
            Size::new(1920, 1280)
        );
        assert_eq!(
            cover_size(Size::new(8000, 2000), screen),
            Size::new(4320, 1080)
        );
    }

    #[test]
    fn cover_size_never_upscales() {
        let small = Size::new(800, 600);
        assert_eq!(cover_size(small, Size::new(3840, 2160)), small);
    }

    #[test]
    fn crop_is_centered_and_keeps_screen_aspect() {
        let crop = cover_crop(Size::new(2000, 1000), Size::new(1000, 1000), 1.0);
        assert_eq!(
            crop,
            Crop {
                x: 500.0,
                y: 0.0,
                width: 1000.0,
                height: 1000.0
            }
        );
        let zoomed = cover_crop(Size::new(1000, 1000), Size::new(1000, 1000), 2.0);
        assert_eq!(
            zoomed,
            Crop {
                x: 250.0,
                y: 250.0,
                width: 500.0,
                height: 500.0
            }
        );
    }

    #[test]
    fn uv_map_matches_crop() {
        let map = cover_uv_map(Size::new(2000, 1000), Size::new(1000, 1000), 1.0);
        assert_eq!(map, [0.5, 1.0, 0.25, 0.0]);
    }
}
