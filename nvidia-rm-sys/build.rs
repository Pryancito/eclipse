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
}
