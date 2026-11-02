//! The architecture name, which is also a path and a target spec.
//!
//! `Arch::name` is not a label. It is the directory under `archs/` where the
//! downloaded toolchain lands, the directory under `target/` where the kernel
//! is built, the file name of the target spec JSON (`zCore/x86_64.json`) and
//! the prefix of the musl cross toolchain tarball. `FromStr` is what `clap`
//! calls on `--arch`, and the error it returns is the only thing the user is
//! shown when they get it wrong. None of it had a test.

use super::*;

/// Every name parses back to the arch it names. The three strings are written
/// out rather than taken from `name()`: a test that derives both sides from
/// the thing it is testing goes on passing when that thing changes, and these
/// three strings are a public interface -- they are what `--arch` takes, what
/// `zCore/<name>.json` is called, and what the CI matrix says.
#[test]
fn every_architecture_name_parses_back_to_itself() {
    for name in ["riscv64", "x86_64", "aarch64"] {
        let arch = Arch::from_str(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(arch.name(), name);
    }
}

/// And the name is spelled the way the rest of the build spells it. Getting
/// one of these wrong does not fail to compile: it builds into a directory
/// nobody looks in, or asks for a target spec that is not there.
#[test]
fn the_name_is_the_one_the_paths_and_the_target_spec_use() {
    assert_eq!(Arch::Riscv64.name(), "riscv64");
    assert_eq!(Arch::X86_64.name(), "x86_64");
    assert_eq!(Arch::Aarch64.name(), "aarch64");
    assert_ne!(Arch::X86_64.name(), "amd64");
    assert_ne!(Arch::Aarch64.name(), "arm64");
}

/// The two directories each arch gets are named after it and are not the same
/// one: `origin` holds what was downloaded and survives a clean, `target`
/// holds what the build generated.
#[test]
fn the_download_directory_and_the_build_directory_are_both_named_after_the_arch() {
    for arch in [Arch::Riscv64, Arch::X86_64, Arch::Aarch64] {
        let origin = arch.origin();
        let target = arch.target();
        assert!(origin.ends_with(arch.name()), "{}", origin.display());
        assert!(target.ends_with(arch.name()), "{}", target.display());
        assert_ne!(
            origin, target,
            "what was downloaded and what was built must not share a directory"
        );
        assert!(origin.is_absolute(), "{}", origin.display());
        assert!(target.is_absolute(), "{}", target.display());
    }
}

/// `clap` hands over whatever the user typed, so the case they typed it in is
/// not the build's business.
#[test]
fn an_architecture_is_parsed_whatever_case_it_is_typed_in() {
    for typed in ["X86_64", "x86_64"] {
        assert_eq!(
            Arch::from_str(typed)
                .unwrap_or_else(|e| panic!("{typed}: {e}"))
                .name(),
            "x86_64"
        );
    }
    assert_eq!(
        Arch::from_str("AArch64")
            .unwrap_or_else(|e| panic!("{e}"))
            .name(),
        "aarch64"
    );
}

/// An unknown architecture is refused, and the refusal names both the thing
/// being parsed and what the user actually typed. That message is the whole of
/// what they get: `clap` prints it and stops.
#[test]
fn an_unknown_architecture_names_itself_and_what_was_typed() {
    // `expect_err` would want `Arch: Debug`, which it has no other reason to
    // be, so the refusal is taken apart by hand.
    let e = match Arch::from_str("mips64") {
        Ok(a) => panic!("mips64 parsed as {}", a.name()),
        Err(e) => e,
    };
    match &e {
        XError::EnumParse { type_name, value } => {
            assert_eq!(*type_name, "Arch");
            assert_eq!(value, "mips64");
        }
    }
    let shown = e.to_string();
    assert!(shown.contains("Arch"), "{shown}");
    assert!(shown.contains("mips64"), "{shown}");
}

/// The empty string is not an architecture either. It is what a bare `--arch`
/// with nothing after it leaves behind, and the message has to name the
/// emptiness rather than come out looking truncated.
#[test]
fn an_empty_architecture_is_refused_too() {
    let e = match Arch::from_str("") {
        Ok(a) => panic!("the empty string parsed as {}", a.name()),
        Err(e) => e,
    };
    assert_eq!(e.to_string(), "Parse Arch from  failed.");
}

// ---------------------------------------------------------------------------
// The cross compiler check.
//
// Until 2026-11-02 `linux_musl_cross` asked one question about the toolchain:
// does the directory exist. Whatever was inside went to busybox's makefile as
// `CROSS_COMPILE`, and when it was not a compiler this host could execute,
// `/bin/sh` failed to `execve` it, read it as a shell script instead and
// reported the ELF header as a syntax error at `applets/usage.c`. None of
// these tests can download anything, so they test the two halves that decide
// the message: reading the ELF identification, and the wording that comes out.
// ---------------------------------------------------------------------------

/// The first bytes of the `x86_64-linux-musl-gcc` that this build downloads,
/// as published at `YdrMaster/zCore` `musl-cache` (sha256
/// `c5d410d9…`, read 2026-11-02): `EI_CLASS` 1 and `e_machine` 3, i.e. a
/// **32-bit i386** binary, despite the `x86_64-` in its name. That is not a
/// corruption, it is what the tarball has always held -- so a host that
/// cannot run 32-bit x86 binaries cannot use this toolchain, and no amount of
/// re-downloading will help it.
const MUSL_GCC_HEAD: [u8; 20] = [
    0x7f, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 3, 0,
];

#[test]
fn the_published_cross_compiler_is_read_as_a_32_bit_i386_binary() {
    let elf = ElfId::parse(&MUSL_GCC_HEAD).unwrap();
    assert_eq!(elf.class, 1);
    assert_eq!(elf.machine, 3);
    assert_eq!(elf.width(), "32-bit");
    assert_eq!(elf.machine_name(), "i386");
}

/// A 64-bit x86 compiler reads as one, so the check does not call every
/// toolchain 32-bit.
#[test]
fn a_64_bit_x86_compiler_reads_as_one() {
    let mut head = MUSL_GCC_HEAD;
    head[4] = 2; // ELFCLASS64
    head[18] = 0x3e; // EM_X86_64
    let elf = ElfId::parse(&head).unwrap();
    assert_eq!(elf.width(), "64-bit");
    assert_eq!(elf.machine_name(), "x86-64");
}

/// An extraction that died part way through leaves a file too short to say
/// anything about. That is the case worth re-downloading, and the one the old
/// code passed straight to `make`.
#[test]
fn a_file_too_short_to_hold_a_header_is_not_identified() {
    for len in 0..20 {
        let e = ElfId::parse(&MUSL_GCC_HEAD[..len]).unwrap_err();
        assert!(e.contains(&len.to_string()), "{len}: {e}");
        assert!(e.contains("ELF header"), "{len}: {e}");
    }
    // Twenty bytes is enough: `e_machine` ends at offset 20 in both widths.
    assert!(ElfId::parse(&MUSL_GCC_HEAD[..20]).is_ok());
}

/// And a file that is not an ELF at all -- an HTML error page saved under the
/// compiler's name, say -- is refused by the magic rather than read as a
/// header full of rubbish.
#[test]
fn something_that_is_not_an_elf_file_is_refused_by_the_magic() {
    let html = b"<!DOCTYPE html><html><head><title>404";
    let e = ElfId::parse(html).unwrap_err();
    assert!(e.contains("ELF magic"), "{e}");
    // A near miss is still a miss.
    let mut almost = MUSL_GCC_HEAD;
    almost[3] = b'G';
    assert!(ElfId::parse(&almost).unwrap_err().contains("ELF magic"));
}

/// The message for a half-written toolchain names the file and says to delete
/// it, because that is the one the user can act on.
#[test]
fn the_message_for_a_broken_toolchain_names_the_file_and_the_way_out() {
    let gcc = PathBuf::from("/root/eclipse/ignored/target/x86_64")
        .join("x86_64-linux-musl-cross/bin/x86_64-linux-musl-gcc");
    let shown = Compiler::Broken("not there".into()).explain(&gcc, "MUSL_CROSS_HOME");
    assert!(shown.contains("x86_64-linux-musl-gcc"), "{shown}");
    assert!(shown.contains("not there"), "{shown}");
    assert!(shown.contains("Delete"), "{shown}");
}

/// The message for a compiler this host cannot execute says so in those words,
/// names the width and the machine it is actually for, and does **not** tell
/// anybody to download it again -- which was the first thing anybody tried.
#[test]
fn the_message_for_an_unrunnable_toolchain_says_downloading_it_again_will_not_help() {
    let gcc = PathBuf::from("bin/x86_64-linux-musl-gcc");
    let shown = Compiler::Unrunnable {
        elf: ElfId::parse(&MUSL_GCC_HEAD).unwrap(),
        why: "Exec format error (os error 8)".into(),
    }
    .explain(&gcc, "MUSL_CROSS_HOME");
    assert!(shown.contains("32-bit"), "{shown}");
    assert!(shown.contains("i386"), "{shown}");
    assert!(shown.contains("Exec format error"), "{shown}");
    assert!(shown.contains("will not change that"), "{shown}");
    assert!(shown.contains("MUSL_CROSS_HOME"), "{shown}");
}

/// `check_compiler` answers for a path that is not there without touching the
/// network or the filesystem it is not allowed to touch, and the answer is the
/// one that triggers a re-download rather than the one that stops the build.
#[test]
fn a_compiler_that_is_not_there_is_broken_rather_than_unrunnable() {
    let missing = PathBuf::from("/nonexistent-8b1f/bin/x86_64-linux-musl-gcc");
    match check_compiler(&missing) {
        Compiler::Broken(why) => assert!(why.contains("not there"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// A file that exists but holds rubbish is `Broken` too: the check never gets
/// as far as running it, so no test here ever executes a downloaded binary.
#[test]
fn a_file_of_rubbish_is_broken_without_being_run() {
    let path = std::env::temp_dir().join(format!("xtask-arch-test-{}", std::process::id()));
    std::fs::write(&path, b"#!/bin/sh\necho not a compiler\n").unwrap();
    let verdict = check_compiler(&path);
    std::fs::remove_file(&path).unwrap();
    match verdict {
        Compiler::Broken(why) => assert!(why.contains("ELF magic"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// The compiler the callers run is the one the check looks at: `bin/` under
/// the toolchain directory, named after the arch. If those two ever drift the
/// check would be passing a file nobody uses.
#[test]
fn the_checked_compiler_is_the_one_the_callers_run() {
    let dir = PathBuf::from("/tmp/x86_64-linux-musl-cross");
    assert_eq!(
        Arch::X86_64.musl_gcc(&dir),
        dir.join("bin/x86_64-linux-musl-gcc")
    );
    assert_eq!(
        Arch::Aarch64.musl_gcc(&dir),
        dir.join("bin/aarch64-linux-musl-gcc")
    );
    assert_eq!(
        Arch::Riscv64.musl_gcc(&dir),
        dir.join("bin/riscv64-linux-musl-gcc")
    );
}

// ---------------------------------------------------------------------------
// The order the toolchain is looked for in.
// ---------------------------------------------------------------------------

/// The 64-bit-hosted source is tried **first**. That ordering is the whole
/// point: the mirror that comes second works everywhere a host runs 32-bit x86
/// binaries, so a build that asked it first would never notice the 64-bit one
/// had appeared, and the host that needs it is exactly the one that cannot say
/// so -- it fails inside busybox's makefile, not here.
#[test]
fn the_64_bit_source_is_tried_before_the_32_bit_mirror() {
    assert_eq!(Source::ALL, [Source::Eclipse64, Source::ZcoreMusl]);
}

/// Each source fetches a tarball named for the toolchain, from its own
/// release.
#[test]
fn each_source_names_the_toolchain_it_fetches() {
    let name = "x86_64-linux-musl-cross";
    let eclipse = Source::Eclipse64.url(name);
    assert!(eclipse.contains("Pryancito/eclipse"), "{eclipse}");
    assert!(eclipse.contains("musl-cross-64"), "{eclipse}");
    assert!(
        eclipse.ends_with("/x86_64-linux-musl-cross.tgz"),
        "{eclipse}"
    );

    let zcore = Source::ZcoreMusl.url(name);
    assert!(zcore.contains("YdrMaster/zCore"), "{zcore}");
    assert!(zcore.contains("musl-cache"), "{zcore}");
    assert!(zcore.ends_with("/x86_64-linux-musl-cross.tgz"), "{zcore}");
}

/// And every arch gets its own tarball from every source, so nothing collides
/// across arches either.
#[test]
fn every_arch_has_its_own_tarball_from_every_source() {
    let mut seen = std::collections::HashSet::new();
    for arch in [Arch::Riscv64, Arch::X86_64, Arch::Aarch64] {
        let name = format!("{}-linux-musl-cross", arch.name());
        for source in Source::ALL {
            assert!(
                seen.insert(source.tarball(&name)),
                "two tarballs share a name: {} for {}",
                source.tarball(&name),
                arch.name()
            );
            assert!(source.tarball(&name).contains(arch.name()));
        }
    }
}

/// The two sources must not share a cached file name. `wget` returns early when
/// the destination already exists, so if both wrote `<name>.tgz`, a failed
/// first download that left a file behind -- or, worse, a *successful* one
/// holding the wrong toolchain -- would be handed straight to the second
/// source and unpacked as if it were its own.
#[test]
fn the_two_sources_cache_their_downloads_under_different_names() {
    let name = "x86_64-linux-musl-cross";
    assert_ne!(
        Source::Eclipse64.tarball(name),
        Source::ZcoreMusl.tarball(name)
    );
    // The historical one keeps its historical name, so an existing checkout's
    // cache is still used rather than downloaded again.
    assert_eq!(
        Source::ZcoreMusl.tarball(name),
        "x86_64-linux-musl-cross.tgz"
    );
}

/// The one-line form is what the "tried, in order" list is built from, so it
/// has to say which of the two failures it was without repeating the URL that
/// already precedes it.
#[test]
fn the_one_line_form_distinguishes_broken_from_unrunnable() {
    let gcc = PathBuf::from("bin/x86_64-linux-musl-gcc");

    let broken = Compiler::Broken("not there".into()).one_line(&gcc);
    assert!(broken.contains("not there"), "{broken}");

    let unrunnable = Compiler::Unrunnable {
        elf: ElfId::parse(&MUSL_GCC_HEAD).unwrap(),
        why: "Exec format error".into(),
    }
    .one_line(&gcc);
    assert!(unrunnable.contains("32-bit"), "{unrunnable}");
    assert!(unrunnable.contains("i386"), "{unrunnable}");
    assert!(unrunnable.contains("will not run"), "{unrunnable}");
}

// ---------------------------------------------------------------------------
// A musl toolchain already on this host.
// ---------------------------------------------------------------------------

/// `-dumpmachine` is the only thing worth trusting about a compiler, because
/// the name lies: the published `x86_64-linux-musl-gcc` is a 32-bit i386
/// binary. These are the answers the three compilers that matter give.
#[test]
fn only_a_musl_triple_for_the_right_arch_counts_as_a_match() {
    let matches = |triple: &str, arch: &str| {
        triple.starts_with(&format!("{arch}-")) && triple.ends_with("-musl")
    };

    // Alpine's native compiler, which is what Eclipse's own rootfs can install
    // with `apk add build-base`.
    assert!(matches("x86_64-alpine-linux-musl", "x86_64"));
    // A cross toolchain built by musl-cross-make.
    assert!(matches("x86_64-linux-musl", "x86_64"));
    assert!(matches("aarch64-linux-musl", "aarch64"));

    // A glibc host is not a match, which is why the CI runners keep
    // downloading exactly as they did.
    assert!(!matches("x86_64-pc-linux-gnu", "x86_64"));
    assert!(!matches("x86_64-linux-gnu", "x86_64"));
    // Nor is the right libc on the wrong arch.
    assert!(!matches("aarch64-linux-musl", "x86_64"));
    assert!(!matches("i686-linux-musl", "x86_64"));
}

/// The real `targets_musl` agrees with that, run against a compiler that is
/// not there (nothing to run means no match) and against one that answers
/// something else entirely.
#[test]
fn a_compiler_that_cannot_be_asked_is_not_a_match() {
    assert!(!targets_musl(
        &PathBuf::from("/nonexistent-4c2a/x86_64-linux-musl-gcc"),
        "x86_64"
    ));
    // `true` exists and exits 0, but prints no triple.
    if let Some(yes) = which("true") {
        assert!(!targets_musl(&yes, "x86_64"));
    }
}

/// `which` finds what is on `PATH` and nothing else.
#[test]
fn which_finds_a_program_on_the_path() {
    let sh = which("sh").expect("every host has sh");
    assert!(sh.is_file(), "{}", sh.display());
    assert!(sh.ends_with("sh"), "{}", sh.display());
    assert!(which("eclipse-no-such-program-xtask-test").is_none());
}

/// The linked names are the ones `CROSS_COMPILE=<prefix>` makes a makefile
/// reach for. `gcc` is the one every caller in this tree uses by hand, and
/// `strip` is used by `LinuxRootfs` directly, so losing either would break the
/// build in a way no test here would see.
#[test]
fn the_linked_tools_cover_what_the_build_actually_asks_for() {
    for needed in ["gcc", "g++", "strip", "ld", "ar", "ranlib"] {
        assert!(TOOLS.contains(&needed), "{needed} is not linked");
    }
    let unique: std::collections::HashSet<_> = TOOLS.iter().collect();
    assert_eq!(unique.len(), TOOLS.len(), "a tool is listed twice");
}
