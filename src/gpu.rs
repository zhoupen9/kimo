//! The wgpu side of Kimo: the window's surface, egui's renderer, and one frame.
//!
//! Kimo drives winit, egui-winit and egui-wgpu itself rather than going through
//! eframe, because the floating title bar's frosted backdrop needs the frame to
//! be built in two halves (see `crate::glass` for why wgpu cannot copy the
//! pixels behind the bar the way the glow build did):
//!
//! * a frame whose bar is docked is one render pass straight onto the surface;
//! * a frame whose bar floats draws everything *behind* the bar into an
//!   offscreen scene texture, blurs the bar's strip of it, and then draws the
//!   rest of the frame — the bar's fill, its icons, its text — onto the surface
//!   with the blurred strip and the scene under it.
//!
//! The split is egui's own shape list: the glass callback is the first shape
//! that belongs to the second half, so the two halves are rendered by two
//! calls into `egui_wgpu::Renderer`. That is also why there are two queue
//! submissions per frame: egui-wgpu uploads its vertices into one buffer per
//! `update_buffers` call, so the second half's upload must not be queued
//! before the first half has been submitted, or it would overwrite the vertices
//! the first half reads.

use std::sync::Arc;

use winit::window::Window;

use crate::app::KimoApp;

/// The window's own corner radius, in logical points. The window is a card: its
/// four corners are cut out of the surface so the compositor shows what is
/// behind them (see `CornerMask`). A compositor-side window rule of its own has
/// to agree with it — niri's `geometry-corner-radius`, for one — or the shadow
/// it draws will still assume a square window.
const WINDOW_CORNER_RADIUS: f32 = 8.0;

/// The window's GPU context: device, surface, egui's renderer, and the offscreen
/// scene a frame with a floating title bar is drawn into first.
pub struct Gpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: egui_wgpu::Renderer,
    /// The whole window, drawn without the title bar's own content. Made on the
    /// first floating frame and kept until the window is resized.
    scene: Option<Scene>,
    /// Draws the scene onto the surface, one to one.
    composite: Composite,
    /// The last draw of a frame: the window's rounded corners.
    corners: CornerMask,
}

/// The offscreen frame: the texture, and the bind group that samples it.
struct Scene {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// The pipeline that copies the scene onto the surface. It needs no uniforms:
/// the whole point is that the two are the same size.
struct Composite {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

/// Cuts the window's rounded corners out of the frame.
///
/// The last draw of a frame paints the four corner squares of the surface with
/// the corner's *coverage* as alpha, and a blend that keeps nothing of the
/// source colour but scales the destination by `1 - src.a` — for the alpha
/// channel as well as the colour, which is what keeps the result premultiplied.
/// A pixel outside the radius is therefore scaled to zero and the compositor
/// shows what is behind the window there; the rim, a pixel wide, fades out
/// instead of stair-stepping.
struct CornerMask {
    pipeline: wgpu::RenderPipeline,
    /// `size.x, size.y, radius`, in physical pixels (see `mask_bytes`).
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Whether the surface can carry the transparency this needs. Without an
    /// alpha mode the corners stay square rather than turn black.
    enabled: bool,
}

impl Gpu {
    /// The device, the surface and everything the frame needs, for a window that
    /// has just been created.
    pub fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Self {
        // The display handle matters on the GL backend, which needs it to
        // present on Wayland at all.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        ));
        let surface = instance
            .create_surface(window.clone())
            .expect("kimo: the window has no surface this GPU can draw into");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("kimo: no GPU adapter (is a driver or a software rasterizer installed?)");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("kimo"),
            ..Default::default()
        }))
        .expect("kimo: the GPU refused a device");

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("kimo: the surface does not support this GPU");
        // Vsynced, and never multisampled: egui does its own antialiasing, and
        // the glass reads the frame back as a texture, which a multisampled
        // attachment would not allow.
        config.present_mode = wgpu::PresentMode::Fifo;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        // The corner mask cuts pixels out of the frame, so the surface has to
        // carry their alpha to the compositor: `Auto` resolves to `Opaque`,
        // which would leave the cut corners black. Premultiplied is also the
        // space egui-wgpu's own blending is in. A surface with no alpha mode at
        // all keeps its square corners instead of growing black ones.
        let alpha = surface
            .get_capabilities(&adapter)
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        if alpha {
            config.alpha_mode = wgpu::CompositeAlphaMode::PreMultiplied;
        } else {
            eprintln!(
                "kimo: this surface has no premultiplied alpha mode; the window keeps square corners"
            );
        }
        surface.configure(&device, &config);

        let renderer = egui_wgpu::Renderer::new(
            &device,
            config.format,
            egui_wgpu::RendererOptions::default(),
        );
        let composite = Composite::new(&device, config.format);
        let corners = CornerMask::new(&device, config.format, alpha);

        Self {
            surface,
            config,
            device,
            queue,
            renderer,
            scene: None,
            composite,
            corners,
        }
    }

    /// A new window size: the surface, and everything sized to it, is remade.
    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.scene = None;
    }

    /// One frame: egui runs the app, and what it drew is rendered — in one pass
    /// or two, depending on whether the title bar is floating.
    ///
    /// The frames commands back are egui's output for other viewports, which the
    /// caller hands to `egui_winit::process_viewport_commands`.
    pub fn render(
        &mut self,
        window: &Arc<Window>,
        state: &mut egui_winit::State,
        app: &mut KimoApp,
        event_loop: &winit::event_loop::ActiveEventLoop,
    ) -> egui::OrderedViewportIdMap<egui::ViewportOutput> {
        let egui_ctx = state.egui_ctx().clone();
        app.glass().begin_frame();

        let raw_input = state.take_egui_input(window);
        let full_output = egui_ctx.run_ui(raw_input, |ui| app.ui(ui));
        let egui::FullOutput {
            platform_output,
            mut textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        for (id, deltas) in &textures_delta.set {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        // epaint panics on a `TexturesDelta` that is dropped without having been
        // applied, so the uploads are taken off it as they are done; what is
        // left are the frees, which happen once the frame is on its way.
        textures_delta.set.clear();
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        let size = [self.config.width, self.config.height];
        // The mask's corner radius is a UI measurement, so it follows the scale
        // factor; both it and the size are physical pixels in the shader.
        self.corners
            .set(&self.queue, size, corner_radius(size, pixels_per_point));
        // The scheme is installed by the app's frame, so the clear color — what
        // shows in the moment before the panels cover the window — is read here.
        let clear = clear_color(crate::theme::of(&egui_ctx));
        // egui hands over shapes; triangles are what the renderer takes, and
        // tessellating here (rather than letting the renderer do it) is what
        // lets the frame be split around the glass callback.
        let primitives = egui_ctx.tessellate(shapes, pixels_per_point);

        let glass = app.glass().rect_px(pixels_per_point, size);
        let split = app.glass().split(&primitives);
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            // Usable, but the swapchain wants reconfiguring: the next frame's
            // `Resized` (or this one's) does it.
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            other => {
                eprintln!("kimo: cannot draw this frame: {other:?}");
                self.free_textures(&mut textures_delta);
                return viewport_output;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        match (glass, split) {
            (Some(rect), Some(split)) if split > 0 && split < primitives.len() => {
                app.glass().ensure(
                    &self.device,
                    &self.queue,
                    self.config.format,
                    [rect[2], rect[3]],
                );
                self.ensure_scene(size);
                let (under, over) = primitives.split_at(split);

                // First half: everything behind the bar.
                let mut encoder = self.encoder("kimo: behind the title bar");
                let mut callbacks = self.renderer.update_buffers(
                    &self.device,
                    &self.queue,
                    &mut encoder,
                    under,
                    &descriptor,
                );
                self.scene_pass(&mut encoder, under, &descriptor, clear);
                if let Some(scene) = self.scene.as_ref() {
                    app.glass().blur(&mut encoder, &scene.texture, rect);
                }
                callbacks.push(encoder.finish());
                self.queue.submit(callbacks);

                // Second half: the scene, the glass, and the bar itself.
                let mut encoder = self.encoder("kimo: the title bar");
                let callbacks = self.renderer.update_buffers(
                    &self.device,
                    &self.queue,
                    &mut encoder,
                    over,
                    &descriptor,
                );
                {
                    // One pass, two draws: the scene under everything, then
                    // the half of the frame that belongs over the glass — the
                    // bar's fill, its icons, its text.
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("kimo"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                depth_slice: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(clear),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    self.composite.draw(&mut pass, self.scene.as_ref());
                    self.renderer.render(&mut pass, over, &descriptor);
                    // Last, over everything: the window's own corners.
                    self.corners.round(&mut pass);
                    drop(pass);
                }
                self.queue.submit(
                    callbacks
                        .into_iter()
                        .chain(std::iter::once(encoder.finish())),
                );
            }
            _ => {
                // No glass this frame: the whole frame, straight onto the window.
                let mut encoder = self.encoder("kimo");
                let callbacks = self.renderer.update_buffers(
                    &self.device,
                    &self.queue,
                    &mut encoder,
                    &primitives,
                    &descriptor,
                );
                {
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("kimo"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                depth_slice: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(clear),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    self.renderer.render(&mut pass, &primitives, &descriptor);
                    // Last, over everything: the window's own corners.
                    self.corners.round(&mut pass);
                }
                self.queue.submit(
                    callbacks
                        .into_iter()
                        .chain(std::iter::once(encoder.finish())),
                );
            }
        }

        self.free_textures(&mut textures_delta);
        state.handle_platform_output_with_event_loop(window, event_loop, platform_output);
        window.pre_present_notify();
        self.queue.present(frame);
        viewport_output
    }

    /// Hand back the textures egui is done with. Nothing may be left in the
    /// delta when it drops.
    fn free_textures(&mut self, delta: &mut egui::epaint::textures::TexturesDelta) {
        for id in &delta.free {
            self.renderer.free_texture(id);
        }
        delta.free.clear();
    }

    fn encoder(&self, label: &str) -> wgpu::CommandEncoder {
        self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) })
    }

    /// Everything behind the title bar, into the scene texture. The glass's
    /// copy and blur are recorded into the same encoder, so this one is
    /// finished by the caller.
    fn scene_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        shapes: &[egui::ClippedPrimitive],
        descriptor: &egui_wgpu::ScreenDescriptor,
        clear: wgpu::Color,
    ) {
        let scene = self.scene.as_ref().expect("the scene was just made");
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kimo: behind the title bar"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene.view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.renderer
                .render(&mut pass.forget_lifetime(), shapes, descriptor);
        }
    }

    /// Make the scene texture for this window size, if it is not already made.
    fn ensure_scene(&mut self, size: [u32; 2]) {
        if self.scene.as_ref().is_some_and(|scene| {
            scene.texture.width() == size[0] && scene.texture.height() == size[1]
        }) {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kimo_scene"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.composite.bind_group(&self.device, &view);
        self.scene = Some(Scene {
            texture,
            view,
            bind_group,
        });
    }
}

impl Composite {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kimo_composite"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kimo_composite"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kimo_composite"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kimo_composite"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // The scene is opaque: the windows it is copied onto are.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kimo_composite"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
        }
    }

    fn bind_group(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kimo_composite"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'static>, scene: Option<&Scene>) {
        let Some(scene) = scene else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &scene.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl CornerMask {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, enabled: bool) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kimo_corners"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kimo_corners"),
            source: wgpu::ShaderSource::Wgsl(MASK_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kimo_corners"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kimo_corners"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Not a colour but a coverage: what the frame already holds
                    // is scaled by what the coverage leaves of it.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kimo_corners"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kimo_corners"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform,
            bind_group,
            enabled,
        }
    }

    /// This frame's surface size and corner radius, both in physical pixels.
    fn set(&self, queue: &wgpu::Queue, size: [u32; 2], radius: f32) {
        if self.enabled {
            queue.write_buffer(&self.uniform, 0, &mask_bytes(size, radius));
        }
    }

    /// Draw the four corners over whatever the frame has drawn so far.
    fn round(&self, pass: &mut wgpu::RenderPass<'static>) {
        if !self.enabled {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        // Four corner quads, two triangles each.
        pass.draw(0..24, 0..1);
    }
}

/// The window's corner radius in physical pixels: [`WINDOW_CORNER_RADIUS`] in
/// points, and never more than half the window, which would fold the corner
/// quads inside out.
fn corner_radius(size: [u32; 2], pixels_per_point: f32) -> f32 {
    let half = (size[0] as f32 / 2.0).min(size[1] as f32 / 2.0);
    (WINDOW_CORNER_RADIUS * pixels_per_point)
        .round()
        .clamp(0.0, half)
}

/// The mask's uniform: the surface size and the corner radius, both in physical
/// pixels, then the padding the uniform's `vec2` alignment asks for.
fn mask_bytes(size: [u32; 2], radius: f32) -> Vec<u8> {
    [size[0] as f32, size[1] as f32, radius, 0.0]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect()
}

/// The window's background, which egui's panels cover but a resize can show for
/// a frame.
fn clear_color(scheme: crate::theme::ColorScheme) -> wgpu::Color {
    let [r, g, b, a] = scheme.window.to_normalized_gamma_f32();
    wgpu::Color {
        r: r as f64,
        g: g as f64,
        b: b as f64,
        a: a as f64,
    }
}

/// Copies the scene texture onto the window, texel for texel.
const SHADER: &str = r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VertexOut;
    out.position = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(source, source_sampler, in.uv);
}
"#;

/// Draws the four corner squares of the window, each carrying the coverage of
/// the rounded corner that falls in it: 0 where the frame stays, 1 where it is
/// cut away, and the ramp between the two.
const MASK_SHADER: &str = r#"
struct Mask {
    // The surface, in physical pixels.
    size: vec2<f32>,
    // The corner radius, in physical pixels.
    radius: f32,
    _padding: f32,
};

@group(0) @binding(0) var<uniform> mask: Mask;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    // The centre of this corner's circle, in physical pixels.
    @location(0) centre: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    // Four quads of six vertices: `corner` picks one (bit 0 is the right edge,
    // bit 1 the bottom one) and `vertex` the point inside it.
    let corner = index / 6u;
    let vertex = index % 6u;
    let right = (corner & 1u) == 1u;
    let bottom = (corner & 2u) == 2u;
    // The two triangles of a unit quad: (0,0) (1,0) (0,1) and (1,0) (1,1) (0,1).
    let u = select(0.0, 1.0, vertex == 1u || vertex == 3u || vertex == 4u);
    let v = select(0.0, 1.0, vertex == 2u || vertex == 4u || vertex == 5u);

    let radius = mask.radius;
    let centre = vec2<f32>(
        select(radius, mask.size.x - radius, right),
        select(radius, mask.size.y - radius, bottom),
    );
    let origin = vec2<f32>(
        select(0.0, mask.size.x - radius, right),
        select(0.0, mask.size.y - radius, bottom),
    );
    let pixel = origin + vec2<f32>(u, v) * radius;

    var out: VertexOut;
    out.position = vec4<f32>(
        pixel.x / mask.size.x * 2.0 - 1.0,
        1.0 - pixel.y / mask.size.y * 2.0,
        0.0,
        1.0,
    );
    out.centre = centre;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    // `position` is the pixel's centre in framebuffer coordinates, which is the
    // space the radius is in.
    let d = distance(in.position.xy, in.centre);
    // Nothing inside the corner, everything outside it, and a pixel of ramp
    // between the two, which the blend turns into the antialiased rim.
    let cut = smoothstep(mask.radius - 0.5, mask.radius + 0.5, d);
    return vec4<f32>(0.0, 0.0, 0.0, cut);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The uniform as the shader sees it: size.x, size.y, radius.
    fn uniform(size: [u32; 2], radius: f32) -> [f32; 4] {
        let bytes = mask_bytes(size, radius);
        let word = |index: usize| {
            let mut four = [0u8; 4];
            four.copy_from_slice(&bytes[index * 4..index * 4 + 4]);
            f32::from_le_bytes(four)
        };
        [word(0), word(1), word(2), word(3)]
    }

    #[test]
    fn the_mask_uniform_is_the_size_and_the_radius() {
        assert_eq!(mask_bytes([1920, 1080], 32.0).len(), 16);
        assert_eq!(uniform([1920, 1080], 32.0), [1920.0, 1080.0, 32.0, 0.0]);
    }

    #[test]
    fn the_corner_radius_follows_the_scale_factor_and_the_window() {
        assert_eq!(corner_radius([1920, 1080], 1.0), 8.0);
        assert_eq!(corner_radius([1920, 1080], 2.0), 16.0);
        // Never more than half the window: the quads would fold inside out.
        assert_eq!(corner_radius([20, 1080], 2.0), 10.0);
        assert_eq!(corner_radius([0, 0], 1.0), 0.0);
    }
}
