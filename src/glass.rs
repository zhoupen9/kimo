//! A frosted-glass backdrop for the floating title bar.
//!
//! egui cannot blur: nothing in the painter can sample what has already been
//! drawn, and there is no blur primitive. The *renderer* can, through a paint
//! callback — so the backdrop is a small OpenGL pass of our own
//! (`egui_glow::CallbackFn`): the pixels behind the bar are copied out of the
//! framebuffer into a texture and drawn back, blurred. The bar then paints its
//! translucent fill over that, which is what turns the mail scrolling under it
//! into a smudge of its colors instead of unreadable text.
//!
//! The blur is separable — a horizontal pass into an offscreen texture of the
//! bar's size, then a vertical one back onto the window — because a single
//! two-dimensional kernel cheap enough to run per frame is sampled too sparsely
//! to hide the detail it is meant to destroy: nine taps four pixels apart left
//! a visible comb over the text. Seventeen taps a pixel and a half apart, twice,
//! cost less and leave nothing readable.
//!
//! egui_glow restores its own GL state after every callback (it calls
//! `prepare_painting` again), so this pass binds program, VAO, textures and
//! framebuffer as it needs them and leaves them bound. Both passes draw one
//! full-screen quad — the bar *is* the viewport — so there is no projection to
//! set up, only the viewport of the offscreen pass, which is the bar's size.
//!
//! Every failure here is soft. A driver that will not compile the shader or
//! refuses to copy out of the framebuffer — a multisampled one would, which is
//! why `main.rs` keeps eframe's `multisampling` at its default of 0 — leaves
//! the bar translucent over the crisp content instead of frosted, and `failed`
//! keeps it from trying again every frame.

use std::sync::{Arc, Mutex};

use eframe::egui_glow;
use eframe::glow;

/// The blur kernel: `TAPS` samples either side of a fragment, `STEP_PX`
/// physical pixels apart, run once per axis. Sampling inside the pixel grid is
/// what keeps text from aliasing into a pattern instead of dissolving.
const TAPS: i32 = 8;
const STEP_PX: f32 = 2.0;

/// The glass pass, shared with the paint callback it hands to egui. Cloning it
/// clones a handle: the GL objects are made once, on the first callback.
#[derive(Clone, Default)]
pub struct Glass {
    blur: Arc<Mutex<Option<Blur>>>,
}

impl Glass {
    /// Blur whatever has already been drawn behind `rect`.
    ///
    /// egui runs a callback in the order the shape was added, so this belongs
    /// *before* the fill and the glyphs that sit on the glass. Nothing is
    /// painted outside `rect`: egui has already set the viewport to it.
    pub fn backdrop(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !rect.is_positive() {
            return;
        }
        let blur = self.blur.clone();
        painter.add(egui::Shape::Callback(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                let Ok(mut slot) = blur.lock() else {
                    return;
                };
                let blur = slot.get_or_insert_with(Blur::default);
                blur.paint(painter.gl(), painter.intermediate_fbo(), &info);
            })),
        }));
    }
}

/// The GL objects of the pass, made on first use and kept for the life of the
/// context (a new context would rebuild everything from scratch anyway).
#[derive(Default)]
struct Blur {
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    vbo: Option<glow::Buffer>,
    /// The pixels behind the bar, copied out of the framebuffer.
    source: Option<glow::Texture>,
    /// The horizontal pass's result, drawn into the next pass.
    target: Option<glow::Texture>,
    /// The framebuffer `target` is attached to.
    fbo: Option<glow::Framebuffer>,
    /// The size `source` and `target` were made for, so a resized bar makes
    /// new ones.
    size: [i32; 2],
    /// Set for good once the driver has refused something: the bar is then only
    /// translucent.
    failed: bool,
}

impl Blur {
    /// Copy the pixels behind the bar and draw them back blurred.
    fn paint(
        &mut self,
        gl: &glow::Context,
        window_fbo: Option<glow::Framebuffer>,
        info: &egui::PaintCallbackInfo,
    ) {
        use glow::HasContext as _;

        if self.failed {
            return;
        }
        let viewport = info.viewport_in_pixels();
        let (width, height) = (viewport.width_px, viewport.height_px);
        if width <= 0 || height <= 0 {
            return;
        }
        if self.program.is_none() {
            if let Err(error) = self.build(gl) {
                self.fail(&error);
                return;
            }
        }

        unsafe {
            // The offscreen pass has its own coordinate system, so egui's
            // scissor (the bar, in window pixels) cannot stay on; it re-enables
            // it after this callback.
            gl.disable(glow::SCISSOR_TEST);

            if self.size != [width, height] {
                self.discard_textures(gl);
                self.size = [width, height];
                match Self::new_target(gl, width, height) {
                    Ok((source, target, fbo)) => {
                        self.source = Some(source);
                        self.target = Some(target);
                        self.fbo = Some(fbo);
                    }
                    Err(error) => {
                        self.fail(&error);
                        return;
                    }
                }
            }
            let (Some(program), Some(vao), Some(source), Some(target), Some(fbo)) =
                (self.program, self.vao, self.source, self.target, self.fbo)
            else {
                return;
            };

            // Everything drawn before the bar is in the framebuffer we are
            // painting into, so the backdrop is one copy out of it.
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(source));
            gl.copy_tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                viewport.left_px,
                viewport.from_bottom_px,
                width,
                height,
            );
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                // A driver that will not read its own framebuffer (the copy is
                // invalid on a multisampled one) leaves the texture alone and
                // the bar translucent over the content instead of frosted.
                self.fail(&format!("GL error {error:#x} on the framebuffer copy"));
                return;
            }

            gl.use_program(Some(program));
            gl.bind_vertex_array(Some(vao));
            let texture_uniform = gl.get_uniform_location(program, "u_texture");
            let step_uniform = gl.get_uniform_location(program, "u_step");
            if let Some(location) = &texture_uniform {
                gl.uniform_1_i32(Some(location), 0);
            }
            let step = |gl: &glow::Context, x: f32, y: f32| {
                if let Some(location) = &step_uniform {
                    gl.uniform_2_f32(Some(location), x, y);
                }
            };

            // Horizontal, offscreen: the quad covers the whole target, so the
            // viewport is its size rather than the bar's place in the window.
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, width, height);
            step(gl, STEP_PX / width as f32, 0.0);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Vertical, back onto the bar itself.
            gl.bind_framebuffer(glow::FRAMEBUFFER, window_fbo);
            gl.viewport(viewport.left_px, viewport.from_bottom_px, width, height);
            gl.bind_texture(glow::TEXTURE_2D, Some(target));
            step(gl, 0.0, STEP_PX / height as f32);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Report a failure once and stop trying: the bar stays translucent.
    fn fail(&mut self, error: &str) {
        self.failed = true;
        eprintln!("kimo: the title bar will not be frosted: {error}");
    }

    /// Free what a resize replaces.
    unsafe fn discard_textures(&mut self, gl: &glow::Context) {
        use glow::HasContext as _;

        if let Some(texture) = self.source.take() {
            gl.delete_texture(texture);
        }
        if let Some(texture) = self.target.take() {
            gl.delete_texture(texture);
        }
        if let Some(fbo) = self.fbo.take() {
            gl.delete_framebuffer(fbo);
        }
    }

    /// The two textures of the pass and the framebuffer the first one draws
    /// into: the copy of the bar, and the horizontally blurred result.
    unsafe fn new_target(
        gl: &glow::Context,
        width: i32,
        height: i32,
    ) -> Result<(glow::Texture, glow::Texture, glow::Framebuffer), String> {
        use glow::HasContext as _;

        let source = new_texture(gl, width, height)?;
        let target = new_texture(gl, width, height)?;
        let fbo = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(target),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(fbo);
            gl.delete_texture(target);
            gl.delete_texture(source);
            return Err(format!("incomplete framebuffer: {status:#x}"));
        }
        Ok((source, target, fbo))
    }

    /// Compile the pass: one program, one VAO holding the quad.
    fn build(&mut self, gl: &glow::Context) -> Result<(), String> {
        use glow::HasContext as _;

        unsafe {
            let version = egui_glow::ShaderVersion::get(gl);
            // `#version 140` and `#version 300 es` both take the modern
            // in/out interface; a context older than either is not one eframe
            // runs on, and a shader that will not build is a soft failure.
            let header = format!(
                "{}{}",
                version.version_declaration(),
                if version.is_embedded() {
                    "precision mediump float;\n"
                } else {
                    ""
                }
            );
            let taps = TAPS;
            let samples = (2 * TAPS + 1) as f32;
            let vertex = format!(
                "{header}in vec2 a_pos;\nin vec2 a_uv;\nout vec2 v_uv;\n\
                 void main() {{\n    v_uv = a_uv;\n    gl_Position = vec4(a_pos, 0.0, 1.0);\n}}\n"
            );
            let fragment = format!(
                "{header}uniform sampler2D u_texture;\nuniform vec2 u_step;\nin vec2 v_uv;\n\
                 out vec4 f_color;\nvoid main() {{\n    vec4 sum = vec4(0.0);\n    \
                 for (int i = -{taps}; i <= {taps}; ++i) {{\n        \
                 sum += texture(u_texture, v_uv + vec2(float(i)) * u_step);\n    }}\n    \
                 f_color = sum / {samples};\n}}\n"
            );

            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, source) in [
                (glow::VERTEX_SHADER, &vertex),
                (glow::FRAGMENT_SHADER, &fragment),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    for shader in shaders {
                        gl.detach_shader(program, shader);
                        gl.delete_shader(shader);
                    }
                    gl.delete_program(program);
                    return Err(log);
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            let linked = gl.get_program_link_status(program);
            let log = gl.get_program_info_log(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            if !linked {
                gl.delete_program(program);
                return Err(log);
            }

            // A triangle strip over the whole viewport: the bar *is* the
            // viewport here, so the NDC corners are the bar's corners and the
            // texture's own corners are the uvs.
            let quad: [f32; 16] = [
                -1.0, -1.0, 0.0, 0.0, //
                1.0, -1.0, 1.0, 0.0, //
                -1.0, 1.0, 0.0, 1.0, //
                1.0, 1.0, 1.0, 1.0, //
            ];
            let bytes = std::slice::from_raw_parts(
                quad.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&quad),
            );
            let vao = gl.create_vertex_array()?;
            let vbo = gl.create_buffer()?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
            let stride = 4 * std::mem::size_of::<f32>() as i32;
            for (name, offset) in [("a_pos", 0), ("a_uv", 8)] {
                if let Some(location) = gl.get_attrib_location(program, name) {
                    gl.enable_vertex_attrib_array(location);
                    gl.vertex_attrib_pointer_f32(location, 2, glow::FLOAT, false, stride, offset);
                }
            }

            self.program = Some(program);
            self.vao = Some(vao);
            self.vbo = Some(vbo);
            Ok(())
        }
    }
}

/// A texture the size of the bar, with linear sampling clamped to its edge: the
/// blur reads past the edges, and clamping keeps the far side from bleeding in.
unsafe fn new_texture(
    gl: &glow::Context,
    width: i32,
    height: i32,
) -> Result<glow::Texture, String> {
    use glow::HasContext as _;

    let texture = gl.create_texture()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    // No pixels of our own: whatever samples this fills it first.
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        width,
        height,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(None),
    );
    Ok(texture)
}
