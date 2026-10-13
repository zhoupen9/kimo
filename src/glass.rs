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
//! glCopyTexSubImage2D *inside* the pass that draws the bar: OpenGL will read
//! the framebuffer it is currently drawing into. wgpu will not — a texture
//! cannot be sampled while it is a pass's attachment, and the attachment here
//! is the window's surface, which is not even created with COPY_SRC. So the
//! frame is split around this pass instead (see crate::gpu): everything behind
//! the bar is drawn into an offscreen *scene* texture, this module copies the
//! bar's strip of it (a plain texture-to-texture copy, the one thing wgpu does
//! allow), blurs it in two separable passes, and the rest of the frame — the
//! bar's fill, its icons, its text — is drawn onto the window with the blurred
//! strip under it. The callback is the seam between the two halves of the frame
//! and, in the second half, the thing that draws the strip.
//!
//! The blur is a **Gaussian**, one axis per pass: a horizontal pass into an
//! offscreen texture of the bar's size, then a vertical one. SIGMA_PX is the
//! standard deviation, in physical pixels, and the kernel stops a little short
//! of three of them — past that a Gaussian contributes nothing the eight-bit
//! strip could show. It is normalised, so the weights always sum to one and the
//! frost never brightens or darkens what is behind it. Each axis walks TAPS
//! *pairs* of taps either side of the fragment plus a centre one, and each pair
//! is placed half a texel past an even boundary, between two texels, exactly
//! where the sampler's linear filter averages them: one fetch stands in for two
//! taps, so a 17-tap Gaussian costs 9 fetches rather than 17. (Its precursor was
//! a box: 17 taps two pixels apart, which skipped every other texel and left a
//! faint comb over the text.)
//!
//! Both passes draw one full-viewport triangle (the strip *is* the viewport),
//! and the callback's half of the frame copies the result texel for texel.
//!
//! Every failure here is soft. A device that will not take the pipeline or the
//! textures — for want of a format, say — reports once, sets failed, and leaves
//! the bar translucent over the crisp content instead of frosted.

use std::sync::{Arc, Mutex};

use egui_wgpu::CallbackTrait;

/// The Gaussian's standard deviation, in physical pixels: what "how frosted"
/// means. Raise it for a heavier smear; TAPS has to come with it.
const SIGMA_PX: f32 = 6.0;

/// Tap pairs either side of the fragment: eight reaches 16 px, which is a shade
/// under three standard deviations at the sigma above — far enough out that
/// what is left over is below what the eight-bit strip could show.
const TAPS: usize = 8;

/// The glass pass, shared with the paint callback it hands to egui. Cloning it
/// clones a handle: the GPU objects are made once, on the first floating frame.
#[derive(Clone, Default)]
pub struct Glass {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    /// The bar's rect this frame, in points, recorded by Glass::backdrop. It is
    /// what tells the integration where the frame splits and what to blur;
    /// None means the bar is docked and the frame needs none of it.
    rect: Option<egui::Rect>,
    /// The GPU objects, made on the first frame that needs them.
    gpu: Option<Gpu>,
    /// Set for good once the device has refused something: the bar is then only
    /// translucent, and nothing here runs again.
    failed: bool,
}

impl Glass {
    /// Blur whatever has already been drawn behind rect.
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

    /// Forget last frame's bar: backdrop records this frame's.
    pub(crate) fn begin_frame(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.rect = None;
        }
    }

    /// The bar's rect in physical pixels, clamped to the window, or None when
    /// the bar is docked (or the pass has failed).
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

    /// Copy the bar's strip out of the scene and blur it, into the texture the
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
        // One axis per pass: the bind group carries that axis's kernel — its
        // offsets along the axis, in uv — and each pass draws the one triangle
        // over its whole viewport.
        gpu.blur_pass(encoder, &gpu.first.view, &gpu.strip.bind_group);
        gpu.blur_pass(encoder, &gpu.second.view, &gpu.first.bind_group);
    }
}

/// The GPU objects of the pass, made on the first floating frame and kept until
/// the window — or the bar — changes size, which changes the kernels with it.
struct Gpu {
    /// The Gaussian, one axis per pass.
    blur: wgpu::RenderPipeline,
    /// The callback's draw: the finished strip, texel for texel.
    copy: wgpu::RenderPipeline,
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
            source: wgpu::ShaderSource::Wgsl(shader().into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kimo_glass"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    entry_point: Some(entry),
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
            })
        };
        let blur = pipeline("fs_blur");
        let copy = pipeline("fs_copy");

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kimo_glass"),
            // Clamped: a strip that runs out of texels at its own edge samples
            // that edge rather than the far side of the bar.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // The kernels, written once: their uv offsets depend only on the
        // strip's size, which is one of the things that makes the pass be made
        // again at all.
        let kernel = |extent: u32, horizontal: bool| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("kimo_glass_kernel"),
                size: (4 * (TAPS + 1) * 4) as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&buffer, 0, &kernel_bytes(extent as f32, horizontal));
            buffer
        };
        let kernels = [kernel(size[0], true), kernel(size[1], false)];

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
        let mut target = |kernel: usize| {
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
                        resource: kernels[kernel].as_entire_binding(),
                    },
                ],
            });
            Target {
                texture,
                view,
                bind_group,
            }
        };
        // The strip is blurred horizontally, that result vertically, and the
        // second result is what the callback copies — its kernel goes unused.
        let (strip, first, second) = (target(0), target(1), target(1));

        Some(Self {
            blur,
            copy,
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

    /// One axis of the blur: a triangle over the whole target, sampling the
    /// source texture through its own bind group and that axis's kernel.
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
        pass.set_pipeline(&self.blur);
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
        pass.set_pipeline(&gpu.copy);
        pass.set_bind_group(0, &gpu.second.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The pass's shader, with the tap count baked in so the two cannot drift: one
/// triangle covers the viewport — the strip *is* the viewport, so there is no
/// projection to set up — and the fragment walks the kernel's pairs.
fn shader() -> String {
    let taps = TAPS;
    format!(
        r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct Kernel {{
    // One entry per tap pair: the offset along the axis being blurred, in uv,
    // and the weight of the pair. Written by kernel_bytes.
    pairs: array<vec4<f32>, {taps}>,
    centre: vec4<f32>,
}};

@group(0) @binding(2) var<uniform> kernel: Kernel;

struct VertexOut {{
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {{
    // (0,0), (2,0), (0,2): one triangle over the whole clip square, whose uv
    // runs 0..1 with v = 0 at the top, as the texture does.
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VertexOut;
    out.position = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}}

// The Gaussian itself: the centre tap, then each pair either side of it. A
// pair's offset sits between two texels, so the sampler's linear filter
// averages them and one fetch covers two taps.
@fragment
fn fs_blur(in: VertexOut) -> @location(0) vec4<f32> {{
    var sum = kernel.centre.x * textureSample(source, source_sampler, in.uv);
    for (var i = 0; i < {taps}; i = i + 1) {{
        let pair = kernel.pairs[i];
        sum = sum
            + pair.z * (textureSample(source, source_sampler, in.uv + pair.xy)
                      + textureSample(source, source_sampler, in.uv - pair.xy));
    }}
    return sum;
}}

// Texel for texel, for the half of the frame that draws the bar.
@fragment
fn fs_copy(in: VertexOut) -> @location(0) vec4<f32> {{
    return textureSample(source, source_sampler, in.uv);
}}
"#
    )
}

/// One weight of the Gaussian, before normalisation.
fn weight(distance_px: f32) -> f32 {
    (-(distance_px * distance_px) / (2.0 * SIGMA_PX * SIGMA_PX)).exp()
}

/// One axis's kernel, as the uniform buffer wants it: TAPS pairs of
/// (offset.x, offset.y, weight, 0) — the offset along the axis being blurred,
/// in uv — then the centre tap's weight.
///
/// The pairs are the Gaussian's taps folded two at a time: the taps 1 and 2 px
/// out share one fetch, placed at 1.5 px, because the sampler averages them
/// there, and the pair's weight is the two weights added up. The whole set is
/// then divided by its sum, so blurring a flat colour returns that colour.
fn kernel_bytes(extent_px: f32, horizontal: bool) -> Vec<u8> {
    let mut pairs = Vec::with_capacity(TAPS);
    let mut total = weight(0.0);
    for tap in 1..=TAPS {
        let near = (2 * tap - 1) as f32;
        let far = (2 * tap) as f32;
        let pair = weight(near) + weight(far);
        // Half a texel past the even boundary: between the two taps' texels.
        let offset = (near + far) / 2.0 / extent_px;
        pairs.push((offset, pair));
        total += 2.0 * pair;
    }

    let mut words = Vec::with_capacity(4 * (TAPS + 1));
    for (offset, pair) in pairs {
        let x = if horizontal { offset } else { 0.0 };
        let y = if horizontal { 0.0 } else { offset };
        words.extend_from_slice(&[x, y, pair / total, 0.0]);
    }
    words.extend_from_slice(&[weight(0.0) / total, 0.0, 0.0, 0.0]);
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel as Rust sees it: each pair's offset in pixels and its
    /// normalised weight, plus the centre tap's weight.
    fn taps(extent_px: f32) -> (Vec<(f32, f32)>, f32) {
        let bytes = kernel_bytes(extent_px, true);
        let word = |entry: &[u8], index: usize| {
            f32::from_le_bytes(entry[index * 4..index * 4 + 4].try_into().unwrap())
        };
        let pairs = bytes
            .as_chunks::<16>()
            .0
            .iter()
            .take(TAPS)
            .map(|entry| (word(entry, 0) * extent_px, word(entry, 2)))
            .collect();
        let centre = word(&bytes[16 * TAPS..], 0);
        (pairs, centre)
    }

    #[test]
    fn the_gaussian_adds_up_to_one_and_falls_off() {
        let (pairs, centre) = taps(100.0);
        let total = centre + 2.0 * pairs.iter().map(|(_, weight)| weight).sum::<f32>();
        assert!((total - 1.0).abs() < 1e-4, "the weights sum to {total}");

        // A pair outweighs the centre tap because it stands for two of them,
        // so what falls off is the pairs — and, per texel, the middle of the
        // kernel is still the heaviest, which is what `centre > pair / 2` says.
        let weights: Vec<f32> = pairs.iter().map(|(_, weight)| *weight).collect();
        assert!(centre > weights[0] / 2.0, "the centre tap is the heaviest");
        assert!(
            weights.windows(2).all(|pair| pair[0] > pair[1]),
            "the weights fall off with distance: {weights:?}"
        );
    }

    #[test]
    fn the_pairs_sit_between_two_texels() {
        // 1.5, 3.5, ...: half a texel past each even boundary, which is where
        // the sampler averages the two taps the pair stands for.
        for (index, (offset_px, _)) in taps(100.0).0.into_iter().enumerate() {
            let expected = 2.0 * (index + 1) as f32 - 0.5;
            assert!(
                (offset_px - expected).abs() < 1e-3,
                "pair {index} is at {offset_px} px, not {expected}"
            );
        }
    }

    #[test]
    fn an_axis_only_blurs_along_itself() {
        let bytes = kernel_bytes(96.0, false);
        for (index, entry) in bytes.as_chunks::<16>().0.iter().take(TAPS).enumerate() {
            let word =
                |at: usize| f32::from_le_bytes(entry[at * 4..at * 4 + 4].try_into().unwrap());
            assert_eq!(word(0), 0.0, "pair {index} has an x offset on the y axis");
            assert!(word(1) > 0.0, "pair {index} has no y offset");
        }
    }
}
