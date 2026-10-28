//! GPU wallpaper textures.

use wgpu::util::{DeviceExt, TextureDataOrder};

use crownpaper::shm::ShmImage;
use crownpaper::util::cover::Size;

/// An uploaded wallpaper. Cheap to clone — wgpu resources are internally
/// reference-counted.
#[derive(Clone)]
pub struct GpuImage {
    pub view: wgpu::TextureView,
}

impl GpuImage {
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, image: &ShmImage) -> Self {
        Self::from_bgrx(device, queue, image.pixels(), image.size())
    }

    /// 1x1 black texture bound to shader slots with no wallpaper.
    pub fn placeholder(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self::from_bgrx(device, queue, &[0, 0, 0, 0xFF], Size::new(1, 1))
    }

    fn from_bgrx(device: &wgpu::Device, queue: &wgpu::Queue, pixels: &[u8], size: Size) -> Self {
        let texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("wallpaper"),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            TextureDataOrder::LayerMajor,
            pixels,
        );
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
        }
    }
}
