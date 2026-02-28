//! Error types for `sidewind_wgpu`.

/// Errors returned by `sidewind_wgpu` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WgpuError {
    /// No compatible GPU backend was found.
    NoAdapterFound,
    /// The requested operation is not supported by the active backend.
    Unsupported,
    /// A kernel syscall or hardware operation failed.
    DeviceLost,
    /// Memory allocation failed (VRAM or host).
    OutOfMemory,
    /// The surface is no longer valid (e.g. display was disconnected).
    SurfaceLost,
    /// An invalid argument was supplied to an API call.
    InvalidArgument,
}

impl core::fmt::Display for WgpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoAdapterFound  => f.write_str("no GPU adapter found"),
            Self::Unsupported     => f.write_str("operation not supported by backend"),
            Self::DeviceLost      => f.write_str("GPU device lost"),
            Self::OutOfMemory     => f.write_str("out of GPU memory"),
            Self::SurfaceLost     => f.write_str("display surface lost"),
            Self::InvalidArgument => f.write_str("invalid argument"),
        }
    }
}
