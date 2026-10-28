//! The blur→focus render pipeline.
//!
//! The shader ships as SPIR-V compiled by `build.rs` and goes to the driver
//! as-is, so bringing the renderer up never parses or translates WGSL. The
//! driver's own compile is skipped too once the persisted pipeline cache is
//! warm (see [`super::context`]).

use bytemuck::{Pod, Zeroable};

use super::texture::GpuImage;

/// The format every surface is rendered through: an sRGB view, so the GPU
/// encodes on store.
pub const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

/// Must match `Uniforms` in `shaders/blur_focus.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Uniforms {
    pub map_a: [f32; 4],
    pub map_b: [f32; 4],
    pub aspect: f32,
    pub blur_a: f32,
    pub blur_b: f32,
    pub fade: f32,
}

pub struct Pipeline {
    shader: wgpu::ShaderModule,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    cache: Option<wgpu::PipelineCache>,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

impl Pipeline {
    pub fn new(device: &wgpu::Device, cache: Option<wgpu::PipelineCache>) -> Self {
        // SAFETY: the SPIR-V was produced and validated by naga in build.rs
        // from the WGSL this pipeline layout was written against.
        let shader = unsafe {
            device.create_shader_module_passthrough(wgpu::include_spirv_raw!(concat!(
                env!("OUT_DIR"),
                "/blur_focus.spv"
            )))
        };
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wallpaper"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<Uniforms>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
                texture_entry(3),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wallpaper"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("wallpaper"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mut pipeline = Self {
            shader,
            bind_group_layout,
            pipeline_layout,
            sampler,
            cache,
            pipelines: Vec::with_capacity(1),
        };
        pipeline.for_format(device, TARGET_FORMAT);
        pipeline
    }

    pub fn cache(&self) -> Option<&wgpu::PipelineCache> {
        self.cache.as_ref()
    }

    pub fn get(&self, format: wgpu::TextureFormat) -> Option<&wgpu::RenderPipeline> {
        self.pipelines
            .iter()
            .find(|(f, _)| *f == format)
            .map(|(_, pipeline)| pipeline)
    }

    pub fn for_format(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> &wgpu::RenderPipeline {
        let index = match self.pipelines.iter().position(|(f, _)| *f == format) {
            Some(index) => index,
            None => {
                self.pipelines.push((format, self.create(device, format)));
                self.pipelines.len() - 1
            }
        };
        &self.pipelines[index].1
    }

    fn create(&self, device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wallpaper"),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: self.cache.as_ref(),
        })
    }

    pub fn create_uniform_buffer(&self, device: &wgpu::Device) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wallpaper uniforms"),
            size: size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        uniforms: &wgpu::Buffer,
        from: &GpuImage,
        to: &GpuImage,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wallpaper"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&from.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&to.view),
                },
            ],
        })
    }
}
