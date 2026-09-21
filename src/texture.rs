//! GPU wallpaper textures.

use crownshell::wgpu;

use crate::decode::DecodedImage;

/// An uploaded wallpaper: its view plus pixel dimensions for aspect-correct
/// cover mapping in the shader. Cheap to clone — wgpu resources are
/// internally reference-counted.
#[derive(Clone)]
pub struct GpuImage {
    pub view: wgpu::TextureView,
    pub size: [f32; 2],
}

impl GpuImage {
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, image: &DecodedImage) -> Self {
        Self::from_rgba(device, queue, &image.rgba, image.width, image.height)
    }

    /// 1x1 black texture bound to shader slots with no wallpaper yet.
    pub fn placeholder(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self::from_rgba(device, queue, &[0, 0, 0, 255], 1, 1)
    }

    fn from_rgba(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wallpaper"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            size,
        );
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            size: [width as f32, height as f32],
        }
    }
}
