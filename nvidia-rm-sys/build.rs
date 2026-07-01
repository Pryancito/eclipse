// Bring-up smoke test for the C-compile + FFI-link pipeline this crate will
// use to vendor NVIDIA's real open-gpu-kernel-modules source (MIT-licensed,
// src/nvidia/) unmodified into Eclipse. `vendor/smoketest.c` is NOT NVIDIA
// code -- it's a minimal hand-written translation unit that proves two
// things before any real vendoring work happens:
//   1. A C object file can be compiled with the exact freestanding flags
//      Eclipse's kernel target (zCore/x86_64.json: disable-redzone,
//      code-model=kernel, no MMX, panic=abort) requires, and linked into
//      the kernel binary.
//   2. C code can call back into a Rust-exported function -- the same
//      shape NVIDIA's RM uses to call into an os-interface.h implementation.
// Replace this file with real vendored NVIDIA sources once both are proven.
fn main() {
    let mut build = cc::Build::new();
    build.file("vendor/smoketest.c").flag_if_supported("-Wall");

    // Only apply the freestanding kernel flags when actually building for
    // Eclipse's no_std kernel target (CARGO_CFG_TARGET_OS reflects the
    // JSON target spec's "os" field, "none", regardless of how cc's own
    // TARGET-string parsing handles a non-triple custom target name).
    let building_for_kernel =
        std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none");
    if building_for_kernel {
        // cc's own TARGET-string auto-detection rejects Cargo's custom JSON
        // target name ("x86_64" -- a single component, not a full triple):
        // "target `x86_64` only had a single component (at least two
        // required)". Override explicitly with the JSON's own
        // "llvm-target" value so cc's Unix-like defaults apply instead of
        // erroring; this does not change which compiler binary gets
        // invoked (still the host's cc/gcc), only cc's internal flag
        // heuristics.
        build.target("x86_64-unknown-none");
        build
            .flag("-ffreestanding")
            .flag("-fno-builtin")
            .flag("-fno-stack-protector")
            .flag("-fno-asynchronous-unwind-tables")
            .flag("-mno-red-zone")
            .flag("-mcmodel=kernel")
            // gcc on most distros defaults to PIE/PIC; -mcmodel=kernel is
            // incompatible with PIC ("code model kernel does not support
            // PIC mode") and fails to compile without these.
            .flag("-fno-pic")
            .flag("-fno-pie")
            // Match zCore/x86_64.json's `"features": "-mmx,+sse2"` exactly:
            // MMX off, SSE2 left enabled (it's the x86_64 baseline anyway).
            // Do NOT also disable SSE/SSE2 here -- that would diverge from
            // the Rust side's actual ABI assumptions.
            .flag("-mno-mmx")
            .flag("-nostdlib");
    }

    build.compile("nvrm_smoketest");
    println!("cargo:rerun-if-changed=vendor/smoketest.c");

    build_first_real_nvidia_file();
}

/// First real (not hand-written) NVIDIA source: src/nvidia/src/libraries/
/// fnv_hash/fnv_hash.c -- picked for having the smallest #include list of
/// any real .c file surveyed so far (message_queue_cpu.c, the file we
/// actually want for GSP RPC, pulls in 16+ headers including ones NVIDIA's
/// own build generates at build time -- too much to take on before proving
/// the include-path plumbing itself works).
///
/// Only runs if the submodule is actually checked out
/// (`git submodule update --init nvidia-rm-sys/vendor/open-gpu-kernel-modules`
/// from a machine with real GitHub access -- blocked in the sandbox this
/// was authored in). Skips silently otherwise so the crate still builds
/// without it, same as the smoke test above always has.
fn build_first_real_nvidia_file() {
    let vendor = std::path::Path::new("vendor/open-gpu-kernel-modules");
    let target_file = vendor.join("src/nvidia/src/libraries/fnv_hash/fnv_hash.c");
    if !target_file.exists() {
        println!(
            "cargo:warning=nvidia-rm-sys: submodule not checked out ({} missing) -- skipping real NVIDIA source, only the hand-written smoke test compiled this run",
            target_file.display()
        );
        return;
    }

    let nvidia = vendor.join("src/nvidia");
    // SRC_COMMON below is INFERRED as <submodule>/src/common (matches every
    // one of these paths existing under a "common" sibling of "nvidia" --
    // e.g. sdk/nvidia/inc, mbedtls/... -- but the exact `SRC_COMMON =`
    // assignment itself wasn't tracked down across kernel-open/Makefile,
    // src/nvidia/Makefile, and utils.mk). Verify/fix once this actually
    // runs against the checked-out submodule and reports real path errors.
    let common = vendor.join("src/common");

    // Transcribed from src/nvidia/Makefile's `CFLAGS += -I ...` lines, in
    // the same order, with $(SRC_COMMON) substituted.
    let include_dirs: [std::path::PathBuf; 25] = [
        nvidia.join("kernel/inc"),
        nvidia.join("interface"),
        common.join("sdk/nvidia/inc"),
        common.join("sdk/nvidia/inc/hw"),
        nvidia.join("arch/nvalloc/common/inc"),
        nvidia.join("arch/nvalloc/common/inc/gsp"),
        nvidia.join("arch/nvalloc/common/inc/deprecated"),
        nvidia.join("arch/nvalloc/unix/include"),
        nvidia.join("inc"),
        nvidia.join("inc/os"),
        common.join("shared/inc"),
        common.join("inc"),
        common.join("uproc/os/libos-v2.0.0/include"),
        common.join("uproc/os/common/include"),
        common.join("inc/swref"),
        common.join("inc/swref/published"),
        nvidia.join("generated"),
        common.join("nvswitch/kernel/inc"),
        common.join("nvswitch/interface"),
        common.join("nvswitch/common/inc"),
        common.join("inc/displayport"),
        common.join("nvlink/interface"),
        common.join("nvlink/inband/interface"),
        nvidia.join("inc/libraries"),
        nvidia.join("inc/kernel"),
    ];

    let mut build = cc::Build::new();
    build.file(&target_file);
    for dir in &include_dirs {
        build.include(dir);
    }
    // -include $(SRC_COMMON)/sdk/nvidia/inc/cpuopsys.h from the real
    // Makefile: force-included ahead of every translation unit, defines
    // the platform macros (NV_LINUX / NVCPU_* / etc.) most NVIDIA headers
    // key off of instead of detecting the compiler target themselves.
    build.flag(&format!(
        "-include{}",
        common.join("sdk/nvidia/inc/cpuopsys.h").display()
    ));

    let building_for_kernel = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none");
    if building_for_kernel {
        build.target("x86_64-unknown-none");
        build
            .flag("-ffreestanding")
            .flag("-fno-builtin")
            .flag("-fno-stack-protector")
            .flag("-fno-asynchronous-unwind-tables")
            .flag("-mno-red-zone")
            .flag("-mcmodel=kernel")
            .flag("-fno-pic")
            .flag("-fno-pie")
            .flag("-mno-mmx")
            .flag("-nostdlib");
    }

    build.compile("nvrm_fnv_hash");
    println!("cargo:rerun-if-changed={}", target_file.display());
}
