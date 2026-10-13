//! A frosted-glass backdrop for the floating title bar, on wgpu.
//!
//! egui cannot blur: nothing in the painter can sample what has already been
//! drawn, and there is no blur primitive. The *renderer* can — so the backdrop
//! is a small pass of our own: the pixels behind the bar are copied into a
//! texture, blurred, and drawn back before the bar paints its translucent fill
//! over them, which is what turns the mail scrolling under the bar into a
//! smudge of its colors instead of unreadable text.
//!
//! wgpu cannot do what the glow build did, though. There the copy was
//! `glCopyTexSubImage2D` *inside* the pass that draws the bar: OpenGL will read
//! the framebuffer it is currently drawing into. wgpu will not — a texture
//! cannot be sampled while it is a pass's attachment, and the attachment here
//! is the window's surface, which is not even created with `COPY_SRC`. So the
//! frame is split around this pass instead (see `crate::gpu`): everything
//! behind the bar is drawn into an offscreen *scene* texture, this module
//! copies the bar's strip of it (a plain texture-to-texture copy, the one
//! thing wgpu does allow), blurs it in two separable passes, and the rest of
//! the frame — the bar's fill, its icons, its text — is drawn onto the window
//! with the blurred strip under it. The callback is the seam between the two
//! halves of the frame and, in the second half, the thing that draws the strip.
//!
//! The blur is separable — a horizontal pass into an offscreen texture of the
//! bar's size, then a vertical one — because a single two-dimensional kernel
//! cheap enough to run per frame is sampled too sparsely to hide the detail it
//! is meant to destroy: nine taps four pixels apart left a visible comb over
//! the text. Seventeen taps a pixel and a half apart, twice, cost less and
//! leave nothing readable. Both passes draw one full-viewport triangle (the
//! strip *is* the viewport), and the same pipeline draws the result: with a
//! step of zero the blur is a plain copy.
//!
//! Every failure here is soft. A device that will not take the pipeline or the
//! textures — for want of a format, say — reports once, sets `failed`, and
//! leaves the bar translucent over the crisp content instead of frosted.

use std::sync::{Arc, Mutex};

use egui_wgpu::CallbackTrait;

/// The width of one blur step, in physical pixels: the shader takes its taps
/// this far apart (see [`SHADER`], which carries the tap count and the loop
/// that reads them). Sampling inside the pixel grid is what keeps text from
/// aliasing into a pattern instead of dissolving.
const STEP_PX: f32 = 2.0;

/// The glass pass, shared with the paint callback it hands to egui. Cloning it
/// clones a handle: the GPU objects are made once, on the first floating frame.
#[derive(Clone, Default)]
pub struct Glass {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    /// The bar's rect this frame, in points, recorded by `Glass::backdrop`.
    /// It is what tells the integration where the frame splits and what to
    /// blur; `None` means the bar is docked and the frame needs none of it.
    rect: Option<egui::Rect>,
    /// The GPU objects, made on the first frame that needs them.
    gpu: Option<Gpu>,
    /// Set for good once the device has refused something: the bar is then only
    /// translucent, and nothing here runs again.
    failed: bool,
}

impl Glass {
    /// Blur whatever has already been drawn behind `rect`.
    ///
    /// This belongs *before* the fill and the glyphs that sit on the glass,
    /// which is also where the frame is split: everything added before it is
    /// drawn into the scene texture the blur reads, everything after it onto
    /// the window, over the blurred strip.
    pub fn backdrop(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !rect.is_positive() {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.rect = Some(rect);
        }
        let callback = GlassCallback {
            shared: self.shared.clone(),
        };
        painter.add(egui::Shape::Callback(
            egui_wgpu::Callback::new_paint_callback(rect, callback),
        ));
    }

    /// Forget last frame's bar: `backdrop` records this frame's.
    pub(crate) fn begin_frame(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.rect = None;
        }
    }

    /// The bar's rect in physical pixels, clamped to the window, or `None`
    /// when the bar is docked (or the pass has failed).
    pub(crate) fn rect_px(&self, pixels_per_point: f32, size: [u32; 2]) -> Option<[u32; 4]> {
        let shared = self.shared.lock().ok()?;
        if shared.failed {
            return None;
        }
        let rect = shared.rect?;
        let x = (rect.min.x * pixels_per_point)
            .round()
            .clamp(0.0, size[0] as f32) as u32;
        let y = (rect.min.y * pixels_per_point)
            .round()
            .clamp(0.0, size[1] as f32) as u32;
        let width = (rect.width() * pixels_per_point)
            .round()
            .clamp(0.0, (size[0] - x) as f32) as u32;
        let height = (rect.height() * pixels_per_point)
            .round()
            .clamp(0.0, (size[1] - y) as f32) as u32;
        (width > 0 && height > 0).then_some([x, y, width, height])
    }

    /// Where the frame splits: the index of the glass callback in the frame's
    /// shapes, which is the first shape that must be drawn *over* the blurred
    /// strip rather than into the scene behind it.
    pub(crate) fn split(&self, shapes: &[egui::ClippedPrimitive]) -> Option<usize> {
        let shared = self.shared.lock().ok()?;
        if shared.failed {
            return None;
        }
        let rect = shared.rect?;
        shapes.iter().position(|shape| match &shape.primitive {
            egui::epaint::Primitive::Callback(callback) => {
                // The same rect, to within the rounding layout does to it.
                (callback.rect.min - rect.min).length() < 0.5
                    && (callback.rect.size() - rect.size()).length() < 0.5
            }
            _ => false,
        })
    }

    /// Make the pass's textures and pipeline for a strip of this size, if they
    /// are not already made for it.
    pub(crate) fn ensure(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: [u32; 2],
    ) {
        if size[0] == 0 || size[1] == 0 {
            return;
        }
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        if shared.failed
            || shared
                .gpu
                .as_ref()
                .is_some_and(|gpu| gpu.fits(format, size))
        {
            return;
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let gpu = Gpu::new(device, queue, format, size);
        let error = pollster::block_on(scope.pop());
        match (gpu, error) {
            (Some(gpu), None) => shared.gpu = Some(gpu),
            (_, Some(error)) => {
                shared.failed = true;
                eprintln!("kimo: the title bar will not be frosted: {error}");
            }
            (None, None) => {}
        }
    }

    /// Copy the bar's strip out of `scene` and blur it, into the texture the
    /// callback draws. This runs between the frame's two halves, so the scene
    /// holds everything *behind* the bar and nothing of the bar itself.
    pub(crate) fn blur(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &wgpu::Texture,
        rect: [u32; 4],
    ) {
        let Ok(shared) = self.shared.lock() else {
            return;
        };
        let Some(gpu) = shared.gpu.as_ref() else {
            return;
        };
        if shared.failed {
            return;
        }
        let [x, y, width, height] = rect;
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: scene,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &gpu.strip.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        // One pass per axis: the bind group carries that axis's step, and each
        // pass draws the one triangle over its whole viewport.
        gpu.blur_pass(encoder, &gpu.first.view, &gpu.strip.bind_group);
        gpu.blur_pass(encoder, &gpu.second.view, &gpu.first.bind_group);
    }
}

/// The GPU objects of the pass, made on the first floating frame and kept until
/// the window — or the bar — changes size.
struct Gpu {
    /// One pipeline for all three draws: the two blur axes, and the copy the
    /// callback does with a step of zero.
    pipeline: wgpu::RenderPipeline,
    /// The pixels behind the bar, copied out of the scene.
    strip: Target,
    /// The horizontal pass's result.
    first: Target,
    /// The vertical pass's result, which the callback draws.
    second: Target,
    format: wgpu::TextureFormat,
    size: [u32; 2],
}

impl Gpu {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: [u32; 2],
    ) -> Option<Self> {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kimo_glass"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kimo_glass"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kimo_glass"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kimo_glass"),
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
            label: Some("kimo_glass"),
            // Clamped: the strip's edge samples its own edge, not the far side.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // The step of each pass, which is what makes one shader do all three:
        // a pixel and a half either side per axis, none at all for the copy.
        // Written once — they only change when the bar is resized.
        let step = |x: f32, y: f32| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("kimo_glass_step"),
                size: 8,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&buffer, 0, &floats(x, y));
            buffer
        };
        let (width, height) = (size[0] as f32, size[1] as f32);
        let steps = [
            step(STEP_PX / width, 0.0),
            step(0.0, STEP_PX / height),
            step(0.0, 0.0),
        ];

        let mut textures = Vec::new();
        for label in ["kimo_glass_strip", "kimo_glass_first", "kimo_glass_second"] {
            textures.push(device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
        }
        let mut target = |index: usize| {
            let texture = textures.remove(0);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kimo_glass"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: steps[index].as_entire_binding(),
                    },
                ],
            });
            Target {
                texture,
                view,
                bind_group,
            }
        };
        let (strip, first, second) = (target(0), target(1), target(2));

        Some(Self {
            pipeline,
            strip,
            first,
            second,
            format,
            size,
        })
    }

    fn fits(&self, format: wgpu::TextureFormat, size: [u32; 2]) -> bool {
        self.format == format && self.size == size
    }

    /// One axis of the blur, or the copy: a triangle over the whole target,
    /// sampling `sources` through its own bind group.
    fn blur_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        source: &wgpu::BindGroup,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("kimo_glass_blur"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, source, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The one texture a pass reads from and writes to.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// What the bar adds to the frame: in the second half it draws the blurred
/// strip, into the viewport egui has already set to the bar.
struct GlassCallback {
    shared: Arc<Mutex<Shared>>,
}

impl CallbackTrait for GlassCallback {
    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        _resources: &egui_wgpu::CallbackResources,
    ) {
        let Ok(shared) = self.shared.lock() else {
            return;
        };
        let Some(gpu) = shared.gpu.as_ref() else {
            return;
        };
        if shared.failed {
            return;
        }
        pass.set_pipeline(&gpu.pipeline);
        pass.set_bind_group(0, &gpu.second.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The pass's shader. One triangle covers the viewport — the strip (or the bar)
/// *is* the viewport, so there is no projection to set up, only the uv of the
/// corner that says which way the texture reads.
const SHADER: &str = r#"
struct Uniforms {
    step: vec2<f32>,
};

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

const TAPS: i32 = 8;
const SAMPLES: f32 = 17.0;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    // (0,0), (2,0), (0,2): one triangle over the whole clip square, whose uv
    // runs 0..1 with v = 0 at the top, as the texture does.
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VertexOut;
    out.position = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    var sum = vec4<f32>(0.0);
    for (var i: i32 = -TAPS; i <= TAPS; i = i + 1) {
        sum = sum + textureSample(source, source_sampler, in.uv + vec2<f32>(f32(i), 0.0) * uniforms.step);
    }
    return sum / SAMPLES;
}
"#;

/// Two little-endian floats for a uniform buffer. (wgpu's own bytemuck is not
/// a dependency of this crate.)
fn floats(x: f32, y: f32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&x.to_le_bytes());
    bytes[4..8].copy_from_slice(&y.to_le_bytes());
    bytes
}
