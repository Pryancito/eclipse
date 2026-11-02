//! 支持架构的定义。

use crate::{commands::wget, variant::Variant, LinuxRootfs, XError, ARCHS, TARGET};
use os_xtask_utils::{dir, CommandExt, Tar};
use std::{
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

/// 支持的 CPU 架构。
#[derive(Clone, Copy)]
pub(crate) enum Arch {
    Riscv64,
    X86_64,
    Aarch64,
}

impl Arch {
    /// Returns the name of Arch.
    #[inline]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Riscv64 => "riscv64",
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Returns the path to store arch-dependent files from network.
    #[inline]
    pub fn origin(&self) -> PathBuf {
        ARCHS.join(self.name())
    }

    /// Returns the path to cache arch-dependent generated files durning processes.
    #[inline]
    pub fn target(&self) -> PathBuf {
        TARGET.join(self.name())
    }

    /// Downloads linux musl toolchain, and returns its path.
    ///
    /// The toolchain is only ever *used* through `<dir>/bin/<arch>-linux-musl-gcc`,
    /// handed to busybox's and to Xorg's makefiles as `CROSS_COMPILE`. Until
    /// 2026-11-02 the only thing checked here was that `<dir>` existed, so
    /// anything under it -- a half-written extraction, a compiler this host
    /// cannot execute -- was passed on to `make`, which handed it to `/bin/sh`,
    /// which failed to `execve` it, fell back to reading it as a shell script
    /// and printed the first bytes of the ELF header as a syntax error:
    ///
    /// ```text
    /// .../bin/x86_64-linux-musl-gcc: line 1: ELF...: Invalid argument
    /// .../bin/x86_64-linux-musl-gcc: line 2: syntax error: unexpected "("
    /// make[2]: *** [scripts/Makefile.build:198: applets/applets.o] Error 2
    /// ```
    ///
    /// Nothing in that names the compiler as the problem. So the compiler is
    /// checked here instead, before anybody is given its path: see
    /// [`check_compiler`].
    pub fn linux_musl_cross(&self) -> PathBuf {
        let name = format!("{}-linux-musl-cross", self.name().to_lowercase());

        let origin = self.origin();
        let target = self.target();

        let tgz = origin.join(format!("{name}.tgz"));
        let dir = target.join(&name);

        // An explicit toolchain wins over the downloaded one, and goes through
        // exactly the same check. It is the only way out for a host that
        // cannot run the prebuilt compiler at all.
        if let Some(home) = std::env::var_os(MUSL_CROSS_HOME) {
            let home = PathBuf::from(home);
            match check_compiler(&self.musl_gcc(&home)) {
                Compiler::Usable => return home,
                bad => panic!("{}", bad.explain(&self.musl_gcc(&home), MUSL_CROSS_HOME)),
            }
        }

        dir::create_parent(&dir).unwrap();
        let gcc = self.musl_gcc(&dir);
        match check_compiler(&gcc) {
            // Already there and it runs: nothing to do.
            Compiler::Usable => return dir,
            // It runs, but it is not a compiler we can use, or it cannot run
            // on this host at all. Downloading the same tarball again would
            // produce the same file, so say why and stop.
            bad @ Compiler::Unrunnable { .. } => {
                panic!("{}", bad.explain(&gcc, MUSL_CROSS_HOME))
            }
            // Missing, truncated, or not an ELF at all: whatever is cached is
            // not the toolchain. Throw away *both* the directory and the
            // tarball -- `wget` skips the download when the tarball is still
            // there, so leaving it behind would re-extract the same rubbish --
            // and fetch it again.
            Compiler::Broken(why) => {
                if dir.exists() {
                    println!("{}: {why}. Re-downloading.", gcc.display());
                }
                dir::rm(&dir).unwrap();
                dir::rm(&tgz).unwrap();
            }
        }

        wget(
            format!("https://github.com/YdrMaster/zCore/releases/download/musl-cache/{name}.tgz"),
            &tgz,
        );
        Tar::xf(&tgz, Some(target)).invoke();

        // And check the fresh one too: an extraction that ran out of disk, or
        // died with the build, leaves the same half-written compiler behind.
        match check_compiler(&gcc) {
            Compiler::Usable => dir,
            bad => panic!("{}", bad.explain(&gcc, MUSL_CROSS_HOME)),
        }
    }

    /// The one file out of the whole toolchain that every caller runs.
    #[inline]
    fn musl_gcc(&self, dir: &Path) -> PathBuf {
        dir.join("bin")
            .join(format!("{}-linux-musl-gcc", self.name()))
    }
}

/// Points at a directory holding `bin/<arch>-linux-musl-gcc`, used instead of
/// the downloaded toolchain.
const MUSL_CROSS_HOME: &str = "MUSL_CROSS_HOME";

/// What the cross compiler at a given path turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Compiler {
    /// It is there and it ran.
    Usable,
    /// It is not there, or what is there is not a whole ELF file. Fetching it
    /// again is worth trying.
    Broken(String),
    /// It is a whole ELF file and this host still refused to run it. Fetching
    /// it again would change nothing.
    Unrunnable { elf: ElfId, why: String },
}

/// The three fields of an ELF header that decide whether a host can run the
/// file: `EI_CLASS`, `EI_DATA` and `e_machine`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ElfId {
    pub class: u8,
    pub data: u8,
    pub machine: u16,
}

impl ElfId {
    /// Reads the identification out of the first bytes of a file.
    ///
    /// The ELF header is 52 bytes for a 32-bit file and 64 for a 64-bit one;
    /// `e_machine` sits at offset 18 in both, so 20 bytes answer the question
    /// for either. Anything shorter than that is not a program.
    pub fn parse(head: &[u8]) -> Result<Self, String> {
        if head.len() < 20 {
            return Err(format!(
                "only {} bytes long, which is not even an ELF header",
                head.len()
            ));
        }
        if head[..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(format!(
                "does not start with the ELF magic (got {:02x?})",
                &head[..4]
            ));
        }
        Ok(Self {
            class: head[4],
            data: head[5],
            machine: u16::from_le_bytes([head[18], head[19]]),
        })
    }

    /// How wide the file is, in words a message can use.
    pub fn width(&self) -> &'static str {
        match self.class {
            1 => "32-bit",
            2 => "64-bit",
            _ => "of unknown width",
        }
    }

    /// The machine the file is for, for the machines this build can meet.
    pub fn machine_name(&self) -> &'static str {
        match self.machine {
            0x03 => "i386",
            0x3e => "x86-64",
            0x28 => "arm",
            0xb7 => "aarch64",
            0xf3 => "riscv",
            _ => "an unknown machine",
        }
    }
}

impl Compiler {
    /// The whole message the user gets. It has to name the file, say what is
    /// wrong with it in one line, and say what to do about it -- because the
    /// alternative, historically, was busybox's make quoting an ELF header.
    pub fn explain(&self, gcc: &Path, var: &str) -> String {
        match self {
            Self::Usable => format!("{} is usable", gcc.display()),
            Self::Broken(why) => format!(
                "The cross compiler {} is {why}.\n\
                 Delete {} and run the build again to fetch it afresh.",
                gcc.display(),
                gcc.display()
            ),
            Self::Unrunnable { elf, why } => format!(
                "The cross compiler {} is a complete {} {} ELF file and this \
                 host still would not run it: {why}.\n\
                 Downloading it again will not change that -- the toolchain \
                 published for this build is {} {}, so a host that cannot \
                 execute those cannot use it. Build the image on a host that \
                 can, or point {var} at a directory holding \
                 bin/<arch>-linux-musl-gcc that this host can run.",
                gcc.display(),
                elf.width(),
                elf.machine_name(),
                elf.width(),
                elf.machine_name(),
            ),
        }
    }
}

/// Decides what the file at `gcc` is: missing, half-written, unrunnable here,
/// or a working compiler.
///
/// The last question is only answerable by running it, so it is run --
/// `-dumpmachine` is the cheapest thing gcc does and it needs no input file.
/// A host that cannot execute the binary fails at `spawn`, which is where the
/// kernel's refusal (`ENOEXEC`, or `EINVAL` from a loader that only takes
/// 64-bit images) arrives without a shell in between to turn it into a syntax
/// error.
fn check_compiler(gcc: &Path) -> Compiler {
    let mut head = [0u8; 20];
    let read = match std::fs::File::open(gcc) {
        Ok(mut f) => {
            let mut n = 0;
            loop {
                match f.read(&mut head[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return Compiler::Broken(format!("unreadable: {e}")),
                }
                if n == head.len() {
                    break;
                }
            }
            n
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Compiler::Broken("not there".into())
        }
        Err(e) => return Compiler::Broken(format!("unreadable: {e}")),
    };

    let elf = match ElfId::parse(&head[..read]) {
        Ok(elf) => elf,
        Err(why) => return Compiler::Broken(why),
    };

    match std::process::Command::new(gcc).arg("-dumpmachine").output() {
        Ok(out) if out.status.success() => Compiler::Usable,
        Ok(out) => Compiler::Unrunnable {
            elf,
            why: format!(
                "it exited with {}",
                out.status
                    .code()
                    .map_or("a signal".into(), |c| c.to_string())
            ),
        },
        Err(e) => Compiler::Unrunnable {
            elf,
            why: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;

impl FromStr for Arch {
    type Err = XError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "riscv64" => Ok(Self::Riscv64),
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            _ => Err(XError::EnumParse {
                type_name: "Arch",
                value: s.into(),
            }),
        }
    }
}

#[derive(Clone, Copy, Args)]
pub(crate) struct ArchArg {
    /// Build architecture, `riscv64` or `x86_64`.
    #[clap(short, long)]
    pub arch: Arch,

    /// Image variant: `desktop` (labwc/Xorg + the whole apk closure) or
    /// `minimal` (console only). Each variant has its own rootfs directory and
    /// its own artifacts, so building one never disturbs the other.
    ///
    /// Defaults to `desktop`, which is where every historical path lives — so
    /// every command that does not name a variant behaves as it always did.
    #[clap(long, default_value = "desktop")]
    pub variant: Variant,
}

impl ArchArg {
    /// Returns the [`LinuxRootfs`] object related to selected architecture
    /// and variant.
    #[inline]
    pub fn linux_rootfs(&self) -> LinuxRootfs {
        LinuxRootfs::with_variant(self.arch, self.variant)
    }
}
