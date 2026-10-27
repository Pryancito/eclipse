//! nvidia-modeset (nvkms): the modeset, DisplayPort and HDMI code Linux's
//! `nvidia-modeset.ko` is built from, compiled by `build.rs` from the same
//! submodule as the RM. See `docs/README-nvkms-port.md`.
//!
//! Not active yet: its OS hooks (`vendor/eclipse_nvkms_os.c`) only make it
//! link, and nothing loads it. Eclipse will drive it the way `nvidia-drm` does
//! in Linux, through the kapi function table.

#[cfg(target_os = "none")]
extern "C" {
    /// `nvKmsKapiGetFunctionsTable` (kapi/interface/nvkms-kapi.h). Only its
    /// address is taken here.
    fn nvKmsKapiGetFunctionsTable(funcs_table: *mut core::ffi::c_void) -> u8;
}

/// Whether nvkms is in this kernel image.
///
/// Takes the address of kapi's entry point without calling it. That reference
/// is what makes the linker pull nvkms out of its archive -- and with it every
/// object kapi reaches -- so an unresolved symbol anywhere in nvkms fails the
/// kernel link instead of hiding in an archive nobody looked into. Hosted
/// builds compile no NVIDIA C and answer `false`.
pub fn linked() -> bool {
    #[cfg(target_os = "none")]
    {
        let entry: unsafe extern "C" fn(*mut core::ffi::c_void) -> u8 = nvKmsKapiGetFunctionsTable;
        core::hint::black_box(entry as usize) != 0
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}
