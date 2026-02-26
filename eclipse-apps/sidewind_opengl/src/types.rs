//! GL type aliases and symbolic constants.
//!
//! Mirrors the subset of OpenGL types needed by the software rasterizer.

#![allow(non_camel_case_types)]

// ── Primitive type aliases ──────────────────────────────────────────────────
pub type GLenum    = u32;
pub type GLuint    = u32;
pub type GLint     = i32;
pub type GLsizei   = i32;
pub type GLfloat   = f32;
pub type GLboolean = u8;
pub type GLubyte   = u8;
pub type GLbitfield = u32;

// ── Boolean ─────────────────────────────────────────────────────────────────
pub const GL_TRUE:  GLboolean = 1;
pub const GL_FALSE: GLboolean = 0;

// ── Error codes ──────────────────────────────────────────────────────────────
pub const GL_NO_ERROR:          GLenum = 0x0000;
pub const GL_INVALID_ENUM:      GLenum = 0x0500;
pub const GL_INVALID_VALUE:     GLenum = 0x0501;
pub const GL_INVALID_OPERATION: GLenum = 0x0502;
pub const GL_OUT_OF_MEMORY:     GLenum = 0x0505;

// ── Clear bits ───────────────────────────────────────────────────────────────
pub const GL_COLOR_BUFFER_BIT: GLbitfield = 0x4000;
pub const GL_DEPTH_BUFFER_BIT: GLbitfield = 0x0100;

// ── Primitive types ───────────────────────────────────────────────────────────
pub const GL_TRIANGLES:      GLenum = 0x0004;
pub const GL_TRIANGLE_STRIP: GLenum = 0x0005;
pub const GL_TRIANGLE_FAN:   GLenum = 0x0006;
pub const GL_LINES:          GLenum = 0x0001;
pub const GL_POINTS:         GLenum = 0x0000;

// ── Data types ────────────────────────────────────────────────────────────────
pub const GL_FLOAT:          GLenum = 0x1406;
pub const GL_UNSIGNED_BYTE:  GLenum = 0x1401;
pub const GL_UNSIGNED_INT:   GLenum = 0x1405;

// ── Texture formats ───────────────────────────────────────────────────────────
pub const GL_RGBA:            GLenum = 0x1908;
pub const GL_RGB:             GLenum = 0x1907;
pub const GL_TEXTURE_2D:      GLenum = 0x0DE1;
pub const GL_NEAREST:         GLenum = 0x2600;
pub const GL_LINEAR:          GLenum = 0x2601;
pub const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
pub const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
pub const GL_TEXTURE_WRAP_S:  GLenum = 0x2802;
pub const GL_TEXTURE_WRAP_T:  GLenum = 0x2803;
pub const GL_CLAMP_TO_EDGE:   GLenum = 0x812F;
pub const GL_REPEAT:          GLenum = 0x2901;

// ── Blend factors ─────────────────────────────────────────────────────────────
pub const GL_ONE:                    GLenum = 1;
pub const GL_ZERO:                   GLenum = 0;
pub const GL_SRC_ALPHA:              GLenum = 0x0302;
pub const GL_ONE_MINUS_SRC_ALPHA:    GLenum = 0x0303;

// ── Capabilities ──────────────────────────────────────────────────────────────
pub const GL_BLEND:        GLenum = 0x0BE2;
pub const GL_DEPTH_TEST:   GLenum = 0x0B71;
pub const GL_CULL_FACE:    GLenum = 0x0B44;
pub const GL_SCISSOR_TEST: GLenum = 0x0C11;

// ── Winding ───────────────────────────────────────────────────────────────────
pub const GL_CW:  GLenum = 0x0900;
pub const GL_CCW: GLenum = 0x0901;

/// Simple GL error accumulator (last-error model, like real GL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlError(pub GLenum);

impl GlError {
    pub const NONE: Self = Self(GL_NO_ERROR);
    pub fn is_ok(self) -> bool { self.0 == GL_NO_ERROR }
}
