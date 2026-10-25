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
