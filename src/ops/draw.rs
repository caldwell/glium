use std::ptr;
use std::ops::Range;

use BufferExt;
use BufferSliceExt;
use ProgramExt;
use DrawError;
use UniformsExt;

use context::Context;
use ContextExt;
use QueryExt;
use TransformFeedbackSessionExt;

use fbo::{self, ValidatedAttachments};

use uniforms::Uniforms;
use {Program, ToGlEnum};
use index::{self, IndicesSource, PrimitiveType};
use vertex::{MultiVerticesSource, VerticesSource, TransformFeedbackSession};
use vertex_array_object::VertexAttributesSystem;

use draw_parameters::DrawParameters;
use draw_parameters::{Blend, BlendingFunction, BackfaceCullingMode,
    LinearBlendingFactor};
use draw_parameters::{DepthTest, DepthClamp, PolygonMode, StencilTest};
use draw_parameters::{SamplesQueryParam, TransformFeedbackPrimitivesWrittenQuery};
use draw_parameters::{PrimitivesGeneratedQuery, TimeElapsedQuery, ConditionalRendering};
use draw_parameters::{Smooth, ProvokingVertex};
use Rect;

use libc;
use {gl, context, draw_parameters};
use version::Version;
use version::Api;

/// Draws everything.
pub fn draw<'a, U, V>(context: &Context, framebuffer: Option<&ValidatedAttachments>,
                      vertex_buffers: V, indices: IndicesSource,
                      program: &Program, uniforms: &U, draw_parameters: &DrawParameters,
                      dimensions: (u32, u32)) -> Result<(), DrawError>
                      where U: Uniforms, V: MultiVerticesSource<'a>
{
    try!(draw_parameters::validate(context, draw_parameters));

    // this contains the list of fences that will need to be fulfilled after the draw command
    // has started
    let mut fences = Vec::with_capacity(0);

    // handling tessellation
    let vertices_per_patch = match indices.get_primitives_type() {
        index::PrimitiveType::Patches { vertices_per_patch } => {
            if let Some(max) = context.capabilities().max_patch_vertices {
                if vertices_per_patch == 0 || vertices_per_patch as gl::types::GLint > max {
                    return Err(DrawError::UnsupportedVerticesPerPatch);
                }
            } else {
                return Err(DrawError::TessellationNotSupported);
            }

            // TODO: programs created from binaries have the wrong value
            // for `has_tessellation_shaders`
            /*if !program.has_tessellation_shaders() {    // TODO:
                panic!("Default tessellation level is not supported yet");
            }*/

            Some(vertices_per_patch)
        },
        _ => {
            // TODO: programs created from binaries have the wrong value
            // for `has_tessellation_shaders`
            /*if program.has_tessellation_shaders() {
                return Err(DrawError::TessellationWithoutPatches);
            }*/

            None
        },
    };

    // starting the state changes
    let mut ctxt = context.make_current();

    // handling vertices source
    let (vertices_count, instances_count, base_vertex) = {
        let index_buffer = match indices {
            IndicesSource::IndexBuffer { buffer, .. } => Some(buffer),
            IndicesSource::MultidrawArray { .. } => None,
            IndicesSource::MultidrawElement { indices, .. } => Some(indices),
            IndicesSource::NoIndices { .. } => None,
        };

        // determining whether we can use the `base_vertex` variants for drawing
        let use_base_vertex = match indices {
            IndicesSource::MultidrawArray { .. } => false,
            IndicesSource::MultidrawElement { .. } => false,
            IndicesSource::NoIndices { .. } => true,
            _ => ctxt.version >= &Version(Api::Gl, 3, 2) ||
                 ctxt.version >= &Version(Api::GlEs, 3, 2) ||
                 ctxt.extensions.gl_arb_draw_elements_base_vertex ||
                 ctxt.extensions.gl_oes_draw_elements_base_vertex
        };

        // object that is used to build the bindings
        let mut binder = VertexAttributesSystem::start(&mut ctxt, program, index_buffer,
                                                       use_base_vertex);
        // number of vertices in the vertices sources, or `None` if there is a mismatch
        let mut vertices_count: Option<usize> = None;
        // number of instances to draw
        let mut instances_count: Option<usize> = None;

        for src in vertex_buffers.iter() {
            match src {
                VerticesSource::VertexBuffer(buffer, format, per_instance) => {
                    // TODO: assert!(buffer.get_elements_size() == total_size(format));

                    if let Some(fence) = buffer.add_fence() {
                        fences.push(fence);
                    }

                    binder = binder.add(&buffer, format, if per_instance { Some(1) } else { None });
                },
                _ => {}
            }

            match src {
                VerticesSource::VertexBuffer(ref buffer, _, false) => {
                    if let Some(curr) = vertices_count {
                        if curr != buffer.get_elements_count() {
                            vertices_count = None;
                            break;
                        }
                    } else {
                        vertices_count = Some(buffer.get_elements_count());
                    }
                },
                VerticesSource::VertexBuffer(ref buffer, _, true) => {
                    if let Some(curr) = instances_count {
                        if curr != buffer.get_elements_count() {
                            return Err(DrawError::InstancesCountMismatch);
                        }
                    } else {
                        instances_count = Some(buffer.get_elements_count());
                    }
                },
                VerticesSource::Marker { len, per_instance } if !per_instance => {
                    if let Some(curr) = vertices_count {
                        if curr != len {
                            vertices_count = None;
                            break;
                        }
                    } else {
                        vertices_count = Some(len);
                    }
                },
                VerticesSource::Marker { len, per_instance } if per_instance => {
                    if let Some(curr) = instances_count {
                        if curr != len {
                            return Err(DrawError::InstancesCountMismatch);
                        }
                    } else {
                        instances_count = Some(len);
                    }
                },
                _ => ()
            }
        }

        (vertices_count, instances_count, binder.bind().unwrap_or(0))
    };

    // binding the FBO to draw upon
    {
        let fbo_id = fbo::FramebuffersContainer::get_framebuffer_for_drawing(&mut ctxt, framebuffer);
        unsafe { fbo::bind_framebuffer(&mut ctxt, fbo_id, true, false) };
    };

    // binding the program and uniforms
    program.use_program(&mut ctxt);
    try!(uniforms.bind_uniforms(&mut ctxt, program, &mut fences));

    // sync-ing draw_parameters
    unsafe {
        try!(sync_depth(&mut ctxt, draw_parameters.depth_test, draw_parameters.depth_write,
                        draw_parameters.depth_range, draw_parameters.depth_clamp));
        sync_stencil(&mut ctxt, &draw_parameters);
        try!(sync_blending(&mut ctxt, draw_parameters.blend));
        sync_color_mask(&mut ctxt, draw_parameters.color_mask);
        sync_line_width(&mut ctxt, draw_parameters.line_width);
        sync_point_size(&mut ctxt, draw_parameters.point_size);
        sync_polygon_mode(&mut ctxt, draw_parameters.backface_culling, draw_parameters.polygon_mode);
        sync_multisampling(&mut ctxt, draw_parameters.multisampling);
        sync_dithering(&mut ctxt, draw_parameters.dithering);
        sync_viewport_scissor(&mut ctxt, draw_parameters.viewport, draw_parameters.scissor,
                              dimensions);
        try!(sync_rasterizer_discard(&mut ctxt, draw_parameters.draw_primitives));
        sync_vertices_per_patch(&mut ctxt, vertices_per_patch);
        try!(sync_queries(&mut ctxt, draw_parameters.samples_passed_query,
                          draw_parameters.time_elapsed_query,
                          draw_parameters.primitives_generated_query,
                          draw_parameters.transform_feedback_primitives_written_query));
        sync_conditional_render(&mut ctxt, draw_parameters.condition);
        try!(sync_smooth(&mut ctxt, draw_parameters.smooth, indices.get_primitives_type()));
        try!(sync_provoking_vertex(&mut ctxt, draw_parameters.provoking_vertex));
        sync_primitive_bounding_box(&mut ctxt, &draw_parameters.primitive_bounding_box);

        // TODO: make sure that the program is the right one
        // TODO: changing the current transform feedback requires pausing/unbinding before changing the program
        if let Some(ref tf) = draw_parameters.transform_feedback {
            tf.bind(&mut ctxt, indices.get_primitives_type());
        } else {
            TransformFeedbackSession::unbind(&mut ctxt);
        }
    }

    // drawing
    // TODO: make this code more readable
    {
        match &indices {
            &IndicesSource::IndexBuffer { ref buffer, data_type, primitives } => {
                let ptr: *const u8 = ptr::null_mut();
                let ptr = unsafe { ptr.offset(buffer.get_offset_bytes() as isize) };

                if let Some(fence) = buffer.add_fence() {
                    fences.push(fence);
                }

                unsafe {
                    if let Some(instances_count) = instances_count {
                        if base_vertex != 0 {
                            if ctxt.version >= &Version(Api::Gl, 3, 2) ||
                               ctxt.version >= &Version(Api::GlEs, 3, 2) ||
                               ctxt.extensions.gl_arb_draw_elements_base_vertex
                            {
                                ctxt.gl.DrawElementsInstancedBaseVertex(primitives.to_glenum(),
                                                                     buffer.get_elements_count() as
                                                                        gl::types::GLsizei,
                                                                        data_type.to_glenum(),
                                                                        ptr as *const libc::c_void,
                                                                        instances_count as
                                                                        gl::types::GLsizei,
                                                                        base_vertex);

                            } else if ctxt.extensions.gl_oes_draw_elements_base_vertex {
                                ctxt.gl.DrawElementsInstancedBaseVertexOES(primitives.to_glenum(),
                                                                     buffer.get_elements_count() as
                                                                           gl::types::GLsizei,
                                                                           data_type.to_glenum(),
                                                                        ptr as *const libc::c_void,
                                                                           instances_count as
                                                                           gl::types::GLsizei,
                                                                           base_vertex);
                            } else {
                                unreachable!();
                            }

                        } else {
                            ctxt.gl.DrawElementsInstanced(primitives.to_glenum(),
                                                          buffer.get_elements_count() as
                                                          gl::types::GLsizei,
                                                          data_type.to_glenum(),
                                                          ptr as *const libc::c_void,
                                                          instances_count as gl::types::GLsizei);
                        }

                    } else {
                        if base_vertex != 0 {
                            if ctxt.version >= &Version(Api::Gl, 3, 2) ||
                               ctxt.version >= &Version(Api::GlEs, 3, 2) ||
                               ctxt.extensions.gl_arb_draw_elements_base_vertex
                            {
                                ctxt.gl.DrawElementsBaseVertex(primitives.to_glenum(),
                                                               buffer.get_elements_count() as
                                                               gl::types::GLsizei,
                                                               data_type.to_glenum(),
                                                               ptr as *const libc::c_void,
                                                               base_vertex);

                            } else if ctxt.extensions.gl_oes_draw_elements_base_vertex {
                                ctxt.gl.DrawElementsBaseVertexOES(primitives.to_glenum(),
                                                                  buffer.get_elements_count() as
                                                                  gl::types::GLsizei,
                                                                  data_type.to_glenum(),
                                                                  ptr as *const libc::c_void,
                                                                  base_vertex);
                            } else {
                                unreachable!();
                            }

                        } else {
                            ctxt.gl.DrawElements(primitives.to_glenum(),
                                                 buffer.get_elements_count() as gl::types::GLsizei,
                                                 data_type.to_glenum(),
                                                 ptr as *const libc::c_void);
                        }
                    }
                }
            },

            &IndicesSource::MultidrawArray { ref buffer, primitives } => {
                let ptr: *const u8 = ptr::null_mut();
                let ptr = unsafe { ptr.offset(buffer.get_offset_bytes() as isize) };

                debug_assert_eq!(base_vertex, 0);       // enforced earlier in this function

                if let Some(fence) = buffer.add_fence() {
                    fences.push(fence);
                }

                unsafe {
                    buffer.prepare_and_bind_for_draw_indirect(&mut ctxt);
                    ctxt.gl.MultiDrawArraysIndirect(primitives.to_glenum(), ptr as *const _,
                                                    buffer.get_elements_count() as gl::types::GLsizei,
                                                    0);
                }
            },

            &IndicesSource::MultidrawElement { ref commands, ref indices, data_type, primitives } => {
                let cmd_ptr: *const u8 = ptr::null_mut();
                let cmd_ptr = unsafe { cmd_ptr.offset(commands.get_offset_bytes() as isize) };

                if let Some(fence) = commands.add_fence() {
                    fences.push(fence);
                }

                if let Some(fence) = indices.add_fence() {
                    fences.push(fence);
                }

                unsafe {
                    commands.prepare_and_bind_for_draw_indirect(&mut ctxt);
                    debug_assert_eq!(base_vertex, 0);       // enforced earlier in this function
                    ctxt.gl.MultiDrawElementsIndirect(primitives.to_glenum(), data_type.to_glenum(),
                                                      cmd_ptr as *const _,
                                                      commands.get_elements_count() as gl::types::GLsizei,
                                                      0);
                }
            },

            &IndicesSource::NoIndices { primitives } => {
                let vertices_count = match vertices_count {
                    Some(c) => c,
                    None => return Err(DrawError::VerticesSourcesLengthMismatch)
                };

                unsafe {
                    if let Some(instances_count) = instances_count {
                        ctxt.gl.DrawArraysInstanced(primitives.to_glenum(), base_vertex,
                                                    vertices_count as gl::types::GLsizei,
                                                    instances_count as gl::types::GLsizei);
                    } else {
                        ctxt.gl.DrawArrays(primitives.to_glenum(), base_vertex,
                                           vertices_count as gl::types::GLsizei);
                    }
                }
            },
        };
    };

    ctxt.state.next_draw_call_id += 1;

    // fulfilling the fences
    for fence in fences.into_iter() {
        fence.insert(&mut ctxt);
    }

    Ok(())
}

fn sync_depth(ctxt: &mut context::CommandContext, depth_test: DepthTest, depth_write: bool,
              depth_range: (f32, f32), depth_clamp: DepthClamp) -> Result<(), DrawError>
{
    // depth clamp
    {
        let state = &mut *ctxt.state;
        match (depth_clamp, &mut state.enabled_depth_clamp_near,
               &mut state.enabled_depth_clamp_far)
        {
            (DepthClamp::NoClamp, &mut false, &mut false) => (),
            (DepthClamp::Clamp, &mut true, &mut true) => (),

            (DepthClamp::NoClamp, near, far) => {
                if ctxt.version >= &Version(Api::Gl, 3, 0) || ctxt.extensions.gl_arb_depth_clamp ||
                   ctxt.extensions.gl_nv_depth_clamp
                {
                    unsafe { ctxt.gl.Disable(gl::DEPTH_CLAMP) };
                    *near = false;
                    *far = false;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }
            },

            (DepthClamp::Clamp, near, far) => {
                if ctxt.version >= &Version(Api::Gl, 3, 0) || ctxt.extensions.gl_arb_depth_clamp ||
                   ctxt.extensions.gl_nv_depth_clamp
                {
                    unsafe { ctxt.gl.Enable(gl::DEPTH_CLAMP) };
                    *near = true;
                    *far = true;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }
            },

            (DepthClamp::ClampNear, &mut true, &mut false) => (),
            (DepthClamp::ClampFar, &mut false, &mut true) => (),

            (DepthClamp::ClampNear, &mut true, far) => {
                if ctxt.extensions.gl_amd_depth_clamp_separate {
                    unsafe { ctxt.gl.Disable(gl::DEPTH_CLAMP_FAR_AMD) };
                    *far = false;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }

            },

            (DepthClamp::ClampNear, near @ &mut false, far) => {
                if ctxt.extensions.gl_amd_depth_clamp_separate {
                    unsafe { ctxt.gl.Enable(gl::DEPTH_CLAMP_NEAR_AMD) };
                    if *far { unsafe { ctxt.gl.Disable(gl::DEPTH_CLAMP_FAR_AMD); } }
                    *near = true;
                    *far = false;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }
            },

            (DepthClamp::ClampFar, near, &mut true) => {
                if ctxt.extensions.gl_amd_depth_clamp_separate {
                    unsafe { ctxt.gl.Disable(gl::DEPTH_CLAMP_NEAR_AMD) };
                    *near = false;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }
            },

            (DepthClamp::ClampFar, near, far @ &mut false) => {
                if ctxt.extensions.gl_amd_depth_clamp_separate {
                    unsafe { ctxt.gl.Enable(gl::DEPTH_CLAMP_FAR_AMD) };
                    if *near { unsafe { ctxt.gl.Disable(gl::DEPTH_CLAMP_NEAR_AMD); } }
                    *near = false;
                    *far = true;
                } else {
                    return Err(DrawError::DepthClampNotSupported);
                }
            },
        }
    }

    // depth range
    if depth_range != ctxt.state.depth_range {
        unsafe {
            ctxt.gl.DepthRange(depth_range.0 as f64, depth_range.1 as f64);
        }
        ctxt.state.depth_range = depth_range;
    }

    if depth_test == DepthTest::Overwrite && !depth_write {
        // simply disabling GL_DEPTH_TEST
        if ctxt.state.enabled_depth_test {
            unsafe { ctxt.gl.Disable(gl::DEPTH_TEST) };
            ctxt.state.enabled_depth_test = false;
        }
        return Ok(());

    } else {
        if !ctxt.state.enabled_depth_test {
            unsafe { ctxt.gl.Enable(gl::DEPTH_TEST) };
            ctxt.state.enabled_depth_test = true;
        }
    }

    // depth test
    unsafe {
        let depth_test = depth_test.to_glenum();
        if ctxt.state.depth_func != depth_test {
            ctxt.gl.DepthFunc(depth_test);
            ctxt.state.depth_func = depth_test;
        }
    }

    // depth mask
    if depth_write != ctxt.state.depth_mask {
        unsafe {
            ctxt.gl.DepthMask(if depth_write { gl::TRUE } else { gl::FALSE });
        }
        ctxt.state.depth_mask = depth_write;
    }

    Ok(())
}

fn sync_stencil(ctxt: &mut context::CommandContext, params: &DrawParameters) {
    // TODO: optimize me

    let (test_cw, read_mask_cw) = match params.stencil_test_clockwise {
        StencilTest::AlwaysPass => (gl::ALWAYS, 0),
        StencilTest::AlwaysFail => (gl::NEVER, 0),
        StencilTest::IfLess { mask } => (gl::LESS, mask),
        StencilTest::IfLessOrEqual { mask } => (gl::LEQUAL, mask),
        StencilTest::IfMore { mask } => (gl::GREATER, mask),
        StencilTest::IfMoreOrEqual { mask } => (gl::GEQUAL, mask),
        StencilTest::IfEqual { mask } => (gl::EQUAL, mask),
        StencilTest::IfNotEqual { mask } => (gl::NOTEQUAL, mask),
    };

    let (test_ccw, read_mask_ccw) = match params.stencil_test_counter_clockwise {
        StencilTest::AlwaysPass => (gl::ALWAYS, 0),
        StencilTest::AlwaysFail => (gl::NEVER, 0),
        StencilTest::IfLess { mask } => (gl::LESS, mask),
        StencilTest::IfLessOrEqual { mask } => (gl::LEQUAL, mask),
        StencilTest::IfMore { mask } => (gl::GREATER, mask),
        StencilTest::IfMoreOrEqual { mask } => (gl::GEQUAL, mask),
        StencilTest::IfEqual { mask } => (gl::EQUAL, mask),
        StencilTest::IfNotEqual { mask } => (gl::NOTEQUAL, mask),
    };

    if ctxt.state.stencil_func_back != (test_cw, params.stencil_reference_value_clockwise, read_mask_cw) {
        unsafe { ctxt.gl.StencilFuncSeparate(gl::BACK, test_cw, params.stencil_reference_value_clockwise, read_mask_cw) };
        ctxt.state.stencil_func_back = (test_cw, params.stencil_reference_value_clockwise, read_mask_cw);
    }

    if ctxt.state.stencil_func_front != (test_ccw, params.stencil_reference_value_counter_clockwise, read_mask_ccw) {
        unsafe { ctxt.gl.StencilFuncSeparate(gl::FRONT, test_ccw, params.stencil_reference_value_counter_clockwise, read_mask_ccw) };
        ctxt.state.stencil_func_front = (test_ccw, params.stencil_reference_value_counter_clockwise, read_mask_ccw);
    }

    if ctxt.state.stencil_mask_back != params.stencil_write_mask_clockwise {
        unsafe { ctxt.gl.StencilMaskSeparate(gl::BACK, params.stencil_write_mask_clockwise) };
        ctxt.state.stencil_mask_back = params.stencil_write_mask_clockwise;
    }

    if ctxt.state.stencil_mask_front != params.stencil_write_mask_clockwise {
        unsafe { ctxt.gl.StencilMaskSeparate(gl::FRONT, params.stencil_write_mask_clockwise) };
        ctxt.state.stencil_mask_front = params.stencil_write_mask_clockwise;
    }

    let op_back = (params.stencil_fail_operation_clockwise.to_glenum(),
                   params.stencil_pass_depth_fail_operation_clockwise.to_glenum(),
                   params.stencil_depth_pass_operation_clockwise.to_glenum());
    if ctxt.state.stencil_op_back != op_back {
        unsafe { ctxt.gl.StencilOpSeparate(gl::BACK, op_back.0, op_back.1, op_back.2) };
        ctxt.state.stencil_op_back = op_back;
    }

    let op_front = (params.stencil_fail_operation_counter_clockwise.to_glenum(),
                    params.stencil_pass_depth_fail_operation_counter_clockwise.to_glenum(),
                    params.stencil_depth_pass_operation_counter_clockwise.to_glenum());
    if ctxt.state.stencil_op_front != op_front {
        unsafe { ctxt.gl.StencilOpSeparate(gl::FRONT, op_front.0, op_front.1, op_front.2) };
        ctxt.state.stencil_op_front = op_front;
    }

    let enable_stencil = test_cw != gl::ALWAYS || test_ccw != gl::ALWAYS ||
                         op_back.0 != gl::KEEP || op_front.0 != gl::KEEP ||
                         op_back.1 != gl::KEEP || op_front.1 != gl::KEEP ||
                         op_back.2 != gl::KEEP || op_front.2 != gl::KEEP;
    if ctxt.state.enabled_stencil_test != enable_stencil {
        if enable_stencil {
            unsafe { ctxt.gl.Enable(gl::STENCIL_TEST) };
        } else {
            unsafe { ctxt.gl.Disable(gl::STENCIL_TEST) };
        }

        ctxt.state.enabled_stencil_test = enable_stencil;
    }
}

fn sync_blending(ctxt: &mut context::CommandContext, blend: Blend) -> Result<(), DrawError> {
    #[inline(always)]
    fn blend_eq(ctxt: &mut context::CommandContext, blending_function: BlendingFunction)
                -> Result<gl::types::GLenum, DrawError>
    {
        match blending_function {
            BlendingFunction::AlwaysReplace |
            BlendingFunction::Addition { .. } => Ok(gl::FUNC_ADD),
            BlendingFunction::Subtraction { .. } => Ok(gl::FUNC_SUBTRACT),
            BlendingFunction::ReverseSubtraction { .. } => Ok(gl::FUNC_REVERSE_SUBTRACT),

            BlendingFunction::Min => {
                if ctxt.version <= &Version(Api::GlEs, 2, 0) &&
                   !ctxt.extensions.gl_ext_blend_minmax
                {
                    Err(DrawError::BlendingParameterNotSupported)
                } else {
                    Ok(gl::MIN)
                }
            },

            BlendingFunction::Max => {
                if ctxt.version <= &Version(Api::GlEs, 2, 0) &&
                   !ctxt.extensions.gl_ext_blend_minmax
                {
                    Err(DrawError::BlendingParameterNotSupported)
                } else {
                    Ok(gl::MAX)
                }
            },
        }
    }

    #[inline(always)]
    fn blending_factors(blending_function: BlendingFunction)
                        -> Option<(LinearBlendingFactor, LinearBlendingFactor)>
    {
        match blending_function {
            BlendingFunction::AlwaysReplace |
            BlendingFunction::Min |
            BlendingFunction::Max => None,
            BlendingFunction::Addition { source, destination } =>
                Some((source, destination)),
            BlendingFunction::Subtraction { source, destination } =>
                Some((source, destination)),
            BlendingFunction::ReverseSubtraction { source, destination } =>
                Some((source, destination)),
        }
    }

    if let (BlendingFunction::AlwaysReplace, BlendingFunction::AlwaysReplace) =
           (blend.color, blend.alpha)
    {
        // Both color and alpha always replace. This equals no blending.
        if ctxt.state.enabled_blend {
            unsafe { ctxt.gl.Disable(gl::BLEND); }
            ctxt.state.enabled_blend = false;
        }

    } else {
        if !ctxt.state.enabled_blend {
            unsafe { ctxt.gl.Enable(gl::BLEND); }
            ctxt.state.enabled_blend = true;
        }

        let (color_eq, alpha_eq) = (try!(blend_eq(ctxt, blend.color)),
                                    try!(blend_eq(ctxt, blend.alpha)));
        if ctxt.state.blend_equation != (color_eq, alpha_eq) {
            unsafe { ctxt.gl.BlendEquationSeparate(color_eq, alpha_eq); }
            ctxt.state.blend_equation = (color_eq, alpha_eq);
        }

        // Map to dummy factors if the blending equation does not use the factors.
        let (color_factor_src, color_factor_dst) = blending_factors(blend.color)
            .unwrap_or((LinearBlendingFactor::One, LinearBlendingFactor::Zero));
        let (alpha_factor_src, alpha_factor_dst) = blending_factors(blend.alpha)
            .unwrap_or((LinearBlendingFactor::One, LinearBlendingFactor::Zero));
        let color_factor_src = color_factor_src.to_glenum();
        let color_factor_dst = color_factor_dst.to_glenum();
        let alpha_factor_src = alpha_factor_src.to_glenum();
        let alpha_factor_dst = alpha_factor_dst.to_glenum();
        if ctxt.state.blend_func != (color_factor_src, color_factor_dst,
            alpha_factor_src, alpha_factor_dst) {
            unsafe {
                ctxt.gl.BlendFuncSeparate(color_factor_src, color_factor_dst,
                    alpha_factor_src, alpha_factor_dst);
            }
            ctxt.state.blend_func = (color_factor_src, color_factor_dst,
                alpha_factor_src, alpha_factor_dst);
        }

        // Update blend color.
        if ctxt.state.blend_color != blend.constant_value {
            let (r, g, b, a) = blend.constant_value;
            unsafe { ctxt.gl.BlendColor(r, g, b, a); }
            ctxt.state.blend_color = blend.constant_value;
        }
    }

    Ok(())
}

fn sync_color_mask(ctxt: &mut context::CommandContext, mask: (bool, bool, bool, bool)) {
    let mask = (
        if mask.0 { 1 } else { 0 },
        if mask.1 { 1 } else { 0 },
        if mask.2 { 1 } else { 0 },
        if mask.3 { 1 } else { 0 },
    );

    if ctxt.state.color_mask != mask {
        unsafe {
            ctxt.gl.ColorMask(mask.0, mask.1, mask.2, mask.3);
        }

        ctxt.state.color_mask = mask;
    }
}

fn sync_line_width(ctxt: &mut context::CommandContext, line_width: Option<f32>) {
    if let Some(line_width) = line_width {
        if ctxt.state.line_width != line_width {
            unsafe {
                ctxt.gl.LineWidth(line_width);
                ctxt.state.line_width = line_width;
            }
        }
    }
}

fn sync_point_size(ctxt: &mut context::CommandContext, point_size: Option<f32>) {
    if let Some(point_size) = point_size {
        if ctxt.state.point_size != point_size {
            unsafe {
                ctxt.gl.PointSize(point_size);
                ctxt.state.point_size = point_size;
            }
        }
    }
}

fn sync_polygon_mode(ctxt: &mut context::CommandContext, backface_culling: BackfaceCullingMode,
                     polygon_mode: PolygonMode)
{
    // back-face culling
    // note: we never change the value of `glFrontFace`, whose default is GL_CCW
    //  that's why `CullClockWise` uses `GL_BACK` for example
    match backface_culling {
        BackfaceCullingMode::CullingDisabled => unsafe {
            if ctxt.state.enabled_cull_face {
                ctxt.gl.Disable(gl::CULL_FACE);
                ctxt.state.enabled_cull_face = false;
            }
        },
        BackfaceCullingMode::CullCounterClockWise => unsafe {
            if !ctxt.state.enabled_cull_face {
                ctxt.gl.Enable(gl::CULL_FACE);
                ctxt.state.enabled_cull_face = true;
            }
            if ctxt.state.cull_face != gl::FRONT {
                ctxt.gl.CullFace(gl::FRONT);
                ctxt.state.cull_face = gl::FRONT;
            }
        },
        BackfaceCullingMode::CullClockWise => unsafe {
            if !ctxt.state.enabled_cull_face {
                ctxt.gl.Enable(gl::CULL_FACE);
                ctxt.state.enabled_cull_face = true;
            }
            if ctxt.state.cull_face != gl::BACK {
                ctxt.gl.CullFace(gl::BACK);
                ctxt.state.cull_face = gl::BACK;
            }
        },
    }

    // polygon mode
    unsafe {
        let polygon_mode = polygon_mode.to_glenum();
        if ctxt.state.polygon_mode != polygon_mode {
            ctxt.gl.PolygonMode(gl::FRONT_AND_BACK, polygon_mode);
            ctxt.state.polygon_mode = polygon_mode;
        }
    }
}

fn sync_multisampling(ctxt: &mut context::CommandContext, multisampling: bool) {
    if ctxt.state.enabled_multisample != multisampling {
        unsafe {
            if multisampling {
                ctxt.gl.Enable(gl::MULTISAMPLE);
                ctxt.state.enabled_multisample = true;
            } else {
                ctxt.gl.Disable(gl::MULTISAMPLE);
                ctxt.state.enabled_multisample = false;
            }
        }
    }
}

fn sync_dithering(ctxt: &mut context::CommandContext, dithering: bool) {
    if ctxt.state.enabled_dither != dithering {
        unsafe {
            if dithering {
                ctxt.gl.Enable(gl::DITHER);
                ctxt.state.enabled_dither = true;
            } else {
                ctxt.gl.Disable(gl::DITHER);
                ctxt.state.enabled_dither = false;
            }
        }
    }
}

fn sync_viewport_scissor(ctxt: &mut context::CommandContext, viewport: Option<Rect>,
                         scissor: Option<Rect>, surface_dimensions: (u32, u32))
{
    // viewport
    if let Some(viewport) = viewport {
        assert!(viewport.width <= ctxt.capabilities.max_viewport_dims.0 as u32,
                "Viewport dimensions are too large");
        assert!(viewport.height <= ctxt.capabilities.max_viewport_dims.1 as u32,
                "Viewport dimensions are too large");

        let viewport = (viewport.left as gl::types::GLint, viewport.bottom as gl::types::GLint,
                        viewport.width as gl::types::GLsizei,
                        viewport.height as gl::types::GLsizei);

        if ctxt.state.viewport != Some(viewport) {
            unsafe { ctxt.gl.Viewport(viewport.0, viewport.1, viewport.2, viewport.3); }
            ctxt.state.viewport = Some(viewport);
        }

    } else {
        assert!(surface_dimensions.0 <= ctxt.capabilities.max_viewport_dims.0 as u32,
                "Viewport dimensions are too large");
        assert!(surface_dimensions.1 <= ctxt.capabilities.max_viewport_dims.1 as u32,
                "Viewport dimensions are too large");

        let viewport = (0, 0, surface_dimensions.0 as gl::types::GLsizei,
                        surface_dimensions.1 as gl::types::GLsizei);

        if ctxt.state.viewport != Some(viewport) {
            unsafe { ctxt.gl.Viewport(viewport.0, viewport.1, viewport.2, viewport.3); }
            ctxt.state.viewport = Some(viewport);
        }
    }

    // scissor
    if let Some(scissor) = scissor {
        let scissor = (scissor.left as gl::types::GLint, scissor.bottom as gl::types::GLint,
                       scissor.width as gl::types::GLsizei,
                       scissor.height as gl::types::GLsizei);

        unsafe {
            if ctxt.state.scissor != Some(scissor) {
                ctxt.gl.Scissor(scissor.0, scissor.1, scissor.2, scissor.3);
                ctxt.state.scissor = Some(scissor);
            }

            if !ctxt.state.enabled_scissor_test {
                ctxt.gl.Enable(gl::SCISSOR_TEST);
                ctxt.state.enabled_scissor_test = true;
            }
        }
    } else {
        unsafe {
            if ctxt.state.enabled_scissor_test {
                ctxt.gl.Disable(gl::SCISSOR_TEST);
                ctxt.state.enabled_scissor_test = false;
            }
        }
    }
}

fn sync_rasterizer_discard(ctxt: &mut context::CommandContext, draw_primitives: bool)
                           -> Result<(), DrawError>
{
    if ctxt.state.enabled_rasterizer_discard == draw_primitives {
        if ctxt.version >= &Version(Api::Gl, 3, 0) {
            if draw_primitives {
                unsafe { ctxt.gl.Disable(gl::RASTERIZER_DISCARD); }
                ctxt.state.enabled_rasterizer_discard = false;
            } else {
                unsafe { ctxt.gl.Enable(gl::RASTERIZER_DISCARD); }
                ctxt.state.enabled_rasterizer_discard = true;
            }

        } else if ctxt.extensions.gl_ext_transform_feedback {
            if draw_primitives {
                unsafe { ctxt.gl.Disable(gl::RASTERIZER_DISCARD_EXT); }
                ctxt.state.enabled_rasterizer_discard = false;
            } else {
                unsafe { ctxt.gl.Enable(gl::RASTERIZER_DISCARD_EXT); }
                ctxt.state.enabled_rasterizer_discard = true;
            }

        } else {
            return Err(DrawError::RasterizerDiscardNotSupported);
        }
    }

    Ok(())
}

unsafe fn sync_vertices_per_patch(ctxt: &mut context::CommandContext, vertices_per_patch: Option<u16>) {
    if let Some(vertices_per_patch) = vertices_per_patch {
        let vertices_per_patch = vertices_per_patch as gl::types::GLint;
        if ctxt.state.patch_patch_vertices != vertices_per_patch {
            ctxt.gl.PatchParameteri(gl::PATCH_VERTICES, vertices_per_patch);
            ctxt.state.patch_patch_vertices = vertices_per_patch;
        }
    }
}

fn sync_queries(ctxt: &mut context::CommandContext,
                samples_passed_query: Option<SamplesQueryParam>,
                time_elapsed_query: Option<&TimeElapsedQuery>,
                primitives_generated_query: Option<&PrimitivesGeneratedQuery>,
                transform_feedback_primitives_written_query:
                                            Option<&TransformFeedbackPrimitivesWrittenQuery>)
                -> Result<(), DrawError>
{
    if let Some(SamplesQueryParam::SamplesPassedQuery(q)) = samples_passed_query {
        try!(q.begin_query(ctxt));
    } else if let Some(SamplesQueryParam::AnySamplesPassedQuery(q)) = samples_passed_query {
        try!(q.begin_query(ctxt));
    } else {
        TimeElapsedQuery::end_samples_passed_query(ctxt);
    }

    if let Some(time_elapsed_query) = time_elapsed_query {
        try!(time_elapsed_query.begin_query(ctxt));
    } else {
        TimeElapsedQuery::end_time_elapsed_query(ctxt);
    }

    if let Some(primitives_generated_query) = primitives_generated_query {
        try!(primitives_generated_query.begin_query(ctxt));
    } else {
        TimeElapsedQuery::end_primitives_generated_query(ctxt);
    }

    if let Some(tfq) = transform_feedback_primitives_written_query {
        try!(tfq.begin_query(ctxt));
    } else {
        TimeElapsedQuery::end_transform_feedback_primitives_written_query(ctxt);
    }

    Ok(())
}

fn sync_conditional_render(ctxt: &mut context::CommandContext,
                           condition: Option<ConditionalRendering>)
{
    if let Some(ConditionalRendering { query, wait, per_region }) = condition {
        match query {
            SamplesQueryParam::SamplesPassedQuery(ref q) => {
                q.begin_conditional_render(ctxt, wait, per_region);
            },
            SamplesQueryParam::AnySamplesPassedQuery(ref q) => {
                q.begin_conditional_render(ctxt, wait, per_region);
            },
        }

    } else {
        TimeElapsedQuery::end_conditional_render(ctxt);
    }
}

fn sync_smooth(ctxt: &mut context::CommandContext,
               smooth: Option<Smooth>,
               primitive_type: PrimitiveType) -> Result<(), DrawError> {

    if let Some(smooth) = smooth {
        // check if smoothing is supported, it isn't on OpenGL ES
        if !(ctxt.version >= &Version(Api::Gl, 1, 0)) {
            return Err(DrawError::SmoothingNotSupported);
        }

        let hint = smooth.to_glenum();

        match primitive_type {
            // point
            PrimitiveType::Points =>
                return Err(DrawError::SmoothingNotSupported),

            // line
            PrimitiveType::LinesList | PrimitiveType::LinesListAdjacency |
            PrimitiveType::LineStrip | PrimitiveType::LineStripAdjacency |
            PrimitiveType::LineLoop => unsafe {
                if !ctxt.state.enabled_line_smooth {
                    ctxt.state.enabled_line_smooth = true;
                    ctxt.gl.Enable(gl::LINE_SMOOTH);
                }

                if ctxt.state.smooth.0 != hint {
                    ctxt.state.smooth.0 = hint;
                    ctxt.gl.Hint(gl::LINE_SMOOTH_HINT, hint);
                }
            },

            // polygon
            _ => unsafe {
                if !ctxt.state.enabled_polygon_smooth {
                    ctxt.state.enabled_polygon_smooth = true;
                    ctxt.gl.Enable(gl::POLYGON_SMOOTH);
                }

                if ctxt.state.smooth.1 != hint {
                    ctxt.state.smooth.1 = hint;
                    ctxt.gl.Hint(gl::POLYGON_SMOOTH_HINT, hint);
                }
            }
          }
        }
        else {
          match primitive_type {
            // point
            PrimitiveType::Points => (),

            // line
            PrimitiveType::LinesList | PrimitiveType::LinesListAdjacency |
            PrimitiveType::LineStrip | PrimitiveType::LineStripAdjacency |
            PrimitiveType::LineLoop => unsafe {
                if ctxt.state.enabled_line_smooth {
                    ctxt.state.enabled_line_smooth = false;
                    ctxt.gl.Disable(gl::LINE_SMOOTH);
                }
            },

            // polygon
            _ => unsafe {
                if ctxt.state.enabled_polygon_smooth {
                    ctxt.state.enabled_polygon_smooth = false;
                    ctxt.gl.Disable(gl::POLYGON_SMOOTH);
                }
            }
        }
    }

    Ok(())
}

fn sync_provoking_vertex(ctxt: &mut context::CommandContext, value: ProvokingVertex)
                         -> Result<(), DrawError>
{
    let value = match value {
        ProvokingVertex::LastVertex => gl::LAST_VERTEX_CONVENTION,
        ProvokingVertex::FirstVertex => gl::FIRST_VERTEX_CONVENTION,
    };

    if ctxt.state.provoking_vertex == value {
        return Ok(());
    }

    if ctxt.version >= &Version(Api::Gl, 3, 2) || ctxt.extensions.gl_arb_provoking_vertex {
        unsafe { ctxt.gl.ProvokingVertex(value); }
        ctxt.state.provoking_vertex = value;

    } else if ctxt.extensions.gl_ext_provoking_vertex {
        unsafe { ctxt.gl.ProvokingVertexEXT(value); }
        ctxt.state.provoking_vertex = value;

    } else {
        return Err(DrawError::ProvokingVertexNotSupported);
    }

    Ok(())
}

fn sync_primitive_bounding_box(ctxt: &mut context::CommandContext,
                               bb: &(Range<f32>, Range<f32>, Range<f32>, Range<f32>))
{
    let value = (bb.0.start, bb.1.start, bb.2.start, bb.3.start,
                 bb.0.end, bb.1.end, bb.2.end, bb.3.end);

    if ctxt.state.primitive_bounding_box == value {
        return;
    }

    if ctxt.version >= &Version(Api::GlEs, 3, 2) {
        unsafe { ctxt.gl.PrimitiveBoundingBox(value.0, value.1, value.2, value.3,
                                              value.4, value.5, value.6, value.7); }
        ctxt.state.primitive_bounding_box = value;

    } else if ctxt.extensions.gl_arb_es3_2_compatibility {
        unsafe { ctxt.gl.PrimitiveBoundingBoxARB(value.0, value.1, value.2, value.3,
                                                 value.4, value.5, value.6, value.7); }
        ctxt.state.primitive_bounding_box = value;

    } else if ctxt.extensions.gl_oes_primitive_bounding_box {
        unsafe { ctxt.gl.PrimitiveBoundingBoxOES(value.0, value.1, value.2, value.3,
                                                 value.4, value.5, value.6, value.7); }
        ctxt.state.primitive_bounding_box = value;

    } else if ctxt.extensions.gl_ext_primitive_bounding_box {
        unsafe { ctxt.gl.PrimitiveBoundingBoxEXT(value.0, value.1, value.2, value.3,
                                                 value.4, value.5, value.6, value.7); }
        ctxt.state.primitive_bounding_box = value;
    }
}
