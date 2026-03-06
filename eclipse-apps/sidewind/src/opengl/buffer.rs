//! Fixed-size vertex and index buffer objects.
//!
//! Avoids heap allocation by using const-generic arrays — compatible with
//! a bump allocator or stack-only environments.

use super::types::{GLenum, GLfloat, GL_FLOAT};

// ─────────────────────────────────────────────────────────────────────────────
// Vertex attribute descriptor
// ─────────────────────────────────────────────────────────────────────────────

/// Describes one attribute in a vertex (e.g. position, normal, uv).
#[derive(Debug, Clone, Copy)]
pub struct VertexAttrib {
    /// Number of components (1–4).
    pub components: u8,
    /// Component type — currently only `GL_FLOAT` is supported.
    pub type_: GLenum,
    /// Byte offset of this attribute within one vertex.
    pub offset: u32,
    /// Total bytes per vertex (stride).
    pub stride: u32,
}

impl VertexAttrib {
    /// `components` floats starting at byte `offset`, stride = full vertex size.
    pub const fn float(components: u8, offset: u32, stride: u32) -> Self {
        Self { components, type_: GL_FLOAT, offset, stride }
    }

    /// Size of the attribute data in bytes.
    pub const fn byte_size(&self) -> u32 {
        match self.type_ {
            GL_FLOAT => self.components as u32 * 4,
            _ => 0,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VertexBuffer
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed-capacity vertex buffer.
///
/// `N` is the maximum number of **floats** (not vertices) stored.
pub struct VertexBuffer<const N: usize> {
    data: [GLfloat; N],
    len:  usize,
}

impl<const N: usize> VertexBuffer<N> {
    /// Create an empty buffer.
    pub const fn new() -> Self {
        Self { data: [0.0; N], len: 0 }
    }

    /// Upload (copy) vertex data into the buffer. Returns `false` if `src` is
    /// too large.
    pub fn upload(&mut self, src: &[GLfloat]) -> bool {
        if src.len() > N { return false; }
        self.data[..src.len()].copy_from_slice(src);
        self.len = src.len();
        true
    }

    /// Number of floats stored.
    #[inline] pub fn len(&self) -> usize { self.len }
    #[inline] pub fn is_empty(&self) -> bool { self.len == 0 }

    /// Raw float slice.
    #[inline] pub fn as_slice(&self) -> &[GLfloat] { &self.data[..self.len] }

    /// Read a single attribute for vertex `idx` according to `attrib`.
    ///
    /// Returns a slice of `attrib.components` floats, or `None` if out of
    /// bounds.
    pub fn read_attrib<'a>(&'a self, idx: usize, attrib: &VertexAttrib) -> Option<&'a [GLfloat]> {
        let stride_f  = (attrib.stride  / 4) as usize;
        let offset_f  = (attrib.offset  / 4) as usize;
        let count     = attrib.components as usize;
        let start     = idx * stride_f + offset_f;
        let end       = start + count;
        if end > self.len { return None; }
        Some(&self.data[start..end])
    }

    /// Number of complete vertices that fit given a stride (bytes).
    pub fn vertex_count(&self, stride_bytes: u32) -> usize {
        if stride_bytes == 0 { return 0; }
        let stride_f = (stride_bytes / 4) as usize;
        self.len / stride_f
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IndexBuffer
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed-capacity index buffer (u32 indices).
pub struct IndexBuffer<const N: usize> {
    data: [u32; N],
    len:  usize,
}

impl<const N: usize> IndexBuffer<N> {
    pub const fn new() -> Self { Self { data: [0; N], len: 0 } }

    pub fn upload(&mut self, src: &[u32]) -> bool {
        if src.len() > N { return false; }
        self.data[..src.len()].copy_from_slice(src);
        self.len = src.len();
        true
    }

    #[inline] pub fn len(&self) -> usize { self.len }
    #[inline] pub fn is_empty(&self) -> bool { self.len == 0 }
    #[inline] pub fn as_slice(&self) -> &[u32] { &self.data[..self.len] }
}
