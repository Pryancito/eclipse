use super::*;

/// The host-native build must keep the exact command line it has always
/// used. x86_64-on-x86_64 has been working for the whole life of the tree;
/// pointing it at a downloaded cross gcc to fix arm64 would be trading one
/// broken arch for another.
#[test]
fn el_host_nativo_no_toca_el_enlazador() {
    // A host Eclipse does not target at all (an ARMv7 builder, say) has no
    // native case to check; it must not fail the suite either.
    let Ok(host) = std::env::consts::ARCH.parse::<Arch>() else {
        return;
    };
    let rootfs = LinuxRootfs::new(host);
    assert!(
        !rootfs.needs_cross_linker(),
        "{} es el arch del host y no debería necesitar cross",
        host.name()
    );
    let env = rootfs.cross_build_env();
    assert_eq!(
        env,
        vec![("RUSTFLAGS".to_string(), STATIC_RUSTFLAGS.to_string())],
        "el caso nativo ha dejado de ser el de siempre"
    );
}

/// And every OTHER arch does need one, which is the actual bug: on this
/// x86_64 host `aarch64-unknown-linux-musl` was linked by `/usr/bin/ld`,
/// which rejects rustc's `--fix-cortex-a53-843419` outright.
#[test]
fn los_demas_arcos_si_necesitan_cross() {
    let host = std::env::consts::ARCH;
    for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64] {
        assert_eq!(
            LinuxRootfs::new(arch).needs_cross_linker(),
            arch.name() != host,
            "{} contra un host {host}",
            arch.name()
        );
    }
}

/// The cross case has to ADD the linker, not replace the flags: a tool
/// linked PIE would not be the static non-PIE ET_EXEC the Eclipse loader
/// and the busybox/apk base expect.
#[test]
fn el_enlazador_cruzado_no_pisa_el_relocation_model() {
    let rootfs = LinuxRootfs::new(Arch::Aarch64);
    let env = rootfs.cross_build_env_with(Some("/toolchain/bin/aarch64-linux-musl-gcc"));
    let rustflags = env
        .iter()
        .find(|(k, _)| k == "RUSTFLAGS")
        .map(|(_, v)| v.as_str())
        .expect("RUSTFLAGS");
    assert!(
        rustflags.contains("-C relocation-model=static"),
        "{rustflags:?} ha perdido el relocation-model"
    );
    assert!(
        rustflags.contains("-C linker=/toolchain/bin/aarch64-linux-musl-gcc"),
        "{rustflags:?} no lleva el enlazador cruzado"
    );
}

/// cc-rs reads `CC_<triple>` with the dashes turned into UNDERSCORES. With
/// the dashes left in, the variable is never read at all and a dependency
/// with a C build script goes back to the host compiler -- silently, which
/// is exactly the failure mode this batch is about.
#[test]
fn la_variable_de_cc_va_con_guiones_bajos() {
    let env = LinuxRootfs::new(Arch::Aarch64).cross_build_env_with(Some("/cc"));
    assert!(
        env.iter()
            .any(|(k, v)| k == "CC_aarch64_unknown_linux_musl" && v == "/cc"),
        "no está CC_aarch64_unknown_linux_musl: {env:?}"
    );
    assert!(
        !env.iter().any(|(k, _)| k.contains('-')),
        "una variable de entorno con guiones no la lee nadie: {env:?}"
    );
}

/// No `CC_…` at all in the native case: it would point host build scripts
/// at a compiler they must not use.
#[test]
fn sin_cross_no_se_fija_ningun_compilador() {
    let env = LinuxRootfs::new(Arch::X86_64).cross_build_env_with(None);
    assert!(
        !env.iter().any(|(k, _)| k.starts_with("CC")),
        "{env:?} fija un compilador sin hacer falta"
    );
}

/// Every arch, cross or native, links static. riscv64 is the one that
/// needs saying: its musl target spec has no `crt-static-default`, so
/// before this flag its `/sbin/init` was a dynamic binary that asked the
/// rootfs for `_Unwind_*` out of a `libgcc_s.so` nobody ships, exited 127
/// and left the arch unbootable.
#[test]
fn todos_los_arcos_enlazan_estatico() {
    for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64] {
        for cc in [None, Some("/toolchain/bin/cross-gcc")] {
            let env = LinuxRootfs::new(arch).cross_build_env_with(cc);
            let rustflags = env
                .iter()
                .find(|(k, _)| k == "RUSTFLAGS")
                .map(|(_, v)| v.as_str())
                .expect("RUSTFLAGS");
            assert!(
                rustflags.contains("-C target-feature=+crt-static"),
                "{} se enlazaria dinamico: {rustflags:?}",
                arch.name()
            );
        }
    }
}

/// The recurrence, not the instance. The same `RUSTFLAGS` line was copied
/// into five build functions, so the sixth tool would have copied it again
/// and arrived with the same broken linker. Nothing may hand `RUSTFLAGS`
/// to a command directly any more: the value comes from
/// `cross_build_env`, which is the only thing that knows whether a cross
/// linker is needed.
#[test]
fn ninguna_herramienta_arma_su_propio_rustflags() {
    let src = include_str!("mod.rs");
    let culprits: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("env(\"RUSTFLAGS\""))
        .collect();
    assert!(
        culprits.is_empty(),
        "estas líneas le pasan RUSTFLAGS a un comando a mano en vez de \
         usar cross_build_env: {culprits:?}"
    );
}
