//! 支持架构的定义。

use crate::{
    commands::{optional_status, wget_optional},
    variant::Variant,
    LinuxRootfs, XError, ARCHS, TARGET,
};
use os_xtask_utils::{dir, CommandExt, Ext};
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
        let dir = target.join(&name);
        let gcc = self.musl_gcc(&dir);

        // An explicit toolchain wins over every download, and goes through
        // exactly the same check. It is the way out for a host none of the
        // published toolchains run on.
        if let Some(home) = std::env::var_os(MUSL_CROSS_HOME) {
            let home = PathBuf::from(home);
            match check_compiler(&self.musl_gcc(&home)) {
                Compiler::Usable => return home,
                bad => panic!("{}", bad.explain(&self.musl_gcc(&home), MUSL_CROSS_HOME)),
            }
        }

        dir::create_parent(&dir).unwrap();
        // Whatever was left from a previous build, if it still runs.
        if let Compiler::Usable = check_compiler(&gcc) {
            return dir;
        }

        // A compiler already on this host that targets the same musl triple
        // beats anything downloaded: it is native, so it runs by definition.
        // This is what makes building from inside Eclipse work -- its rootfs
        // ships `apk` pointed at Alpine, and Alpine's `gcc` *is* an
        // `x86_64-...-musl` compiler.
        if let Some(native) = self.native_musl(&target) {
            return native;
        }

        let mut refused = Vec::new();
        for source in Source::ALL {
            let tgz = origin.join(source.tarball(&name));
            // Nothing cached here is trusted: the directory holds a compiler
            // that already failed the check above, and a tarball `wget` would
            // skip re-downloading may be the one that produced it.
            let _ = dir::rm(&dir);
            if !wget_optional(source.url(&name), &tgz) {
                refused.push(format!("{}: could not be downloaded", source.url(&name)));
                let _ = dir::rm(&tgz);
                continue;
            }
            if !optional_status(
                Ext::new("tar").arg("xf").arg(&tgz).arg("-C").arg(&target),
                "needed to unpack the musl toolchain",
            ) {
                refused.push(format!("{}: could not be unpacked", tgz.display()));
                let _ = dir::rm(&tgz);
                continue;
            }
            match check_compiler(&gcc) {
                Compiler::Usable => return dir,
                // Not this host's toolchain, or it arrived broken. Either way
                // the next source gets its turn, and this tarball goes, so a
                // later run does not re-extract it from the cache.
                bad => {
                    refused.push(format!("{}: {}", source.url(&name), bad.one_line(&gcc)));
                    let _ = dir::rm(&tgz);
                }
            }
        }

        let _ = dir::rm(&dir);
        panic!(
            "No usable {name} toolchain. Tried, in order:\n  {}\n\n\
             Point {MUSL_CROSS_HOME} at a directory holding \
             bin/{}-linux-musl-gcc that this host can run.",
            refused.join("\n  "),
            self.name(),
        );
    }

    /// The one file out of the whole toolchain that every caller runs.
    #[inline]
    fn musl_gcc(&self, dir: &Path) -> PathBuf {
        dir.join("bin")
            .join(format!("{}-linux-musl-gcc", self.name()))
    }

    /// A toolchain already installed on this host that can build musl
    /// binaries, presented in the layout every caller expects: a directory
    /// whose `bin/` holds `<arch>-linux-musl-<tool>`.
    ///
    /// Three shapes count, in this order:
    ///
    /// 1. A cross toolchain installed under its own name, `<arch>-linux-musl-gcc`
    ///    on `PATH`. Its whole `bin/` is already spelled the way the build
    ///    wants, so its prefix directory is handed back as it is.
    /// 2. A **native** musl compiler -- plain `gcc` whose `-dumpmachine` is
    ///    something like `x86_64-alpine-linux-musl`.
    /// 3. `musl-gcc`, the wrapper `musl-tools` installs on Debian and Ubuntu.
    ///    This is the one that matters inside Eclipse, whose x86_64 and aarch64
    ///    rootfs is Ubuntu with glibc (`apt_rootfs::use_apt`) while busybox is
    ///    still built `-static` against musl on purpose.
    ///
    /// For 2 and 3 a directory of links named with the prefix is built under
    /// `target/<arch>/`, because `CROSS_COMPILE` is a prefix and nothing in the
    /// build asks for a tool by its bare name.
    ///
    /// A glibc host with no musl anything is not a match, and the download
    /// happens exactly as before. That is why the CI runners are unaffected.
    fn native_musl(&self, target: &Path) -> Option<PathBuf> {
        let prefix = format!("{}-linux-musl-", self.name());
        let prefixed = format!("{prefix}gcc");

        // 1. A whole cross toolchain, already named the way the build wants.
        if let Some(path) = which(&prefixed) {
            if targets_musl(&path, self.name()) {
                println!(
                    "using the {prefix}toolchain already on PATH: {}",
                    path.display()
                );
                return path.parent().and_then(Path::parent).map(Path::to_path_buf);
            }
        }

        // 2. A native musl compiler, or 3. the `musl-tools` wrapper.
        //
        // The wrapper is `exec gcc "$@" -specs .../musl-gcc.specs`, so
        // `-dumpmachine` answers for the gcc underneath and says `-gnu`. Its
        // name is what identifies it, and unlike `x86_64-linux-musl-gcc` --
        // which names a 32-bit i386 binary -- that name is a package's, not a
        // mirror's guess. It is still asked for its arch, so an x86_64 wrapper
        // is never used to build for aarch64.
        for (candidate, cxx) in [("gcc", true), ("cc", true), ("musl-gcc", false)] {
            let Some(cc) = which(candidate) else { continue };
            let native_musl = targets_musl(&cc, self.name());
            if !native_musl && !(candidate == "musl-gcc" && builds_for(&cc, self.name())) {
                continue;
            }
            // A `musl-tools` wrapper has no `musl-g++`: linking the system
            // `g++` under a musl name would hand a C++ caller a glibc compiler
            // that silently builds the wrong thing. C only, then.
            let with_cxx = cxx && native_musl;
            if let Some(home) = self.link_farm(target, &prefix, &cc, with_cxx) {
                println!(
                    "using this host's musl compiler: {} (C++ {})",
                    cc.display(),
                    if with_cxx {
                        "included"
                    } else {
                        "not available"
                    }
                );
                return Some(home);
            }
        }
        None
    }

    /// Builds `target/<arch>/native-musl/bin/<prefix><tool>` for every tool
    /// this host has, with `gcc` and `cc` pointing at `cc` itself rather than
    /// at whatever shares its directory -- for the `musl-gcc` wrapper those are
    /// not the same program, and the one beside it is the glibc one.
    fn link_farm(&self, target: &Path, prefix: &str, cc: &Path, with_cxx: bool) -> Option<PathBuf> {
        let bin = cc.parent()?;
        let home = target.join("native-musl");
        let link_dir = home.join("bin");
        dir::clear(&link_dir).ok()?;

        let mut linked = 0;
        for tool in TOOLS.iter().chain(if with_cxx { CXX_TOOLS } else { &[] }) {
            let from = match *tool {
                "gcc" | "cc" => cc.to_path_buf(),
                other => bin.join(other),
            };
            if !from.exists() {
                continue;
            }
            if std::os::unix::fs::symlink(&from, link_dir.join(format!("{prefix}{tool}"))).is_ok() {
                linked += 1;
            }
        }
        // The compiler itself has to be there, or this is not a toolchain.
        link_dir
            .join(format!("{prefix}gcc"))
            .exists()
            .then_some(())?;
        println!("{linked} tools linked into {}", link_dir.display());
        Some(home)
    }
}

/// Points at a directory holding `bin/<arch>-linux-musl-gcc`, used instead of
/// the downloaded toolchain.
const MUSL_CROSS_HOME: &str = "MUSL_CROSS_HOME";

/// Where a prebuilt musl cross toolchain can be fetched from.
///
/// There is more than one because the original mirror's toolchains are
/// **32-bit i386** binaries -- `x86_64-linux-musl-gcc-11.2.1` is
/// `ELF 32-bit LSB executable, Intel 80386`, despite the `x86_64-` naming,
/// which is the target triple and says nothing about the host. That is fine on
/// a host with 32-bit x86 support, which every CI runner has, and impossible on
/// one without: Eclipse's own ELF loader takes 64-bit images only, so building
/// the image *from inside Eclipse* got as far as busybox and then handed
/// `/bin/sh` a compiler it could not `execve`.
///
/// So the sources are tried in order and the first one that yields a compiler
/// this host actually runs wins. Each keeps its own tarball under `archs/`, so
/// a cached download for one is never mistaken for the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A 64-bit-hosted build published on this project's own releases.
    Eclipse64,
    /// The historical mirror: 32-bit i386 hosted.
    ZcoreMusl,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::Eclipse64, Self::ZcoreMusl];

    /// The tarball to fetch, for the toolchain named `name`.
    pub fn url(&self, name: &str) -> String {
        match self {
            Self::Eclipse64 => format!(
                "https://github.com/Pryancito/eclipse/releases/download/musl-cross-64/{name}.tgz"
            ),
            Self::ZcoreMusl => format!(
                "https://github.com/YdrMaster/zCore/releases/download/musl-cache/{name}.tgz"
            ),
        }
    }

    /// Its file name under `archs/<arch>/`. The two must differ, or the second
    /// source would find the first one's failed download already cached.
    pub fn tarball(&self, name: &str) -> String {
        match self {
            Self::Eclipse64 => format!("{name}-64.tgz"),
            Self::ZcoreMusl => format!("{name}.tgz"),
        }
    }
}

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
    /// The same thing in one line, for a list of sources that were tried.
    pub fn one_line(&self, gcc: &Path) -> String {
        match self {
            Self::Usable => "usable".into(),
            Self::Broken(why) => format!("{} is {why}", gcc.display()),
            Self::Unrunnable { elf, why } => format!(
                "holds a {} {} compiler this host will not run ({why})",
                elf.width(),
                elf.machine_name(),
            ),
        }
    }

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

/// The tools `CROSS_COMPILE=<prefix>` makes a makefile reach for. Whichever of
/// them the host has get a prefixed name; the rest are simply not there, which
/// is what a cross toolchain missing one would look like anyway.
const TOOLS: [&str; 12] = [
    "gcc", "cc", "ld", "as", "ar", "ranlib", "nm", "strip", "objcopy", "objdump", "readelf", "size",
];

/// The C++ half, linked only when the compiler found is a real musl-targeting
/// toolchain. The `musl-tools` wrapper has no C++ counterpart, and the `g++`
/// beside it builds against glibc.
const CXX_TOOLS: &[&str] = &["g++", "c++", "cpp"];

/// Where `name` resolves on `PATH`, or `None` if it is not there. There is no
/// `which` in the standard library and no crate for it in this build, and
/// `PATH` lookup is three lines.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether the compiler at `cc` builds for musl on `arch`.
///
/// `-dumpmachine` is the compiler's own answer and the only one worth trusting:
/// a name says nothing (the published `x86_64-linux-musl-gcc` is a 32-bit i386
/// binary) and neither does the host distribution. Alpine's native compiler
/// answers `x86_64-alpine-linux-musl`, a cross one `x86_64-linux-musl`, and a
/// glibc one `x86_64-pc-linux-gnu`, which is not a match.
fn targets_musl(cc: &Path, arch: &str) -> bool {
    dumpmachine(cc).is_some_and(|t| t.starts_with(&format!("{arch}-")) && t.ends_with("-musl"))
}

/// The compiler's own answer about what it builds, or `None` if it cannot be
/// asked. `-dumpmachine` is the only answer worth trusting about a toolchain:
/// a name says nothing (the published `x86_64-linux-musl-gcc` is a 32-bit i386
/// binary) and neither does the host distribution.
fn dumpmachine(cc: &Path) -> Option<String> {
    let out = std::process::Command::new(cc)
        .arg("-dumpmachine")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether the compiler at `cc` builds for `arch` at all, whatever its libc.
///
/// This is the weaker question, asked only of `musl-gcc`, whose libc cannot be
/// read off `-dumpmachine` because the wrapper reports the gcc underneath it.
fn builds_for(cc: &Path, arch: &str) -> bool {
    dumpmachine(cc).is_some_and(|t| t.starts_with(&format!("{arch}-")))
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
