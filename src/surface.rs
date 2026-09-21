//! Per-output wallpaper surface.
//!
//! Each output gets one background layer surface whose handler renders the
//! shared scene: the surface owns only its uniform buffer and bind group,
//! while textures, pipeline and animation clocks live in the shared state so
//! every output shows the same wallpaper frame-locked to the same timeline.

use std::cell::RefCell;
use std::rc::Rc;

use crownshell::wgpu;
use crownshell::{RawSurfaceCtx, RawSurfaceHandler};

use crate::daemon::state::State;
use crate::render::{Uniforms, MAX_BLUR};

pub struct WallpaperSurface {
    state: Rc<RefCell<State>>,
    uniforms: Option<wgpu::Buffer>,
    bind_group: Option<wgpu::BindGroup>,
    bound_generation: u64,
    seen_paint_serial: u64,
}

impl WallpaperSurface {
    pub fn new(state: Rc<RefCell<State>>) -> Self {
        Self {
            state,
            uniforms: None,
            bind_group: None,
            bound_generation: 0,
            seen_paint_serial: 0,
        }
    }
}

impl RawSurfaceHandler for WallpaperSurface {
    fn setup(&mut self, ctx: RawSurfaceCtx<'_>) {
        let mut state = self.state.borrow_mut();
        state.ensure_gpu(ctx.device, ctx.queue);
        let gpu = state.gpu.as_ref().expect("ensure_gpu just ran");
        self.uniforms = Some(gpu.create_uniform_buffer());
        log::info!(
            "surface ready: {}x{} px, format {:?}",
            ctx.size.0,
            ctx.size.1,
            ctx.format
        );
    }

    fn render(
        &mut self,
        _surface_texture: &wgpu::SurfaceTexture,
        view: &wgpu::TextureView,
        ctx: RawSurfaceCtx<'_>,
    ) {
        let state = &mut *self.state.borrow_mut();
        self.seen_paint_serial = state.paint_serial;
        let (Some(gpu), Some(uniforms)) = (state.gpu.as_mut(), self.uniforms.as_ref()) else {
            return;
        };

        // Advancing the clocks may retire a finished transition, which drops
        // the outgoing layer and bumps the generation — read sizes after.
        let frame = state.scene.frame();
        let from = state
            .scene
            .from
            .as_ref()
            .map_or(&gpu.placeholder, |l| &l.image);
        let to = state
            .scene
            .to
            .as_ref()
            .map_or(&gpu.placeholder, |l| &l.image);

        if self.bind_group.is_none() || self.bound_generation != state.scene.generation {
            self.bind_group = Some(gpu.create_bind_group(uniforms, from, to));
            self.bound_generation = state.scene.generation;
        }

        ctx.queue.write_buffer(
            uniforms,
            0,
            bytemuck::bytes_of(&Uniforms {
                resolution: [ctx.size.0 as f32, ctx.size.1 as f32],
                progress: frame.progress,
                time: frame.time,
                tex_a_size: from.size,
                tex_b_size: to.size,
                intro_scale: frame.intro_scale,
                max_blur: MAX_BLUR,
                _pad: [0.0; 2],
            }),
        );

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("wallpaper"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wallpaper"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(gpu.pipeline(ctx.format));
            pass.set_bind_group(0, self.bind_group.as_ref(), &[]);
            pass.draw(0..3, 0..1);
        }
        ctx.queue.submit([encoder.finish()]);
    }

    fn on_frame(&mut self, _ctx: RawSurfaceCtx<'_>) -> bool {
        self.state.borrow().scene.is_animating()
    }

    fn needs_redraw(&self) -> bool {
        self.state.borrow().paint_serial != self.seen_paint_serial
    }
}
