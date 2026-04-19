//! Tests ejecutables en el host (`std`) para políticas numéricas del kernel.
//! Ver `policy` y `tests/all_kernel_limits.rs`.

pub mod policy;
pub mod extended;

/// Marca el crate para enlaces desde documentación interna.
pub const CRATE_MARKER: u32 = 0x4b_48_54_53; // 'KHTS'
