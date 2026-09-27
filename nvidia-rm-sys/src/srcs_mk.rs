//! The list of C sources `build.rs` compiles, taken out of NVIDIA's own
//! `src/nvidia/srcs.mk`, and the two Eclipse-patched files that replace a
//! submodule copy.
//!
//! **In `src/` but not a module of the library.** `build.rs` includes it with
//! `#[path = "src/srcs_mk.rs"]`, and so does `tests/srcs_mk.rs`, which is the
//! only reason any of it is tested: a build script is not a target that any
//! `cargo test` compiles, so its happy path runs on every build of the kernel
//! and everything it does to catch a problem runs nowhere. Same argument
//! `linux-vdso/build.rs` makes for including `src/elf.rs` instead of
//! re-deriving musl's lookup: one implementation, and the tests drive that one.
//!
//! It also means this file must not need anything the build script cannot have.
//! It is plain `std`, no `cc`, no crate of ours.

use std::path::{Path, PathBuf};

/// The one spelling NVIDIA's `srcs.mk` uses for a source assignment.
const ASSIGN: &str = "SRCS += ";

/// NVIDIA's own Linux platform-integration layer -- the real `/dev/nvidia0`
/// character device, its ioctls, registry access. Eclipse replaces all of it
/// with `os_interface.rs` rather than vendoring it, so it is left out.
const PLATFORM_LAYER: &str = "arch/nvalloc/unix/src/";

/// An RM source Eclipse compiles **instead of** the submodule's own copy.
///
/// The submodule stays pristine upstream (local edits there would never reach
/// another clone), so a file Eclipse has to modify is copied into
/// `vendor/eclipse_overrides/` -- tracked by the main repo -- and compiled in
/// its place. The include search path is identical, so the copy compiles
/// unchanged apart from the marked ECLIPSE edits.
pub struct Override {
    /// Matched against the end of the `srcs.mk` path, so the leading part of
    /// the path can move without this going stale.
    pub upstream_suffix: &'static str,
    /// Path, relative to the crate root, of the file compiled in its place.
    pub replacement: &'static str,
}

/// Currently the two graphics files, which carry the loud golden-image and
/// global-ctx-buffer-map diagnostics and the propagate-map-failure fix from the
/// FECS-RESTORE hang investigation.
pub const OVERRIDES: &[Override] = &[
    Override {
        upstream_suffix: "src/kernel/gpu/gr/kernel_graphics.c",
        replacement: "vendor/eclipse_overrides/kernel_graphics.c",
    },
    Override {
        upstream_suffix: "src/kernel/gpu/gr/kernel_graphics_context.c",
        replacement: "vendor/eclipse_overrides/kernel_graphics_context.c",
    },
];

/// What [`parse`] made of a `srcs.mk`, with enough left over for the caller to
/// check that it did what it meant to.
pub struct Sources {
    /// Every file to compile, in the order `srcs.mk` lists them.
    pub files: Vec<PathBuf>,
    /// How many `srcs.mk` entries each [`OVERRIDES`] entry replaced, by index.
    ///
    /// The point of counting: `upstream_suffix` is matched against the end of a
    /// path, and a path that moves upstream stops matching **silently**. The
    /// build then compiles the submodule's pristine copy, links, boots, and
    /// quietly does not have the Eclipse fix in it. A retarget of the submodule
    /// is exactly when a file moves.
    pub override_hits: Vec<usize>,
    /// Entries left out because they are NVIDIA's Linux platform layer.
    pub skipped_platform_layer: usize,
}

/// Turns a `srcs.mk` into the list of files to compile.
///
/// `nvidia_dir` is where the submodule's `src/nvidia` lives, which is what the
/// relative paths in `srcs.mk` are relative to. Takes the text rather than the
/// path so it can be driven by a test without a submodule on disk -- and then
/// driven by a test *with* one, against the real file.
pub fn parse(text: &str, nvidia_dir: &Path) -> Sources {
    let mut files = Vec::new();
    let mut override_hits = vec![0usize; OVERRIDES.len()];
    let mut skipped_platform_layer = 0;

    'lines: for line in text.lines() {
        let Some(rel) = line.strip_prefix(ASSIGN) else {
            continue;
        };
        let rel = rel.trim();
        if rel.contains(PLATFORM_LAYER) {
            skipped_platform_layer += 1;
            continue;
        }
        for (i, ov) in OVERRIDES.iter().enumerate() {
            if rel.ends_with(ov.upstream_suffix) {
                override_hits[i] += 1;
                files.push(PathBuf::from(ov.replacement));
                continue 'lines;
            }
        }
        files.push(nvidia_dir.join(rel));
    }

    Sources {
        files,
        override_hits,
        skipped_platform_layer,
    }
}

/// Lines that plainly assign a source and that [`parse`] did **not** take.
///
/// [`parse`] matches exactly `"SRCS += "`, which is the only spelling in the
/// 1015 lines of the pinned submodule's `srcs.mk`. Keeping it exact is
/// deliberate -- widening it could pull in a file the build has never compiled
/// -- but silently skipping a line it does not recognise is not: a dropped
/// source surfaces as `undefined symbol` out of the linker, hundreds of lines
/// and one subsystem away from the makefile that caused it. And a retarget of
/// the submodule is exactly when a makefile gets reformatted.
///
/// So anything that looks like an assignment to `SRCS` with `+=` and was not
/// taken comes back here, to be named rather than dropped. `SRCS ?=` is not one
/// (different operator) and neither is `SRCS_CXX` (different variable, and its
/// C++ is not ours to compile).
pub fn unrecognised(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.starts_with(ASSIGN) && assigns_to_srcs(line))
        .collect()
}

/// `SRCS`, then any run of spaces or tabs, then `+=`, then something.
fn assigns_to_srcs(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("SRCS") else {
        return false;
    };
    // `SRCS_CXX`, `SRCS_CXX_EXTRA` and any other variable whose name merely
    // starts with these four letters fall out here on their own: only spaces and
    // tabs may sit between the name and the operator, so anything else before
    // `+=` means a different variable. (A guard that said so explicitly lived
    // here and was dead code -- `strip_prefix` already answers no.)
    match rest.trim_start_matches([' ', '\t']).strip_prefix("+=") {
        Some(value) => !value.trim().is_empty(),
        None => false,
    }
}

/// One line for a `cargo:warning` when an `-I` directory is not there.
///
/// A compiler ignores a `-I` that does not exist, without a word, so the list
/// can drift away from the tree and nothing says so until a header is missing
/// somewhere else entirely. Naming them is the whole fix: the entries are
/// harmless, the silence is not.
pub fn missing_include_dirs(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter()
        .filter(|d| !d.is_dir())
        .map(|d| d.display().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: &str = "vendor/open-gpu-kernel-modules/src/nvidia";

    fn parse_str(text: &str) -> Sources {
        parse(text, Path::new(NVIDIA))
    }

    fn paths(s: &Sources) -> Vec<String> {
        s.files.iter().map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn a_source_becomes_a_path_under_the_submodule() {
        let s = parse_str("SRCS += src/kernel/gpu/gpu.c\n");
        assert_eq!(paths(&s), vec![format!("{NVIDIA}/src/kernel/gpu/gpu.c")]);
    }

    #[test]
    fn the_order_srcs_mk_lists_them_in_is_kept() {
        let s = parse_str("SRCS += b.c\nSRCS += a.c\nSRCS += c.c\n");
        assert_eq!(
            paths(&s),
            vec![
                format!("{NVIDIA}/b.c"),
                format!("{NVIDIA}/a.c"),
                format!("{NVIDIA}/c.c"),
            ]
        );
    }

    #[test]
    fn nvidias_own_linux_platform_layer_is_left_out_and_counted() {
        let s = parse_str(
            "SRCS += src/kernel/gpu/gpu.c\n\
             SRCS += arch/nvalloc/unix/src/osapi.c\n\
             SRCS += arch/nvalloc/unix/src/escape.c\n",
        );
        assert_eq!(s.skipped_platform_layer, 2);
        assert_eq!(paths(&s), vec![format!("{NVIDIA}/src/kernel/gpu/gpu.c")]);
    }

    #[test]
    fn an_overridden_source_is_eclipses_copy_and_not_the_submodules() {
        let s = parse_str("SRCS += src/kernel/gpu/gr/kernel_graphics.c\n");
        assert_eq!(
            paths(&s),
            vec!["vendor/eclipse_overrides/kernel_graphics.c"]
        );
        assert_eq!(s.override_hits, vec![1, 0]);
    }

    /// `kernel_graphics_context.c` does not end with `kernel_graphics.c`, so the
    /// two never cross -- but only because of where the `.c` falls. Fixed here
    /// so a third override named like a prefix of another cannot land quietly on
    /// the wrong file.
    #[test]
    fn the_context_file_is_not_mistaken_for_the_graphics_file() {
        let s = parse_str(
            "SRCS += src/kernel/gpu/gr/kernel_graphics.c\n\
             SRCS += src/kernel/gpu/gr/kernel_graphics_context.c\n\
             SRCS += src/kernel/gpu/gr/kernel_graphics_manager.c\n",
        );
        assert_eq!(
            paths(&s),
            vec![
                "vendor/eclipse_overrides/kernel_graphics.c",
                "vendor/eclipse_overrides/kernel_graphics_context.c",
                &format!("{NVIDIA}/src/kernel/gpu/gr/kernel_graphics_manager.c"),
            ]
        );
        assert_eq!(s.override_hits, vec![1, 1]);
    }

    /// The reason `override_hits` exists. A retarget that moves the file leaves
    /// a build that compiles, links and boots **without the Eclipse fix in it**,
    /// and says nothing.
    #[test]
    fn an_override_that_replaced_nothing_shows_as_zero() {
        let s = parse_str("SRCS += src/kernel/gpu/gr/kernel_graphics_v2.c\n");
        assert_eq!(s.override_hits, vec![0, 0]);
        assert_eq!(
            paths(&s),
            vec![format!("{NVIDIA}/src/kernel/gpu/gr/kernel_graphics_v2.c")]
        );
    }

    #[test]
    fn an_override_that_replaced_twice_shows_as_two() {
        let s = parse_str(
            "SRCS += src/kernel/gpu/gr/kernel_graphics.c\n\
             SRCS += generated/src/kernel/gpu/gr/kernel_graphics.c\n",
        );
        assert_eq!(s.override_hits, vec![2, 0]);
    }

    #[test]
    fn what_is_not_an_assignment_is_neither_taken_nor_reported() {
        let text = "# a comment\n\
                    SRCS ?=\n\
                    SRCS_CXX ?=\n\
                    \n\
                    include utils.mk\n\
                    CFLAGS += -I inc\n";
        assert!(parse_str(text).files.is_empty());
        assert!(unrecognised(text).is_empty());
    }

    /// A line the parser DID take must not also be reported as one it missed:
    /// every build would then stop on the thousand lines that are perfectly fine.
    #[test]
    fn a_line_the_parser_took_is_not_also_reported_as_missed() {
        let text = "SRCS += src/kernel/gpu/gpu.c\nSRCS += src/kernel/gpu/bus/kern_bus.c\n";
        assert_eq!(parse_str(text).files.len(), 2);
        assert!(unrecognised(text).is_empty());
    }

    /// A variable whose name only begins or ends with those four letters is not
    /// ours, and must not be reported as a source we dropped -- the C++ list
    /// least of all, since its contents are not ours to compile.
    #[test]
    fn a_variable_that_merely_looks_like_srcs_is_not_ours() {
        for line in [
            "SRCS_CXX += src/foo.cpp",
            "SRCS_CXX_EXTRA += src/foo.cpp",
            "NVIDIA_SRCS += src/foo.c",
            "SRCSX += src/foo.c",
        ] {
            let text = format!("{line}\n");
            assert!(
                unrecognised(&text).is_empty(),
                "{line:?} no es nuestra variable y se reporta como fuente perdida"
            );
            assert!(
                parse_str(&text).files.is_empty(),
                "y encima se compila: {line:?}"
            );
        }
    }

    /// Every one of these assigns a source, and `parse` takes none of them. A
    /// retarget is exactly when a makefile gets reformatted, and each of these
    /// would otherwise leave the linker complaining about a symbol with nothing
    /// pointing at the file that went missing.
    #[test]
    fn a_spelling_the_parser_does_not_take_is_named_rather_than_dropped() {
        for line in [
            "SRCS +=src/kernel/gpu/gpu.c",
            "SRCS  += src/kernel/gpu/gpu.c",
            "SRCS\t+= src/kernel/gpu/gpu.c",
            "\tSRCS += src/kernel/gpu/gpu.c",
            "SRCS+= src/kernel/gpu/gpu.c",
        ] {
            let text = format!("{line}\n");
            assert!(
                parse_str(&text).files.is_empty(),
                "parse se llevo {line:?}, y entonces este test no prueba nada"
            );
            assert_eq!(
                unrecognised(&text),
                vec![line],
                "esta linea asigna una fuente y nadie la nombra: {line:?}"
            );
        }
    }

    /// `SRCS +=` with nothing after it is not a dropped source.
    #[test]
    fn an_assignment_of_nothing_is_not_a_dropped_source() {
        assert!(unrecognised("SRCS +=\n").is_empty());
        assert!(unrecognised("SRCS +=   \n").is_empty());
    }

    #[test]
    fn the_operator_has_to_be_the_appending_one() {
        assert!(unrecognised("SRCS ?= src/foo.c\n").is_empty());
        assert!(unrecognised("SRCS = src/foo.c\n").is_empty());
        assert!(unrecognised("SRCS := src/foo.c\n").is_empty());
    }

    #[test]
    fn trailing_whitespace_is_not_part_of_a_path() {
        let s = parse_str("SRCS += src/kernel/gpu/gpu.c   \n");
        assert_eq!(paths(&s), vec![format!("{NVIDIA}/src/kernel/gpu/gpu.c")]);
    }

    // -----------------------------------------------------------------------
    // Against the submodule actually checked out, when there is one
    // -----------------------------------------------------------------------

    /// The real `srcs.mk`, parsed whole. Everything above says what `parse`
    /// does with a line; this says it does it to the file the build uses.
    ///
    /// Skipped, not failed, without a submodule: a host `cargo test` in a fresh
    /// clone has none, and `build.rs` already says what to run about that.
    #[test]
    fn the_pinned_submodules_own_srcs_mk_parses_whole() {
        let nvidia = Path::new(NVIDIA);
        let Ok(text) = std::fs::read_to_string(nvidia.join("srcs.mk")) else {
            return;
        };
        assert!(
            unrecognised(&text).is_empty(),
            "srcs.mk asigna fuentes que el parser no se lleva: {:?}",
            unrecognised(&text)
        );
        let s = parse(&text, nvidia);
        assert_eq!(
            s.override_hits,
            vec![1, 1],
            "los overrides de Eclipse no reemplazaron una entrada cada uno"
        );
        assert!(
            s.files.len() > 900,
            "solo {} fuentes; el RM son mas de mil",
            s.files.len()
        );
        assert!(
            s.skipped_platform_layer > 0,
            "ninguna entrada de la capa de Linux: el filtro ya no coincide con nada"
        );
    }

    /// An override that names a file nobody put there would fail in `cc`, far
    /// from the table that named it.
    #[test]
    fn every_override_replacement_is_a_file_on_disk() {
        for ov in OVERRIDES {
            assert!(
                Path::new(ov.replacement).is_file(),
                "el override {:?} no existe",
                ov.replacement
            );
        }
    }

    #[test]
    fn a_directory_that_is_not_there_is_reported_and_one_that_is_is_not() {
        let here = PathBuf::from("src");
        let nope = PathBuf::from("src/no-hay-tal-directorio");
        assert_eq!(missing_include_dirs(&[here.clone()]), Vec::<String>::new());
        assert_eq!(
            missing_include_dirs(&[here, nope.clone()]),
            vec![nope.display().to_string()]
        );
    }
}
