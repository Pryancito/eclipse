//! GPU buffer types for `sidewind_wgpu`.

/// Bitmask describing how a `Buffer` may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferUsages(pub u32);

impl BufferUsages {
    pub const MAP_READ:    Self = Self(1 << 0);
    pub const MAP_WRITE:   Self = Self(1 << 1);
    pub const COPY_SRC:    Self = Self(1 << 2);
    pub const COPY_DST:    Self = Self(1 << 3);
    pub const INDEX:       Self = Self(1 << 4);
    pub const VERTEX:      Self = Self(1 << 5);
    pub const UNIFORM:     Self = Self(1 << 6);
    pub const STORAGE:     Self = Self(1 << 7);
    pub const INDIRECT:    Self = Self(1 << 8);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for BufferUsages {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// How the buffer will be mapped for host access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMode {
    Read,
    Write,
}

/// Descriptor used to create a `Buffer`.
#[derive(Debug, Clone, Copy)]
pub struct BufferDescriptor {
    pub size: u64,
    pub usage: BufferUsages,
    /// If `true`, the buffer starts mapped for CPU write access.
    pub mapped_at_creation: bool,
}

/// A GPU-side memory buffer.
///
/// On Eclipse OS, buffers are backed by:
/// * virgl backing memory (`virgl_alloc_backing`) for VirtIO contexts, or
/// * a VRAM offset (from `sidewind_nvidia::opengl::GlKernelContext::alloc_surface`)
///   for NVIDIA contexts, or
/// * a plain host-side byte allocation for software/staging buffers.
pub struct Buffer {
    /// Byte size of the allocation.
    pub(crate) size: u64,
    pub(crate) usage: BufferUsages,
    /// Virtual address of host-visible data.  Zero if not host-mapped.
    pub(crate) host_vaddr: u64,
    /// virgl resource ID (0 = not a virgl resource).
    pub(crate) virgl_resource_id: u32,
    /// Byte offset into NVIDIA VRAM (if using NVIDIA backend).
    pub(crate) vram_offset: Option<u64>,
}

impl Buffer {
    pub(crate) fn new_empty(descriptor: &BufferDescriptor) -> Self {
        Self {
            size: descriptor.size,
            usage: descriptor.usage,
            host_vaddr: 0,
            virgl_resource_id: 0,
            vram_offset: None,
        }
    }

    /// Byte size of the buffer.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return a host-visible slice of the buffer data, or `None` if not
    /// mapped.
    ///
    /// # Safety
    /// The caller must not access more than `size()` bytes.
    pub unsafe fn as_slice(&self) -> Option<&[u8]> {
        if self.host_vaddr >= 0x1000 {
            Some(core::slice::from_raw_parts(
                self.host_vaddr as *const u8,
                self.size as usize,
            ))
        } else {
            None
        }
    }

    /// Return a host-visible mutable slice, or `None` if not mapped.
    ///
    /// # Safety
    /// The caller must not access more than `size()` bytes.
    pub unsafe fn as_mut_slice(&self) -> Option<&mut [u8]> {
        if self.host_vaddr >= 0x1000 {
            Some(core::slice::from_raw_parts_mut(
                self.host_vaddr as *mut u8,
                self.size as usize,
            ))
        } else {
            None
        }
    }

    /// Write `data` into the buffer starting at `offset`.
    ///
    /// Panics in debug mode if `offset + data.len() > size`.
    pub fn write(&self, offset: u64, data: &[u8]) {
        let end = offset as usize + data.len();
        debug_assert!(end <= self.size as usize, "buffer write out of bounds");
        if self.host_vaddr < 0x1000 { return; }
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.host_vaddr + offset) as *mut u8,
                data.len(),
            );
        }
    }
}
