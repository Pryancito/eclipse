//! What `execve` has to refuse while the caller is still there to be told.
//!
//! `sys_execve` has a point of no return: `vmar.clear()` destroys the calling
//! process's address space, and an error the loader raises after it has
//! nobody to go back to, so the process is killed with `SIGSEGV` instead.
//! That is right for what only loading can find out (memory running out half
//! way through a mapping). It was wrong for everything else, because the
//! loader was also where every *format* error was first noticed: a file of
//! garbage, a script with no `#!` line, a `#!` with nothing after it, a
//! directory, a binary for another machine, a dynamic binary whose
//! `PT_INTERP` is not installed. Each of those killed the program that asked,
//! where Linux hands back an errno and the program carries on.
//!
//! That is not a cosmetic difference. A shell's whole fallback rests on the
//! errno: `ash` and `bash` run a file `execve` answers with `ENOEXEC` as a
//! shell script, which is how a script without a `#!` line runs at all, and
//! they print "not found" for the `ENOENT` of a glibc binary on a musl system
//! (its `/lib64/ld-linux-x86-64.so.2` is not there). Killed instead, the
//! script never ran and the shell reported a segmentation fault.
//!
//! [`LinuxElfLoader::preflight`] asks every one of those questions first,
//! reading only headers, and in the order `fs/exec.c`, `fs/binfmt_script.c`
//! and `fs/binfmt_elf.c` ask them before `begin_new_exec` commits:
//!
//! | what the file is                          | errno     |
//! |-------------------------------------------|-----------|
//! | neither `#!` nor an ELF this machine runs | `ENOEXEC` |
//! | a `#!` line with no interpreter on it     | `ENOEXEC` |
//! | an interpreter that is not a regular file | `EACCES`  |
//! | an interpreter the caller may not execute | `EACCES`  |
//! | an interpreter in a directory the caller may not search | `EACCES` |
//! | a sixth `#!` script in one chain          | `ELOOP`   |
//! | `PT_INTERP` not NUL-terminated, or empty  | `ENOEXEC` |
//! | `PT_INTERP` naming nothing                | `ENOENT`  |
//! | `PT_INTERP` shorter than an ELF header    | `EIO`     |
//! | `PT_INTERP` not an ELF for this machine   | `ELIBBAD` |
//! | `PT_INTERP` of the other class than the program | `ELIBBAD` |
//!
//! It also measures every segment and the section table against the file,
//! which Linux does not do: [`check_elf_bounds`] does, after the line, so a
//! truncated binary was one more way to be killed. Here it is `ENOEXEC`.

use super::*;
use crate::error::LxError;
use crate::process::{Credentials, LinuxProcess, ACCESS_EXEC};
use rcore_fs::vfs::FileType;

/// `e_machine` of the images this kernel runs: `elf_check_arch`.
#[cfg(target_arch = "x86_64")]
const EM_HOST: u16 = 62; // EM_X86_64
#[cfg(target_arch = "aarch64")]
const EM_HOST: u16 = 183; // EM_AARCH64
#[cfg(target_arch = "riscv64")]
const EM_HOST: u16 = 243; // EM_RISCV

/// `EM_386`: the 32-bit image an x86_64 kernel also runs, in the CPU's
/// compatibility mode. Linux's `compat_elf_check_arch`.
#[cfg(target_arch = "x86_64")]
const EM_I386: u16 = 3;

/// Whether `(class, machine)` name an image this kernel will start.
///
/// Two answers on x86_64, one everywhere else: `elf_check_arch` for the
/// native class, and `compat_elf_check_arch` for the 32-bit one. The pair has
/// to be judged together -- an `ELFCLASS32` file claiming `EM_X86_64`, or an
/// `ELFCLASS64` one claiming `EM_386`, is neither, and each would send the
/// loader to read a header of the wrong shape.
fn runnable_here(class: u8, machine: u16) -> bool {
    #[cfg(target_arch = "x86_64")]
    if class == ELFCLASS32 && machine == EM_I386 {
        return true;
    }
    class == ELFCLASS64 && machine == EM_HOST
}

/// `e_type` of an image `execve` will start: a fixed-address executable or a
/// position-independent one.
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_INTERP: u32 = 3;
/// Where `EI_CLASS` sits in the identification bytes, and the two values this
/// loader reads, with the `EI_DATA` that goes with both: little-endian (see
/// [`check_elf_bounds`]).
const EI_CLASS: usize = 4;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Size of an ELF64 header, of one program header and of one section header.
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const SHDR_SIZE: u64 = 64;

/// The same three for an ELF32 image.
const EHDR_SIZE_32: usize = 52;
const PHDR_SIZE_32: usize = 32;
const SHDR_SIZE_32: u64 = 40;

/// The largest program header table `load_elf_phdrs` reads, in bytes:
/// `ELF_MIN_ALIGN`, one page. Seventy-three entries.
const MAX_PHDR_TABLE: usize = 4096;

/// The longest `PT_INTERP` path `load_elf_binary` takes, terminator included:
/// `PATH_MAX`.
const MAX_INTERP_PATH: u64 = 4096;

/// The fields of an ELF header the preflight reads, whatever its class.
///
/// `class` is kept because every later read -- the size of one program
/// header, where `p_offset` and `p_filesz` sit inside one -- depends on it,
/// and because it is half of what says whether this kernel runs the image at
/// all (see [`runnable_here`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    class: u8,
    kind: u16,
    machine: u16,
    ph_offset: u64,
    sh_offset: u64,
    ph_entry_size: u16,
    ph_count: u16,
    sh_entry_size: u16,
    sh_count: u16,
}

impl Header {
    /// Size of one program header of this image's class.
    fn phdr_size(&self) -> usize {
        if self.class == ELFCLASS32 {
            PHDR_SIZE_32
        } else {
            PHDR_SIZE
        }
    }

    /// Smallest `e_shentsize` this image's class can have.
    fn shdr_size(&self) -> u64 {
        if self.class == ELFCLASS32 {
            SHDR_SIZE_32
        } else {
            SHDR_SIZE
        }
    }

    /// `p_offset` and `p_filesz` of the program header `ph`, whose length is
    /// this image's [`Self::phdr_size`].
    ///
    /// The two classes agree on `p_type` at offset 0 and on nothing else: an
    /// ELF32 header puts `p_offset` at 4 and `p_filesz` at 16, both four bytes
    /// wide, where an ELF64 one puts them at 8 and 32, eight bytes wide.
    /// Reading an ELF32 table with the 64-bit offsets is how a well-formed
    /// i386 binary looks like a corrupt one.
    fn segment_extent(&self, ph: &[u8]) -> (u64, u64) {
        if self.class == ELFCLASS32 {
            (u64::from(u32_at(ph, 4)), u64::from(u32_at(ph, 16)))
        } else {
            (u64_at(ph, 8), u64_at(ph, 32))
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// The header at the start of `head`, if `head` starts with one this loader
/// can read at all: the magic, a class it knows, little-endian, and a whole
/// header of that class.
///
/// The two classes share their first eighteen bytes -- magic, `EI_CLASS`,
/// `EI_DATA`, `e_type`, `e_machine` -- and then part company: every offset
/// after that is four bytes earlier in an ELF32 header, and the offset fields
/// are four bytes wide instead of eight. Nothing here judges *which* class the
/// kernel runs; that is [`runnable_here`], asked together with `e_machine`.
fn elf_header(head: &[u8]) -> Option<Header> {
    if head.len() < 6 || head[..4] != *b"\x7fELF" {
        return None;
    }
    if head[5] != ELFDATA2LSB {
        return None;
    }
    let class = head[EI_CLASS];
    let (header_size, ph_offset_at, ph_entry_size_at, offset_width) = match class {
        ELFCLASS32 => (EHDR_SIZE_32, 28, 42, 4),
        ELFCLASS64 => (EHDR_SIZE, 32, 54, 8),
        _ => return None,
    };
    if head.len() < header_size {
        return None;
    }
    let offset_at = |at: usize| -> u64 {
        if offset_width == 4 {
            u64::from(u32_at(head, at))
        } else {
            u64_at(head, at)
        }
    };
    Some(Header {
        class,
        kind: u16_at(head, 16),
        machine: u16_at(head, 18),
        ph_offset: offset_at(ph_offset_at),
        sh_offset: offset_at(ph_offset_at + offset_width),
        ph_entry_size: u16_at(head, ph_entry_size_at),
        ph_count: u16_at(head, ph_entry_size_at + 2),
        sh_entry_size: u16_at(head, ph_entry_size_at + 4),
        sh_count: u16_at(head, ph_entry_size_at + 6),
    })
}

/// Whether `offset .. offset + len` lies inside a file of `size` bytes,
/// without letting the sum wrap first.
fn fits(offset: u64, len: u64, size: u64) -> bool {
    offset.checked_add(len).is_some_and(|end| end <= size)
}

/// Read from `offset` until `buf` is full or the file ends, and say how much
/// was read.
///
/// A `read_at` may stop short of what it was asked for and still not be at
/// the end -- `read_as_vmo` loops for the same reason -- and taking one short
/// read for the end of the file would refuse a good program.
fn read_full(file: &Arc<dyn INode>, offset: usize, buf: &mut [u8]) -> LxResult<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(offset + done, &mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

/// `do_open_execat`, which `open_exec` opens an interpreter with, as
/// `execve` opens the program: `MAY_EXEC` for the caller, and then `if
/// (!S_ISREG(...)) return -EACCES`. A directory or a device named as an
/// interpreter is refused like the image itself would be, and so is one the
/// caller may not execute -- root included, when it has no execute bit at
/// all. That half was missing: a `chmod -x` interpreter still ran.
fn opened_for_exec(file: &Arc<dyn INode>, creds: &Credentials) -> LxResult<()> {
    let m = file.metadata()?;
    let (uid, gid, is_dir) = (m.uid as u32, m.gid as u32, m.type_ == FileType::Dir);
    LinuxProcess::access_verdict(creds, uid, gid, m.mode, is_dir, ACCESS_EXEC, true)?;
    if m.type_ == FileType::File {
        Ok(())
    } else {
        Err(LxError::EACCES)
    }
}

/// Check `file`, whose first bytes are `head`, as an image this machine can
/// run, and hand back its program header table.
///
/// `bad` is the errno for every way it is not one: `ENOEXEC` for the image,
/// `ELIBBAD` for its interpreter -- the split `load_elf_binary` makes.
fn image_phdrs(file: &Arc<dyn INode>, head: &[u8], bad: LxError) -> LxResult<(Header, Vec<u8>)> {
    let header = elf_header(head).ok_or(bad)?;
    if !matches!(header.kind, ET_EXEC | ET_DYN) || !runnable_here(header.class, header.machine) {
        return Err(bad);
    }
    // `load_elf_phdrs`.
    let phdr_size = header.phdr_size();
    let table = usize::from(header.ph_count) * phdr_size;
    if usize::from(header.ph_entry_size) != phdr_size || table == 0 || table > MAX_PHDR_TABLE {
        return Err(bad);
    }
    // Measured before it is read: an offset the file chose is not an offset
    // every filesystem's `read_at` is ready for.
    let size = file.metadata()?.size as u64;
    if !fits(header.ph_offset, table as u64, size) {
        return Err(bad);
    }
    // What `check_elf_bounds` will refuse on the other side of the line.
    if header.sh_count != 0
        && (u64::from(header.sh_entry_size) < header.shdr_size()
            || !fits(
                header.sh_offset,
                u64::from(header.sh_count) * u64::from(header.sh_entry_size),
                size,
            ))
    {
        return Err(bad);
    }
    let mut phdrs = vec![0u8; table];
    if read_full(file, header.ph_offset as usize, &mut phdrs)? != table {
        return Err(bad);
    }
    for ph in phdrs.chunks_exact(phdr_size) {
        let (offset, filesz) = header.segment_extent(ph);
        if !fits(offset, filesz, size) {
            return Err(bad);
        }
    }
    Ok((header, phdrs))
}

impl LinuxElfLoader {
    /// Refuse, with the errno Linux gives, every `execve` of `image` the
    /// loader would fail on for a reason in the files themselves -- without
    /// loading or mapping anything. See the [module documentation](self).
    ///
    /// `image` is the file the caller named, already checked for search and
    /// execute permission and found to be a regular file, and `creds` the
    /// caller's, which every interpreter is opened for execution with. The
    /// `#!` chain is walked from it, every interpreter is looked up and
    /// checked as the loader will look it up, and the ELF at the end of it is
    /// checked together with its `PT_INTERP`.
    pub fn preflight(&self, image: &Arc<dyn INode>, creds: &Credentials) -> LxResult<()> {
        let mut file = image.clone();
        let mut buf = [0u8; 512];
        // `exec_binprm`: `if (depth > 5) return -ELOOP;`, asked before each
        // file of the chain is looked at, so the interpreter a sixth script
        // names is still looked up (and an ENOENT for it still wins).
        for depth in 0.. {
            if depth > Self::MAX_INTERP_DEPTH {
                return Err(LxError::ELOOP);
            }
            let n = read_full(&file, 0, &mut buf)?;
            let head = &buf[..n];
            if !head.starts_with(b"#!") {
                let (header, phdrs) = image_phdrs(&file, head, LxError::ENOEXEC)?;
                return self.preflight_pt_interp(&file, &header, &phdrs, creds);
            }
            // `load_script`: "No interpreter name found".
            let interp = shebang_interp(head).ok_or(LxError::ENOEXEC)?;
            let next = self.lookup_interp(interp).inspect_err(|e| {
                warn!("execve: interpreter {:?} does not resolve: {:?}", interp, e);
            })?;
            opened_for_exec(&next, creds)?;
            file = next;
        }
        unreachable!("the chain ends at a depth `MAX_INTERP_DEPTH` bounds")
    }

    /// The dynamic linker `image` names, checked as `load_elf_binary` checks
    /// it before `begin_new_exec`: looked up, a regular file, and the header
    /// and program headers of an ELF for this machine.
    fn preflight_pt_interp(
        &self,
        image: &Arc<dyn INode>,
        header: &Header,
        phdrs: &[u8],
        creds: &Credentials,
    ) -> LxResult<()> {
        // The first `PT_INTERP`, as `get_interpreter` finds it. `p_type` is at
        // offset 0 in both classes; the extent is not, which is what `header`
        // is for.
        let ph = match phdrs
            .chunks_exact(header.phdr_size())
            .find(|ph| u32_at(ph, 0) == PT_INTERP)
        {
            Some(ph) => ph,
            None => return Ok(()),
        };
        let (offset, len) = header.segment_extent(ph);
        if !(2..=MAX_INTERP_PATH).contains(&len) {
            return Err(LxError::ENOEXEC);
        }
        let mut path = vec![0u8; len as usize];
        // `elf_read`: a short read is `EIO`. `image_phdrs` measured the
        // segment against the file, so this is a file that shrank since.
        if read_full(image, offset as usize, &mut path)? != path.len() {
            return Err(LxError::EIO);
        }
        if path.last() != Some(&0) {
            return Err(LxError::ENOEXEC);
        }
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        // No file in this VFS has a name that is not UTF-8, nor an empty one.
        let interp = match core::str::from_utf8(&path[..end]) {
            Ok(interp) if !interp.is_empty() => interp,
            _ => return Err(LxError::ENOENT),
        };
        let file = self.lookup_interp(interp).inspect_err(|e| {
            warn!("execve: PT_INTERP {:?} does not resolve: {:?}", interp, e);
        })?;
        opened_for_exec(&file, creds)?;
        let mut head = [0u8; EHDR_SIZE];
        let read = read_full(&file, 0, &mut head)?;
        // `elf_read`: a short read is `EIO`. How much is short depends on the
        // class the file claims -- 52 bytes of header for an ELF32 image, 64
        // for an ELF64 one -- and a file too small to say which is short
        // either way.
        let want = if read > EI_CLASS && head[EI_CLASS] == ELFCLASS32 {
            EHDR_SIZE_32
        } else {
            EHDR_SIZE
        };
        if read < want {
            return Err(LxError::EIO);
        }
        let (interp_header, _) = image_phdrs(&file, &head[..read], LxError::ELIBBAD)?;
        // The interpreter has to be of the program's own flavour, and not
        // merely of one this kernel runs. `load_elf_binary` asks
        // `elf_check_arch` and `EI_CLASS != ELF_CLASS` of the interpreter with
        // the macros of the binfmt handler the MAIN program chose, so under
        // `compat_binfmt_elf` -- an `EM_386` program -- both mean the 32-bit
        // ones, and a 64-bit `ld.so` fails them. [`runnable_here`] cannot see
        // that: on x86_64 it says yes to either class, which is right for the
        // program and too wide for its interpreter.
        //
        // Measured on a Linux 6.18 host, both ways round: a 32-bit `ET_EXEC`
        // whose `PT_INTERP` names an x86_64 image, and an x86_64 program whose
        // `PT_INTERP` was patched to a 32-bit one, both give ELIBBAD
        // ("Accessing a corrupted shared library"), never a load.
        if (interp_header.class, interp_header.machine) != (header.class, header.machine) {
            warn!(
                "execve: PT_INTERP {:?} is class {} machine {}, the program is class {} machine {}",
                interp, interp_header.class, interp_header.machine, header.class, header.machine
            );
            return Err(LxError::ELIBBAD);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Every row of the table in the module documentation, each errno as a
    //! Linux host answers the same file (measured with the `execve` survey of
    //! `tools/signal-probe` run natively), and the files the preflight must
    //! let through: refusing a good image would turn every exec into an errno.

    use super::*;
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    const PT_LOAD: u32 = 1;

    /// A machine this kernel does not run.
    const EM_OTHER: u16 = if EM_HOST == 62 { 183 } else { 62 };

    fn put16(v: &mut [u8], at: usize, x: u16) {
        v[at..at + 2].copy_from_slice(&x.to_le_bytes());
    }

    fn put32(v: &mut [u8], at: usize, x: u32) {
        v[at..at + 4].copy_from_slice(&x.to_le_bytes());
    }

    fn put64(v: &mut [u8], at: usize, x: u64) {
        v[at..at + 8].copy_from_slice(&x.to_le_bytes());
    }

    /// Where program header `i` of an [`image`] starts.
    fn phdr(i: usize) -> usize {
        EHDR_SIZE + i * PHDR_SIZE
    }

    /// A well-formed image for this machine: a `PT_LOAD` over the whole
    /// file, then one program header for each of `segments`, a type and the
    /// bytes it covers, which follow the table in order.
    fn image(segments: &[(u32, &[u8])]) -> Vec<u8> {
        let count = 1 + segments.len();
        let mut v = vec![0u8; phdr(count)];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = ELFCLASS64;
        v[5] = ELFDATA2LSB;
        v[6] = 1;
        put16(&mut v, 16, ET_EXEC);
        put16(&mut v, 18, EM_HOST);
        put64(&mut v, 32, EHDR_SIZE as u64);
        put16(&mut v, 52, EHDR_SIZE as u16);
        put16(&mut v, 54, PHDR_SIZE as u16);
        put16(&mut v, 56, count as u16);
        for (i, (kind, bytes)) in segments.iter().enumerate() {
            let at = phdr(1 + i);
            let offset = v.len() as u64;
            v.extend_from_slice(bytes);
            put32(&mut v, at, *kind);
            put64(&mut v, at + 8, offset);
            put64(&mut v, at + 32, bytes.len() as u64);
        }
        let len = v.len() as u64;
        put32(&mut v, phdr(0), PT_LOAD);
        put64(&mut v, phdr(0) + 32, len);
        v
    }

    /// A dynamic image whose `PT_INTERP` names `interp`.
    fn dynamic(interp: &str) -> Vec<u8> {
        let mut path = interp.as_bytes().to_vec();
        path.push(0);
        image(&[(PT_INTERP, &path)])
    }

    /// The same, of the 32-bit class this kernel also runs: an `EM_386`
    /// `ET_EXEC` whose header and program headers are the ELF32 shapes, which
    /// put `e_phoff` at 28 and `p_offset`/`p_filesz` at 4 and 16 of a
    /// 32-byte program header. Only x86_64 runs one at all.
    #[cfg(target_arch = "x86_64")]
    fn image_i386(segments: &[(u32, &[u8])]) -> Vec<u8> {
        let count = 1 + segments.len();
        let phdr32 = |i: usize| EHDR_SIZE_32 + i * PHDR_SIZE_32;
        let mut v = vec![0u8; phdr32(count)];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = ELFCLASS32;
        v[5] = ELFDATA2LSB;
        v[6] = 1;
        put16(&mut v, 16, ET_EXEC);
        put16(&mut v, 18, EM_I386);
        put32(&mut v, 28, EHDR_SIZE_32 as u32);
        put16(&mut v, 40, EHDR_SIZE_32 as u16);
        put16(&mut v, 42, PHDR_SIZE_32 as u16);
        put16(&mut v, 44, count as u16);
        for (i, (kind, bytes)) in segments.iter().enumerate() {
            let at = phdr32(1 + i);
            let offset = v.len() as u32;
            v.extend_from_slice(bytes);
            put32(&mut v, at, *kind);
            put32(&mut v, at + 4, offset);
            put32(&mut v, at + 16, bytes.len() as u32);
        }
        let len = v.len() as u32;
        put32(&mut v, phdr32(0), PT_LOAD);
        put32(&mut v, phdr32(0) + 16, len);
        // `p_vaddr`, `p_memsz` and `p_align` as a 32-bit image really carries
        // them, although nothing in the preflight reads any of the three: it
        // measures a segment by `p_offset` and `p_filesz` alone. A zero vaddr
        // with a non-zero `p_filesz` is not a thing a linker emits, and a test
        // image that is only as well-formed as today's reader demands is one
        // that starts failing for the wrong reason the day the reader asks for
        // more (`p_memsz >= p_filesz`, a load not at page zero). 0x08048000 is
        // the base the real probe in `tools/i386-dyn` is linked at.
        put32(&mut v, phdr32(0) + 8, 0x0804_8000);
        put32(&mut v, phdr32(0) + 12, 0x0804_8000);
        put32(&mut v, phdr32(0) + 20, len);
        put32(&mut v, phdr32(0) + 28, 0x1000);
        v
    }

    /// A 32-bit dynamic image whose `PT_INTERP` names `interp`.
    #[cfg(target_arch = "x86_64")]
    fn dynamic_i386(interp: &str) -> Vec<u8> {
        let mut path = interp.as_bytes().to_vec();
        path.push(0);
        image_i386(&[(PT_INTERP, &path)])
    }

    struct Fs {
        root: Arc<dyn INode>,
        /// Held because an inode reaches its filesystem through a weak
        /// reference, and an absolute symlink target is looked up from there.
        _fs: Arc<RamFS>,
    }

    impl Fs {
        /// A root holding the files most cases need: `/ld.so`, a good
        /// interpreter, and `/dir`, a directory.
        fn new() -> Self {
            let ramfs = RamFS::new();
            let fs = Self {
                root: ramfs.root_inode(),
                _fs: ramfs,
            };
            fs.file("ld.so", &image(&[]));
            fs.root.create("dir", FileType::Dir, 0o755).unwrap();
            fs
        }

        fn file(&self, name: &str, bytes: &[u8]) {
            let f = self.root.create(name, FileType::File, 0o755).unwrap();
            f.write_at(0, bytes).unwrap();
        }

        fn symlink(&self, name: &str, target: &str) {
            let l = self.root.create(name, FileType::SymLink, 0o777).unwrap();
            l.write_at(0, target.as_bytes()).unwrap();
        }

        /// `execve` of `/name` by root, as far as the preflight goes.
        fn exec(&self, name: &str) -> LxResult<()> {
            self.exec_as(name, &Credentials::default())
        }

        /// `execve` of `/name` by `creds`, as far as the preflight goes.
        fn exec_as(&self, name: &str, creds: &Credentials) -> LxResult<()> {
            let image = self.root.lookup(name).unwrap();
            LinuxElfLoader {
                syscall_entry: 0,
                stack_pages: 1,
                root_inode: self.root.clone(),
                interp_lookup: None,
                identity: Default::default(),
            }
            .preflight(&image, creds)
        }

        /// A file holding `bytes`, executed.
        fn run(&self, bytes: &[u8]) -> LxResult<()> {
            self.file("x", bytes);
            let got = self.exec("x");
            self.root.unlink("x").unwrap();
            got
        }
    }

    /// A file that hands out at most `step` bytes per `read_at`, as a
    /// filesystem may at a block or extent boundary.
    struct Dribble {
        inner: Arc<dyn INode>,
        step: usize,
    }

    impl INode for Dribble {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            let len = buf.len().min(self.step);
            self.inner.read_at(offset, &mut buf[..len])
        }
        fn write_at(&self, _: usize, _: &[u8]) -> rcore_fs::vfs::Result<usize> {
            unimplemented!()
        }
        fn poll(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::PollStatus> {
            unimplemented!()
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::Metadata> {
            self.inner.metadata()
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    #[test]
    fn a_filesystem_that_reads_a_little_at_a_time_still_runs_programs() {
        let fs = Fs::new();
        fs.file("x", &dynamic("/ld.so"));
        let image: Arc<dyn INode> = Arc::new(Dribble {
            inner: fs.root.lookup("x").unwrap(),
            step: 7,
        });
        let loader = LinuxElfLoader {
            syscall_entry: 0,
            stack_pages: 1,
            root_inode: fs.root.clone(),
            interp_lookup: None,
            identity: Default::default(),
        };
        assert_eq!(loader.preflight(&image, &Credentials::default()), Ok(()));
    }

    /// Every interpreter is looked up by the lookup the loader was given, a
    /// `#!` one and a `PT_INTERP` alike, with its path as written: the
    /// process's lookup is the one that knows the working directory a
    /// relative path starts at and whom to ask for search permission, and
    /// its refusal is the `execve`'s. Without one it is the root, asking
    /// nothing.
    #[test]
    fn every_interpreter_is_looked_up_by_the_lookup_the_loader_was_given() {
        let fs = Fs::new();
        fs.file("script", b"#!as-interp\n");
        fs.file("dynamic", &dynamic("as-interp"));
        let exec_with = |name: &str, lookup: Option<InterpLookup>| {
            LinuxElfLoader {
                syscall_entry: 0,
                stack_pages: 1,
                root_inode: fs.root.clone(),
                interp_lookup: lookup,
                identity: Default::default(),
            }
            .preflight(&fs.root.lookup(name).unwrap(), &Credentials::default())
        };
        let refusing: InterpLookup = Arc::new(|path: &str| {
            assert_eq!(path, "as-interp", "the path reaches the lookup as written");
            Err(LxError::EACCES)
        });
        assert_eq!(
            exec_with("script", Some(refusing.clone())),
            Err(LxError::EACCES)
        );
        assert_eq!(exec_with("dynamic", Some(refusing)), Err(LxError::EACCES));

        let ld = fs.root.lookup("ld.so").unwrap();
        let finding: InterpLookup = Arc::new(move |_: &str| Ok(ld.clone()));
        assert_eq!(exec_with("script", Some(finding.clone())), Ok(()));
        assert_eq!(exec_with("dynamic", Some(finding)), Ok(()));

        assert_eq!(exec_with("script", None), Err(LxError::ENOENT));
        assert_eq!(exec_with("dynamic", None), Err(LxError::ENOENT));
    }

    #[test]
    fn a_static_and_a_dynamic_image_pass() {
        let fs = Fs::new();
        assert_eq!(fs.run(&image(&[])), Ok(()));
        assert_eq!(fs.run(&dynamic("/ld.so")), Ok(()));
        // A position-independent one too, which is most of a distribution.
        let mut pie = dynamic("/ld.so");
        put16(&mut pie, 16, ET_DYN);
        assert_eq!(fs.run(&pie), Ok(()));
    }

    /// The case that broke `ash`: a script without `#!` is the shell's to
    /// run, and it only finds out from ENOEXEC.
    #[test]
    fn a_file_that_is_neither_a_script_nor_an_elf_is_enoexec() {
        let fs = Fs::new();
        for bytes in [&b"exit 3\n"[..], b"\x01\x02garbage\n", b"", b"\x7fEL"] {
            assert_eq!(fs.run(bytes), Err(LxError::ENOEXEC), "{:?}", bytes);
        }
        // An ELF cut short of its own header, at every length: the fields
        // are read by index, and a short one must not reach them.
        let whole = image(&[]);
        for len in 0..EHDR_SIZE {
            assert_eq!(fs.run(&whole[..len]), Err(LxError::ENOEXEC), "{}", len);
        }
    }

    #[test]
    fn an_elf_this_machine_does_not_run_is_enoexec() {
        let fs = Fs::new();
        let mut other = image(&[]);
        put16(&mut other, 18, EM_OTHER);
        assert_eq!(fs.run(&other), Err(LxError::ENOEXEC));
        let mut class32 = image(&[]);
        class32[4] = 1;
        assert_eq!(fs.run(&class32), Err(LxError::ENOEXEC));
        let mut big_endian = image(&[]);
        big_endian[5] = 2;
        assert_eq!(fs.run(&big_endian), Err(LxError::ENOEXEC));
        // A relocatable object and a core dump are ELF files, not programs.
        for kind in [0u16, 1, 4] {
            let mut v = image(&[]);
            put16(&mut v, 16, kind);
            assert_eq!(fs.run(&v), Err(LxError::ENOEXEC), "e_type {}", kind);
        }
    }

    /// `load_elf_phdrs`: an entry size other than its own, no table, or one
    /// over a page.
    #[test]
    fn a_program_header_table_linux_would_not_read_is_enoexec() {
        let fs = Fs::new();
        let mut v = image(&[]);
        put16(&mut v, 54, PHDR_SIZE as u16 + 8);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
        let mut v = image(&[]);
        put16(&mut v, 56, 0);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));

        // 73 entries is the most that fit in 4096 bytes, and 74 is refused.
        let fits = MAX_PHDR_TABLE / PHDR_SIZE;
        assert_eq!(fits, 73);
        let mut v = image(&[]);
        v.resize(phdr(fits + 1), 0);
        let len = v.len() as u64;
        put64(&mut v, phdr(0) + 32, len);
        put16(&mut v, 56, fits as u16);
        assert_eq!(fs.run(&v), Ok(()));
        put16(&mut v, 56, fits as u16 + 1);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
    }

    /// A truncated binary -- an interrupted copy -- used to be killed by
    /// `check_elf_bounds` on the far side of the line. Every extent the
    /// loader measures is measured here first.
    #[test]
    fn an_image_that_does_not_hold_what_its_headers_say_is_enoexec() {
        let fs = Fs::new();
        let whole = dynamic("/ld.so");
        // Cut anywhere past the header: a segment, or the table, runs out.
        for len in [EHDR_SIZE, phdr(1), whole.len() - 1] {
            assert_eq!(fs.run(&whole[..len]), Err(LxError::ENOEXEC), "{}", len);
        }
        // A table offset the sum would wrap on is refused, not read.
        let mut v = image(&[]);
        put64(&mut v, 32, u64::MAX - 8);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
        // A segment that ends past the file, or wraps.
        let mut v = image(&[]);
        put64(&mut v, phdr(0) + 8, 1);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
        let mut v = image(&[]);
        put64(&mut v, phdr(0) + 8, u64::MAX);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
    }

    #[test]
    fn a_section_table_is_measured_only_when_there_is_one() {
        let fs = Fs::new();
        // `sstrip` and UPX leave `e_shnum` at zero and `e_shoff` anything.
        let mut v = image(&[]);
        put64(&mut v, 40, u64::MAX);
        assert_eq!(fs.run(&v), Ok(()));
        // One entry, inside the file: the first 64 bytes, the header itself.
        put64(&mut v, 40, 0);
        put16(&mut v, 58, SHDR_SIZE as u16);
        put16(&mut v, 60, 1);
        assert_eq!(fs.run(&v), Ok(()));
        // Past the end of the file.
        let last = v.len() as u64 - 1;
        put64(&mut v, 40, last);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
        // Entries smaller than a section header.
        put64(&mut v, 40, 0);
        put16(&mut v, 58, SHDR_SIZE as u16 - 1);
        assert_eq!(fs.run(&v), Err(LxError::ENOEXEC));
    }

    #[test]
    fn a_hash_bang_with_no_interpreter_on_it_is_enoexec() {
        let fs = Fs::new();
        for line in [&b"#!\n"[..], b"#!   \t\n", b"#!", b"#!\xff\xfe\n"] {
            assert_eq!(fs.run(line), Err(LxError::ENOEXEC), "{:?}", line);
        }
    }

    #[test]
    fn a_script_runs_down_its_interpreter_to_an_elf() {
        let fs = Fs::new();
        fs.file("sh", &dynamic("/ld.so"));
        assert_eq!(fs.run(b"#!/sh -e\necho\n"), Ok(()));
        // Not installed.
        assert_eq!(fs.run(b"#!/no/such/sh\n"), Err(LxError::ENOENT));
        // A directory, and a file that is no program.
        assert_eq!(fs.run(b"#!/dir\n"), Err(LxError::EACCES));
        fs.file("junk", &[b'z'; 128]);
        assert_eq!(fs.run(b"#!/junk\n"), Err(LxError::ENOEXEC));
        // An interpreter whose own `PT_INTERP` is missing: the chain is
        // followed into the ELF at the end of it.
        fs.file("glibc-sh", &dynamic("/lib64/ld-linux-x86-64.so.2"));
        assert_eq!(fs.run(b"#!/glibc-sh\n"), Err(LxError::ENOENT));
    }

    /// The interpreter is found the way the loader will find it, symlinks
    /// included -- no further: a path the loader cannot open has to be
    /// refused here, or it is refused after the line.
    #[test]
    fn an_interpreter_is_looked_up_as_the_loader_looks_it_up() {
        let fs = Fs::new();
        fs.file("busybox", &image(&[]));
        fs.symlink("sh", "/busybox");
        fs.symlink("sh2", "/sh");
        assert_eq!(fs.run(b"#!/sh\n"), Ok(()));
        assert_eq!(fs.run(b"#!/sh2\n"), Ok(()));
        fs.symlink("ld1", "/ld.so");
        fs.symlink("ld2", "/ld1");
        assert_eq!(fs.run(&dynamic("/ld2")), Ok(()));
    }

    /// An interpreter's path goes through as many links as any other path,
    /// `MAXSYMLINKS`, and one more is `ELOOP`: a `#!` one and a `PT_INTERP`
    /// alike. They went through 1 and 4.
    #[test]
    fn an_interpreter_goes_through_as_many_links_as_any_path() {
        let fs = Fs::new();
        fs.file("busybox", &image(&[]));
        fs.symlink("l1", "/busybox");
        fs.symlink("ld1", "/ld.so");
        for i in 2..=crate::fs::FOLLOW_MAX_DEPTH + 1 {
            fs.symlink(&format!("l{}", i), &format!("/l{}", i - 1));
            fs.symlink(&format!("ld{}", i), &format!("/ld{}", i - 1));
        }
        let most = crate::fs::FOLLOW_MAX_DEPTH;
        assert_eq!(fs.run(format!("#!/l{}\n", most).as_bytes()), Ok(()));
        assert_eq!(
            fs.run(format!("#!/l{}\n", most + 1).as_bytes()),
            Err(LxError::ELOOP)
        );
        assert_eq!(fs.run(&dynamic(&format!("/ld{}", most))), Ok(()));
        assert_eq!(
            fs.run(&dynamic(&format!("/ld{}", most + 1))),
            Err(LxError::ELOOP)
        );
    }

    /// `open_exec` asks `MAY_EXEC` of an interpreter, so one the caller may
    /// not execute is `EACCES`, as the program itself would be: a `#!` one
    /// and a `PT_INTERP` alike, and for root when the file has no execute
    /// bit at all. It used to run.
    #[test]
    fn an_interpreter_the_caller_may_not_execute_is_eacces() {
        let fs = Fs::new();
        let not_executable = |name: &str, bytes: &[u8], mode: u32| {
            let f = fs.root.create(name, FileType::File, mode).unwrap();
            f.write_at(0, bytes).unwrap();
        };
        not_executable("sh-rw", &image(&[]), 0o644);
        not_executable("ld-rw", &image(&[]), 0o644);
        assert_eq!(fs.run(b"#!/sh-rw\n"), Err(LxError::EACCES));
        assert_eq!(fs.run(&dynamic("/ld-rw")), Err(LxError::EACCES));

        // Execute for its owner only: root may (it has a bit), a stranger
        // may not, through a link as much as by name.
        not_executable("sh-owner", &image(&[]), 0o744);
        fs.symlink("sh-link", "/sh-owner");
        fs.file("script", b"#!/sh-link\n");
        assert_eq!(fs.exec("script"), Ok(()));
        let stranger = Credentials {
            ruid: 1000,
            euid: 1000,
            suid: 1000,
            fsuid: 1000,
            rgid: 1000,
            egid: 1000,
            sgid: 1000,
            fsgid: 1000,
            groups: vec![1000],
            caps: crate::process::Capabilities::for_uids(1000, 1000, 1000),
            ..Credentials::default()
        };
        assert_eq!(fs.exec_as("script", &stranger), Err(LxError::EACCES));
        // Asked of the effective ids, as every open is: a set-user-ID root
        // program run by that stranger may.
        let set_uid_root = Credentials {
            ruid: 1000,
            rgid: 1000,
            ..Credentials::default()
        };
        assert_eq!(fs.exec_as("script", &set_uid_root), Ok(()));
    }

    /// Five scripts may stand in front of the ELF, as on Linux; the sixth is
    /// ELOOP -- unless the interpreter it names is not there, which Linux
    /// finds out first.
    #[test]
    fn a_chain_of_scripts_stops_at_the_depth_linux_stops_at() {
        let fs = Fs::new();
        fs.file("elf", &image(&[]));
        let chain = |len: usize| {
            for i in 0..len {
                let next = if i + 1 == len {
                    String::from("/elf")
                } else {
                    format!("/s{}", i + 1)
                };
                let name = format!("s{}", i);
                let _ = fs.root.unlink(&name);
                fs.file(&name, format!("#!{}\n", next).as_bytes());
            }
            fs.exec("s0")
        };
        assert_eq!(chain(LinuxElfLoader::MAX_INTERP_DEPTH), Ok(()));
        assert_eq!(
            chain(LinuxElfLoader::MAX_INTERP_DEPTH + 1),
            Err(LxError::ELOOP)
        );

        fs.root.unlink("elf").unwrap();
        assert_eq!(
            chain(LinuxElfLoader::MAX_INTERP_DEPTH + 1),
            Err(LxError::ENOENT)
        );
    }

    #[test]
    fn a_pt_interp_that_names_nothing_is_enoent() {
        let fs = Fs::new();
        assert_eq!(
            fs.run(&dynamic("/lib64/ld-linux-x86-64.so.2")),
            Err(LxError::ENOENT)
        );
        // Empty, and not a name any file here can have.
        assert_eq!(
            fs.run(&image(&[(PT_INTERP, b"\0\0")])),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            fs.run(&image(&[(PT_INTERP, b"/l\xffd\0")])),
            Err(LxError::ENOENT)
        );
    }

    /// `load_elf_binary`: two bytes at least, `PATH_MAX` at most, and the
    /// last of them the terminator.
    #[test]
    fn a_pt_interp_linux_would_not_read_as_a_path_is_enoexec() {
        let fs = Fs::new();
        assert_eq!(
            fs.run(&image(&[(PT_INTERP, b"/ld.so")])),
            Err(LxError::ENOEXEC)
        );
        assert_eq!(fs.run(&image(&[(PT_INTERP, b"\0")])), Err(LxError::ENOEXEC));
        let mut long = vec![b'/'; MAX_INTERP_PATH as usize + 1];
        *long.last_mut().unwrap() = 0;
        assert_eq!(fs.run(&image(&[(PT_INTERP, &long)])), Err(LxError::ENOEXEC));
        // The path ends at the first NUL, as `get_interpreter` reads it.
        assert_eq!(fs.run(&image(&[(PT_INTERP, b"/ld.so\0junk\0")])), Ok(()));
    }

    #[test]
    fn only_the_first_pt_interp_counts() {
        let fs = Fs::new();
        let v = image(&[(PT_INTERP, b"/ld.so\0"), (PT_INTERP, b"/nowhere\0")]);
        assert_eq!(fs.run(&v), Ok(()));
        let v = image(&[(PT_INTERP, b"/nowhere\0"), (PT_INTERP, b"/ld.so\0")]);
        assert_eq!(fs.run(&v), Err(LxError::ENOENT));
    }

    /// The errnos of the survey's `PT_INTERP` cases on a Linux host: a
    /// directory is EACCES, ten bytes are EIO (a short read of the header),
    /// 128 bytes of garbage are ELIBBAD.
    #[test]
    fn a_pt_interp_that_is_not_an_interpreter_for_this_machine_is_elibbad() {
        let fs = Fs::new();
        assert_eq!(fs.run(&dynamic("/dir")), Err(LxError::EACCES));
        fs.file("short", b"\x01\x02garbage\n");
        assert_eq!(fs.run(&dynamic("/short")), Err(LxError::EIO));
        fs.file("junk", &[b'z'; 128]);
        assert_eq!(fs.run(&dynamic("/junk")), Err(LxError::ELIBBAD));

        let mut other = image(&[]);
        put16(&mut other, 18, EM_OTHER);
        fs.file("other", &other);
        assert_eq!(fs.run(&dynamic("/other")), Err(LxError::ELIBBAD));
        // Its program headers are read too, and measured like the image's.
        let mut cut = image(&[]);
        cut.truncate(phdr(1) - 1);
        fs.file("cut", &cut);
        assert_eq!(fs.run(&dynamic("/cut")), Err(LxError::ELIBBAD));
        // A script is not an interpreter for an ELF, however long.
        let mut script = b"#!/ld.so\n".to_vec();
        script.resize(EHDR_SIZE, b'#');
        fs.file("script", &script);
        assert_eq!(fs.run(&dynamic("/script")), Err(LxError::ELIBBAD));
    }

    /// An interpreter of the other class than the program it is the
    /// interpreter of, both ways round. Each file on its own is one this
    /// kernel runs, which is why nothing before this looked at the pair.
    ///
    /// Measured on a Linux 6.18 host: a 32-bit `ET_EXEC` whose `PT_INTERP`
    /// names `/usr/bin/true` (x86_64), and `/usr/bin/true` itself with its
    /// `PT_INTERP` patched to a 32-bit static image, both come back
    /// "Accessing a corrupted shared library", which is `ELIBBAD`.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_interpreter_of_the_other_class_than_its_program_is_elibbad() {
        let fs = Fs::new();
        // `/ld.so` is the 64-bit interpreter `Fs::new` installs.
        fs.file("ld32.so", &image_i386(&[]));

        // Each flavour with its own interpreter is let through, because the
        // check is about the pair and not about either file.
        assert_eq!(fs.run(&dynamic("/ld.so")), Ok(()));
        assert_eq!(fs.run(&dynamic_i386("/ld32.so")), Ok(()));

        // And crossed over, neither is.
        assert_eq!(fs.run(&dynamic_i386("/ld.so")), Err(LxError::ELIBBAD));
        assert_eq!(fs.run(&dynamic("/ld32.so")), Err(LxError::ELIBBAD));
    }

    /// A file that reports more bytes than it hands out, which is a file
    /// being written while it is executed.
    struct Shrunk {
        inner: Arc<dyn INode>,
        size: usize,
    }

    impl INode for Shrunk {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, _: usize, _: &[u8]) -> rcore_fs::vfs::Result<usize> {
            unimplemented!()
        }
        fn poll(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::PollStatus> {
            unimplemented!()
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::Metadata> {
            let mut m = self.inner.metadata()?;
            m.size = self.size;
            Ok(m)
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    /// `do_open_execat` refuses an interpreter that is not a regular file --
    /// `if (!S_ISREG(...)) return -EACCES` -- and that is the row of the table
    /// above that nothing asked for. The test next to it only names an
    /// interpreter the caller may not execute, which is the other half of the
    /// same function. A char device, a block device, a FIFO and a socket all
    /// resolve and all carry an exec bit, so every other check lets them
    /// through, and the only thing in the way is that one `==`: relaxed to
    /// "not a directory", `#!/dev/tty` passed the preflight and the process
    /// died past the point of no return, with the `SIGSEGV` this module exists
    /// to replace by an errno.
    #[test]
    fn an_interpreter_that_is_not_a_regular_file_is_eacces() {
        let fs = Fs::new();
        for (name, kind) in [
            ("tty", FileType::CharDevice),
            ("sda", FileType::BlockDevice),
            ("fifo", FileType::NamedPipe),
            ("sock", FileType::Socket),
        ] {
            // 0o777, so the permission half of `opened_for_exec` passes and
            // what is left is the kind of file this is.
            fs.root.create2(name, kind, 0o777, 0).unwrap();
            fs.file("s", alloc::format!("#!/{}\n", name).as_bytes());

            assert_eq!(fs.exec("s"), Err(LxError::EACCES), "#!/{}", name);

            fs.root.unlink("s").unwrap();
            fs.root.unlink(name).unwrap();
        }
    }

    /// A `PT_INTERP` of exactly `PATH_MAX` bytes is a path Linux reads, and
    /// the range that says so is inclusive. The test above names only the far
    /// side of each end -- 4097 refused, 1 refused -- so closing the range to
    /// `2..MAX_INTERP_PATH` refused the longest legal interpreter path there
    /// is, and refused it with `ENOEXEC`: the errno a shell takes as "this is
    /// a shell script", so it would have run the dynamic binary as one.
    #[test]
    fn a_pt_interp_of_exactly_path_max_bytes_is_a_path() {
        let fs = Fs::new();
        // The whole segment, NUL-terminated at the path and zero to the end,
        // which is how a linker pads it.
        let mut path = vec![0u8; MAX_INTERP_PATH as usize];
        path[..b"/ld.so".len()].copy_from_slice(b"/ld.so");

        assert_eq!(fs.run(&image(&[(PT_INTERP, &path)])), Ok(()));
    }

    /// A `PT_INTERP` the file no longer holds is `EIO`, the errno `elf_read`
    /// gives a short read.
    ///
    /// `image_phdrs` already measured that segment against the file's size, so
    /// getting here means the file shrank between the two reads -- a binary
    /// rewritten while it is being executed. The buffer is zeroed, so the
    /// bytes that did arrive look like a NUL-terminated path: with the `!=`
    /// relaxed, the preflight looked up whatever prefix it got and answered
    /// `ENOENT`, which a shell prints as "not found" for an interpreter that
    /// is installed.
    #[test]
    fn a_pt_interp_the_file_no_longer_holds_is_eio() {
        let fs = Fs::new();
        let whole = dynamic("/ld.so");
        fs.file("x", &whole[..whole.len() - 4]);
        let image: Arc<dyn INode> = Arc::new(Shrunk {
            inner: fs.root.lookup("x").unwrap(),
            size: whole.len(),
        });
        let loader = LinuxElfLoader {
            syscall_entry: 0,
            stack_pages: 1,
            root_inode: fs.root.clone(),
            interp_lookup: None,
            identity: Default::default(),
        };

        assert_eq!(
            loader.preflight(&image, &Credentials::default()),
            Err(LxError::EIO)
        );
    }
}
