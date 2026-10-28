//! The renderer: a Vulkan device on the low-power GPU plus the transition
//! pipeline.
//!
//! It is brought up only for animations and dropped again once the screen is
//! at rest, so construction is tuned for latency: it runs on its own thread
//! (overlapping the decode), compiles nothing at runtime, and reuses the
//! driver's pipeline binaries persisted from the previous run.

use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result};
use calloop::channel::Sender;

use super::pipeline::Pipeline;
use super::texture::GpuImage;

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter: wgpu::Adapter,
    pub pipeline: Pipeline,
    pub placeholder: GpuImage,
    /// Declared last so it drops last: Vulkan instance teardown is what
    /// unloads the driver libraries.
    pub instance: wgpu::Instance,
}

impl Renderer {
    pub fn spawn_build(ready: Sender<Result<Renderer, String>>) -> Result<()> {
        thread::Builder::new()
            .name("crownpaper-gpu".into())
            .spawn(move || {
                let renderer = Self::build().map_err(|e| format!("{e:#}"));
                // A closed channel means the daemon is shutting down.
                let _ = ready.send(renderer);
            })
            .context("spawning the renderer thread")?;
        Ok(())
    }

    fn build() -> Result<Self> {
        let started = std::time::Instant::now();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        // A wallpaper never needs the discrete GPU; keeping it asleep is
        // most of the power budget on a hybrid laptop.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .context("no Vulkan adapter")?;
        let adapter_info = adapter.get_info();

        let wanted = wgpu::Features::PASSTHROUGH_SHADERS | wgpu::Features::PIPELINE_CACHE;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("crownpaper"),
            required_features: adapter.features() & wanted,
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d,
                ..wgpu::Limits::downlevel_defaults()
            },
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .context("creating the Vulkan device")?;

        let cache_file = pipeline_cache_file(&adapter_info);
        let cached_data = cache_file
            .as_ref()
            .and_then(|path| std::fs::read(path).ok());
        let cache = device
            .features()
            .contains(wgpu::Features::PIPELINE_CACHE)
            .then(|| {
                // SAFETY: the data was produced by `PipelineCache::get_data` for
                // this adapter (the file name is keyed by vendor and device);
                // the driver validates its header and `fallback` covers a
                // stale or corrupt file.
                unsafe {
                    device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                        label: Some("crownpaper"),
                        data: cached_data.as_deref(),
                        fallback: true,
                    })
                }
            });
        let pipeline = Pipeline::new(&device, cache);
        if let (Some(path), Some(data)) = (cache_file, pipeline.cache().and_then(|c| c.get_data()))
            && cached_data.as_deref() != Some(&data)
        {
            persist(&path, &data);
        }

        let placeholder = GpuImage::placeholder(&device, &queue);
        log::info!(
            "renderer up on {} ({:?}) in {:.1} ms",
            adapter_info.name,
            adapter_info.device_type,
            started.elapsed().as_secs_f64() * 1e3
        );
        Ok(Self {
            device,
            queue,
            adapter,
            pipeline,
            placeholder,
            instance,
        })
    }
}

/// Makes the Vulkan loader load only the driver for the GPU the compositor
/// runs on. Otherwise it initializes every installed driver while
/// enumerating adapters — on a hybrid laptop that maps tens of MB of the
/// discrete GPU's driver and can wake that GPU.
///
/// # Safety
///
/// Mutates the environment, so no other thread may be running.
pub unsafe fn restrict_vulkan_driver(kernel_driver: &str) {
    const SELECT: &str = "VK_LOADER_DRIVERS_SELECT";
    let icd_glob = match kernel_driver {
        "amdgpu" | "radeon" => "*radeon*",
        "i915" | "xe" => "*intel*",
        "nouveau" => "*nouveau*",
        "nvidia" => "*nvidia*",
        _ => return,
    };
    if std::env::var_os(SELECT).is_none() {
        // SAFETY: forwarded to the caller.
        unsafe { std::env::set_var(SELECT, icd_glob) };
    }
}

fn pipeline_cache_file(adapter_info: &wgpu::AdapterInfo) -> Option<PathBuf> {
    let key = wgpu::util::pipeline_cache_key(adapter_info)?;
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(cache_home.join("crownpaper").join(key))
}

fn persist(path: &std::path::Path, data: &[u8]) {
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, data));
    if let Err(e) = written {
        log::warn!(
            "could not persist the pipeline cache to {}: {e}",
            path.display()
        );
    }
}
