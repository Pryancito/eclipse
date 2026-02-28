//! Texture types for `sidewind_wgpu`.

/// Dimensionality of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureDimension {
    D1,
    D2,
    D3,
}

/// Pixel format.  Only the formats that Eclipse OS framebuffers natively
/// support are listed; additional variants can be added as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureFormat {
    /// 4 bytes per pixel: B8 G8 R8 A8 (native VirtIO / NVIDIA framebuffer order).
    Bgra8Unorm,
    /// 4 bytes per pixel: R8 G8 B8 A8.
    Rgba8Unorm,
    /// 4 bytes per pixel: B8 G8 R8 A8 with sRGB gamma.
    Bgra8UnormSrgb,
    /// 4 bytes per pixel: R8 G8 B8 A8 with sRGB gamma.
    Rgba8UnormSrgb,
}

impl TextureFormat {
    /// Bytes per pixel for this format.
    #[inline]
    pub const fn bytes_per_pixel(self) -> u32 {
        4 // all current formats are 32-bit
    }
}

/// Bitmask of allowed texture usages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureUsages(u32);

impl TextureUsages {
    pub const COPY_SRC: Self = Self(1 << 0);
    pub const COPY_DST: Self = Self(1 << 1);
    pub const TEXTURE_BINDING: Self = Self(1 << 2);
    pub const STORAGE_BINDING: Self = Self(1 << 3);
    pub const RENDER_ATTACHMENT: Self = Self(1 << 4);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for TextureUsages {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 3-D extent (width × height × depth_or_array_layers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent3d {
    pub width: u32,
    pub height: u32,
    pub depth_or_array_layers: u32,
}

impl Extent3d {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height, depth_or_array_layers: 1 }
    }
}

/// Descriptor used to create a texture.
#[derive(Debug, Clone, Copy)]
pub struct TextureDescriptor {
    pub size: Extent3d,
    pub mip_level_count: u32,
    pub sample_count: u32,
    pub dimension: TextureDimension,
    pub format: TextureFormat,
    pub usage: TextureUsages,
}

impl Default for TextureDescriptor {
    fn default() -> Self {
        Self {
            size: Extent3d::new(1, 1),
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT,
        }
    }
}

/// A GPU texture.
///
/// On Eclipse OS textures are backed by either:
/// * a VirtIO virgl resource (when `backing_vaddr != 0`), or
/// * a region of NVIDIA VRAM (when `vram_offset` is `Some`), or
/// * a plain host-side allocation (for staging / software fallback).
pub struct Texture {
    pub(crate) descriptor: TextureDescriptor,
    /// Virtual address of the host-visible backing memory.
    /// For virgl resources this is the result of `virgl_alloc_backing`.
    /// For software fallback this is a regular allocation.
    /// Zero means the texture has no host-visible backing.
    pub(crate) backing_vaddr: u64,
    /// Byte size of the backing allocation.
    pub(crate) backing_size: usize,
    /// VirtIO virgl resource ID (0 = not a virgl resource).
    pub(crate) virgl_resource_id: u32,
    /// Offset into NVIDIA VRAM (if on NVIDIA backend).
    pub(crate) vram_offset: Option<u64>,
}

impl Texture {
    pub(crate) fn new_empty(descriptor: TextureDescriptor) -> Self {
        Self {
            descriptor,
            backing_vaddr: 0,
            backing_size: 0,
            virgl_resource_id: 0,
            vram_offset: None,
        }
    }

    /// Width of the texture in pixels.
    pub fn width(&self) -> u32 {
        self.descriptor.size.width
    }

    /// Height of the texture in pixels.
    pub fn height(&self) -> u32 {
        self.descriptor.size.height
    }

    /// Pixel format.
    pub fn format(&self) -> TextureFormat {
        self.descriptor.format
    }

    /// Row pitch in bytes.
    pub fn row_pitch(&self) -> u32 {
        self.descriptor.size.width * self.descriptor.format.bytes_per_pixel()
    }

    /// Byte size of the full texture at mip 0.
    pub fn byte_size(&self) -> usize {
        (self.row_pitch() as usize).saturating_mul(self.descriptor.size.height as usize)
    }

    /// Return a raw pointer to the host-visible pixel data, or `None` if the
    /// texture has no host-visible backing.
    ///
    /// # Safety
    /// The caller must not write past `byte_size()` bytes.
    pub unsafe fn as_mut_ptr(&self) -> Option<*mut u32> {
        if self.backing_vaddr >= 0x1000 {
            Some(self.backing_vaddr as *mut u32)
        } else {
            None
        }
    }

    /// Fill the entire texture with `color_bgra` (packed 0xAARRGGBB).
    ///
    /// This is a CPU-side blit and is always available regardless of backend.
    pub fn clear(&self, color_bgra: u32) {
        let Some(ptr) = (unsafe { self.as_mut_ptr() }) else { return };
        let pixels = (self.descriptor.size.width as usize)
            .saturating_mul(self.descriptor.size.height as usize);
        for i in 0..pixels {
            unsafe { core::ptr::write_volatile(ptr.add(i), color_bgra); }
        }
    }
}
