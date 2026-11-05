//! The i386 probes of `tools/`: the handful of 32-bit x86 programs that
//! prove this kernel's compatibility path, and the freshness rule they
//! share. Own file for the size ceiling of `mod.rs`.
//!
//! All of them are built with the HOST's binutils (`as --32`,
//! `ld -m elf_i386`) and not with the musl cross toolchain this tree
//! downloads, which cannot target i386 at all. All of them are
//! best-effort: a host without 32-bit binutils skips the probe instead of
//! failing `make image`.

use super::{verify, LinuxRootfs};
use crate::{Arch, PROJECT_DIR};
use os_xtask_utils::{dir, CommandExt, Ext};
use std::{
    fs,
    path::{Path, PathBuf},
};

impl LinuxRootfs {
    /// Assemble `i386-hello` (tools/i386-hello): the one program that proves
    /// this kernel runs a 32-bit binary.
    ///
    /// x86_64 only, and freestanding: it is an `ELFCLASS32` / `EM_386` static
    /// executable with no libc, so it exercises the kernel's compatibility
    /// path -- the ELF32 loader, the `iretq` into compatibility mode, the
    /// `int $0x80` gate, the i386 syscall numbering, the 4-byte initial stack
    /// -- and nothing else. See the comment at the top of `i386-hello.s` for
    /// what each exit status means.
    ///
    /// Built with the HOST's binutils (`as --32`, `ld -m elf_i386`) rather
    /// than the musl cross compiler, for two reasons: a program with no libc
    /// needs no toolchain beyond an assembler and a linker, and the musl cross
    /// toolchain this tree downloads cannot target i386 at all.
    ///
    /// Best-effort, like every other probe here: a binutils without 32-bit
    /// support skips the tool instead of failing the image.
    pub(super) fn i386_hello(&self) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("i386-hello");
        let executable = dir.join("i386-hello");
        let source = dir.join("i386-hello.s");
        // `verify::cached_for_i386`, not `self.cached_for_target`: the gate
        // asks whether a cached artifact is still what this cache builds, and
        // what this one builds is an ELF32 / `EM_386`, whatever architecture
        // the image is for. `cached_for_target` would reject it outright.
        if Self::fresh_i386(&executable, &[&source]) {
            return executable;
        }

        println!("Assembling i386-hello...");
        let object = dir.join("i386-hello.o");
        let assembled = Ext::new("as")
            .current_dir(&dir)
            .arg("--32")
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .status();
        if !assembled.success() {
            println!("Failed to assemble i386-hello (does `as` support --32?)");
            return executable;
        }
        let linked = Ext::new("ld")
            .current_dir(&dir)
            .arg("-m")
            .arg("elf_i386")
            .arg("-static")
            .arg("-o")
            .arg(&executable)
            .arg(&object)
            .status();
        let _ = fs::remove_file(&object);
        if !linked.success() {
            println!("Failed to link i386-hello (does `ld` support -m elf_i386?)");
        }
        executable
    }

    /// Build `i386-glibc` (tools/i386-glibc) and put it in `root/bin`: the
    /// one program in the image that is a 32-bit **dynamic** binary.
    ///
    /// Everything else of i386 in here is freestanding, which is right for
    /// measuring one system call at a time and leaves unmeasured the shape
    /// every real 32-bit program has: a `PT_INTERP` naming
    /// `/lib/ld-linux.so.2`, a `DT_NEEDED` on `libc.so.6`, and eighteen
    /// system calls made by the interpreter before the program's first
    /// instruction.
    ///
    /// Linked against the i386 `libc.so.6` that
    /// [`apt_rootfs::multilib32`] has just staged into this very root ---
    /// so no cross toolchain and no 32-bit C compiler, exactly like
    /// [`i386_hello`](Self::i386_hello). And **nothing at all without that
    /// runtime**: a dynamic program whose interpreter is not in the image is
    /// a probe that can only report 127, which is the same reasoning as the
    /// `PT_INTERP` pairs going in whole or not at all.
    ///
    /// `-rpath-link` is for the link only and puts no path in the binary: it
    /// is how `ld` resolves the `ld-linux.so.2` that `libc.so.6` itself
    /// names for its `GLIBC_PRIVATE` symbols.
    pub(super) fn i386_glibc(&self, root: &Path) {
        if !matches!(self.0, Arch::X86_64) {
            return;
        }
        let libdir = root.join("lib").join("i386-linux-gnu");
        let libc = libdir.join("libc.so.6");
        if !libc.is_file() {
            println!(
                "i386-glibc: no hay runtime de 32 bits en la imagen, \
                 asi que la sonda dinamica no entra"
            );
            return;
        }
        let dir = PROJECT_DIR.join("tools").join("i386-glibc");
        let executable = dir.join("i386-glibc");
        let source = dir.join("i386-glibc.s");
        // The libc is a source of this artifact and not only a flag: a
        // rootfs that just got a different one has to relink, or the probe
        // keeps whatever it resolved against last time.
        if !Self::fresh_i386(&executable, &[&source, &libc]) {
            println!("Assembling i386-glibc...");
            let object = dir.join("i386-glibc.o");
            let assembled = Ext::new("as")
                .current_dir(&dir)
                .arg("--32")
                .arg(&source)
                .arg("-o")
                .arg(&object)
                .status();
            if !assembled.success() {
                println!("Failed to assemble i386-glibc (does `as` support --32?)");
                return;
            }
            let linked = Ext::new("ld")
                .current_dir(&dir)
                .arg("-m")
                .arg("elf_i386")
                .arg("-dynamic-linker")
                .arg("/lib/ld-linux.so.2")
                .arg("-o")
                .arg(&executable)
                .arg(&object)
                .arg("-rpath-link")
                .arg(&libdir)
                .arg(&libc)
                .status();
            let _ = fs::remove_file(&object);
            if !linked.success() {
                println!("Failed to link i386-glibc against {}", libc.display());
                return;
            }
        }
        if verify::es_un_elf_de_i386(&executable) {
            let bin = root.join("bin");
            let _ = fs::create_dir_all(&bin);
            let dst = bin.join("i386-glibc");
            let _ = dir::rm(&dst);
            let _ = fs::copy(&executable, &dst);
        }
    }

    /// Whether an i386 artifact on disk is still the one these sources build:
    /// it exists, every source exists, it is not older than any of them, and
    /// it really is an ELF32 / `EM_386`.
    ///
    /// `verify::cached_for_i386` and not `cached_for_target`, which is the
    /// whole reason this is not the ordinary freshness check: what these
    /// caches build is a 32-bit x86 binary whatever architecture the image is
    /// for, and the ordinary check would reject it outright.
    ///
    /// And `verify::es_un_elf_de_i386` as well as it, because the two answer
    /// different questions and only the second one is the claim this doc makes.
    /// `cached_for_i386` says "not from another arch" and so lets through
    /// everything it cannot read -- a file that does not open, one shorter
    /// than a header, one that is not an ELF at all. Asking only that, a
    /// half-written artifact with a good mtime counted as fresh, nothing was
    /// rebuilt, and the probe measured in QEMU was the stale or broken one.
    /// Pointed out by Copilot.
    fn fresh_i386(artifact: &Path, sources: &[&Path]) -> bool {
        if !artifact.is_file()
            || !verify::cached_for_i386(artifact)
            || !verify::es_un_elf_de_i386(artifact)
        {
            return false;
        }
        let Ok(artifact_mtime) = fs::metadata(artifact).and_then(|m| m.modified()) else {
            return false;
        };
        sources.iter().all(|source| {
            match fs::metadata(source).and_then(|m| m.modified()) {
                Ok(source_mtime) => artifact_mtime >= source_mtime,
                // A source that is not there cannot be newer, but it also
                // cannot be rebuilt from: treat it as stale and let the build
                // say so.
                Err(_) => false,
            }
        })
    }

    /// Assemble the `PT_INTERP` pairs of `tools/i386-dyn`, which together are
    /// the kernel's half of stage 4 of the 32-bit compatibility work.
    ///
    /// Each pair is a 32-bit program and the interpreter its `PT_INTERP`
    /// names, and the two halves go in together or not at all: the program
    /// names the interpreter by an absolute path, so one without the other is
    /// a probe that can only fail. Returns the pairs that are there, as
    /// `(what was built, the name it takes in the rootfs)`.
    ///
    /// There are two pairs because the kernel has two paths, and a boot that
    /// runs one of them says nothing about the other:
    ///
    /// - `i386-dyn` with `i386-interp`, an **`ET_EXEC`** interpreter at
    ///   `0x0a00_0000`, clear of the program's own fixed address. Loaded at
    ///   the absolute vaddrs it asks for, with `AT_BASE` zero.
    /// - `i386-pie` with `i386-interp-pie`, an **`ET_DYN`** one, which is what
    ///   every real `ld.so` is. Relocated wherever the loader puts it, with
    ///   `AT_BASE` the bias it was given.
    ///
    /// Built the way `i386_hello` is --- host binutils, no cross toolchain, no
    /// libc --- and every program header written out by hand against a linker
    /// script with `--oformat binary`, because `ld` drops `--dynamic-linker`
    /// when the output needs no dynamic linking and there is no 32-bit libc in
    /// this image to need one, so there is no way to ask it for a bare
    /// `PT_INTERP`.
    pub(super) fn i386_dyn_pairs(&self) -> Vec<[(PathBuf, &'static str); 2]> {
        /// Each half as `(name, the `ld` arguments after `-m elf_i386`)`. The
        /// name is the source (`<name>.s`), the output, and the name in the
        /// rootfs, all three, which is what keeps this table readable.
        const PAIRS: [[(&str, &[&str]); 2]; 2] = [
            [
                ("i386-dyn", &["-T", "i386-dyn.ld", "--oformat", "binary"]),
                ("i386-interp", &["-static", "-Ttext", "0x0a000000"]),
            ],
            [
                ("i386-pie", &["-T", "i386-dyn.ld", "--oformat", "binary"]),
                (
                    "i386-interp-pie",
                    &["-T", "i386-pie.ld", "--oformat", "binary"],
                ),
            ],
        ];

        let dir = PROJECT_DIR.join("tools").join("i386-dyn");
        PAIRS
            .iter()
            .filter_map(|[(program, program_link), (interp, interp_link)]| {
                // Both are attempted before either answer is read, and not
                // short-circuited: a host that cannot build one of them
                // cannot build the other either, and the point of the attempt
                // is the line it prints saying which tool is missing.
                let has_program = Self::assemble_i386(&dir, program, program_link);
                let has_interp = Self::assemble_i386(&dir, interp, interp_link);
                (has_program && has_interp)
                    .then(|| [(dir.join(program), *program), (dir.join(interp), *interp)])
            })
            .collect()
    }

    /// One hand-written i386 artifact of `tools/i386-dyn`: assembled with
    /// `as --32` and linked with `ld -m elf_i386` plus `link`, unless what is
    /// on disk is already current. `false` if it could not be built, which a
    /// host without 32-bit binutils is entitled to be --- `make image` is not
    /// failed over a probe, the probe is simply not installed.
    ///
    /// Every `.ld` among the link arguments counts as a source alongside
    /// `<name>.s`: a change to a linker script moves every address in what it
    /// links, and the whole point of these two is the addresses.
    fn assemble_i386(dir: &Path, name: &str, link: &[&str]) -> bool {
        let out = dir.join(name);
        let src = dir.join(format!("{name}.s"));
        let scripts: Vec<PathBuf> = link
            .iter()
            .filter(|a| a.ends_with(".ld"))
            .map(|a| dir.join(a))
            .collect();
        let mut sources: Vec<&Path> = vec![src.as_path()];
        sources.extend(scripts.iter().map(PathBuf::as_path));
        if Self::fresh_i386(&out, &sources) {
            return true;
        }

        println!("Assembling {name}...");
        let obj = dir.join(format!("{name}.o"));
        if !Ext::new("as")
            .current_dir(dir)
            .arg("--32")
            .arg(&src)
            .arg("-o")
            .arg(&obj)
            .status()
            .success()
        {
            println!("Failed to assemble {name} (does `as` support --32?)");
            return false;
        }
        let mut cmd = Ext::new("ld");
        cmd.current_dir(dir).arg("-m").arg("elf_i386");
        for a in link {
            cmd.arg(a);
        }
        let linked = cmd.arg("-o").arg(&out).arg(&obj).status();
        let _ = fs::remove_file(&obj);
        if !linked.success() {
            println!("Failed to link {name} (does `ld` support -m elf_i386?)");
            return false;
        }
        out.is_file()
    }
}
