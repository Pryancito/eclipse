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
//! | a sixth `#!` script in one chain          | `ELOOP`   |
//! | `PT_INTERP` not NUL-terminated, or empty  | `ENOEXEC` |
//! | `PT_INTERP` naming nothing                | `ENOENT`  |
//! | `PT_INTERP` shorter than an ELF header    | `EIO`     |
//! | `PT_INTERP` not an ELF for this machine   | `ELIBBAD` |
//!
//! It also measures every segment and the section table against the file,
//! which Linux does not do: [`check_elf_bounds`] does, after the line, so a
//! truncated binary was one more way to be killed. Here it is `ENOEXEC`.

use super::*;
use crate::error::LxError;
use rcore_fs::vfs::FileType;

/// `e_machine` of the images this kernel runs: `elf_check_arch`.
#[cfg(target_arch = "x86_64")]
const EM_HOST: u16 = 62; // EM_X86_64
#[cfg(target_arch = "aarch64")]
const EM_HOST: u16 = 183; // EM_AARCH64
#[cfg(target_arch = "riscv64")]
const EM_HOST: u16 = 243; // EM_RISCV

/// `e_type` of an image `execve` will start: a fixed-address executable or a
/// position-independent one.
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_INTERP: u32 = 3;
/// `EI_CLASS` = 64-bit and `EI_DATA` = little-endian, the only kind of image
/// this loader reads (see [`check_elf_bounds`]).
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Size of an ELF64 header, of one program header and of one section header.
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const SHDR_SIZE: u64 = 64;

/// The largest program header table `load_elf_phdrs` reads, in bytes:
/// `ELF_MIN_ALIGN`, one page. Seventy-three entries.
const MAX_PHDR_TABLE: usize = 4096;

/// The longest `PT_INTERP` path `load_elf_binary` takes, terminator included:
/// `PATH_MAX`.
const MAX_INTERP_PATH: u64 = 4096;

/// The fields of an ELF64 header the preflight reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    kind: u16,
    machine: u16,
    ph_offset: u64,
    sh_offset: u64,
    ph_entry_size: u16,
    ph_count: u16,
    sh_entry_size: u16,
    sh_count: u16,
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
/// can read at all: the magic, a whole header, 64-bit, little-endian.
fn elf_header(head: &[u8]) -> Option<Header> {
    if head.len() < EHDR_SIZE || head[..4] != *b"\x7fELF" {
        return None;
    }
    if head[4] != ELFCLASS64 || head[5] != ELFDATA2LSB {
        return None;
    }
    Some(Header {
        kind: u16_at(head, 16),
        machine: u16_at(head, 18),
        ph_offset: u64_at(head, 32),
        sh_offset: u64_at(head, 40),
        ph_entry_size: u16_at(head, 54),
        ph_count: u16_at(head, 56),
        sh_entry_size: u16_at(head, 58),
        sh_count: u16_at(head, 60),
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

/// `do_open_execat`: `if (!S_ISREG(...)) return -EACCES`. A directory or a
/// device named as an interpreter is refused like the image itself would be.
fn regular(file: &Arc<dyn INode>) -> LxResult<()> {
    if file.metadata()?.type_ == FileType::File {
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
fn image_phdrs(file: &Arc<dyn INode>, head: &[u8], bad: LxError) -> LxResult<Vec<u8>> {
    let header = elf_header(head).ok_or(bad)?;
    if !matches!(header.kind, ET_EXEC | ET_DYN) || header.machine != EM_HOST {
        return Err(bad);
    }
    // `load_elf_phdrs`.
    let table = usize::from(header.ph_count) * PHDR_SIZE;
    if usize::from(header.ph_entry_size) != PHDR_SIZE || table == 0 || table > MAX_PHDR_TABLE {
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
        && (u64::from(header.sh_entry_size) < SHDR_SIZE
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
    for ph in phdrs.as_chunks::<PHDR_SIZE>().0 {
        if !fits(u64_at(ph, 8), u64_at(ph, 32), size) {
            return Err(bad);
        }
    }
    Ok(phdrs)
}

impl LinuxElfLoader {
    /// Refuse, with the errno Linux gives, every `execve` of `image` the
    /// loader would fail on for a reason in the files themselves -- without
    /// loading or mapping anything. See the [module documentation](self).
    ///
    /// `image` is the file the caller named, already checked for search and
    /// execute permission and found to be a regular file. The `#!` chain is
    /// walked from it, every interpreter is looked up and checked as the
    /// loader will look it up, and the ELF at the end of it is checked
    /// together with its `PT_INTERP`.
    pub fn preflight(&self, image: &Arc<dyn INode>) -> LxResult<()> {
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
                let phdrs = image_phdrs(&file, head, LxError::ENOEXEC)?;
                return self.preflight_pt_interp(&file, &phdrs);
            }
            // `load_script`: "No interpreter name found".
            let interp = shebang_interp(head).ok_or(LxError::ENOEXEC)?;
            let next = self
                .root_inode
                .lookup_follow(interp.trim_start_matches('/'), SHEBANG_FOLLOW)
                .inspect_err(|e| {
                    warn!("execve: interpreter {:?} does not resolve: {:?}", interp, e);
                })?;
            regular(&next)?;
            file = next;
        }
        unreachable!("the chain ends at a depth `MAX_INTERP_DEPTH` bounds")
    }

    /// The dynamic linker `image` names, checked as `load_elf_binary` checks
    /// it before `begin_new_exec`: looked up, a regular file, and the header
    /// and program headers of an ELF for this machine.
    fn preflight_pt_interp(&self, image: &Arc<dyn INode>, phdrs: &[u8]) -> LxResult<()> {
        // The first `PT_INTERP`, as `get_interpreter` finds it.
        let ph = match phdrs
            .as_chunks::<PHDR_SIZE>()
            .0
            .iter()
            .find(|ph| u32_at(&ph[..], 0) == PT_INTERP)
        {
            Some(ph) => ph,
            None => return Ok(()),
        };
        let (offset, len) = (u64_at(ph, 8), u64_at(ph, 32));
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
        let file = self
            .root_inode
            .lookup_follow(interp.trim_start_matches('/'), PT_INTERP_FOLLOW)
            .inspect_err(|e| {
                warn!("execve: PT_INTERP {:?} does not resolve: {:?}", interp, e);
            })?;
        regular(&file)?;
        let mut head = [0u8; EHDR_SIZE];
        if read_full(&file, 0, &mut head)? != EHDR_SIZE {
            return Err(LxError::EIO);
        }
        image_phdrs(&file, &head, LxError::ELIBBAD)?;
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

        /// `execve` of `/name`, as far as the preflight goes.
        fn exec(&self, name: &str) -> LxResult<()> {
            let image = self.root.lookup(name).unwrap();
            LinuxElfLoader {
                syscall_entry: 0,
                stack_pages: 1,
                root_inode: self.root.clone(),
                identity: Default::default(),
            }
            .preflight(&image)
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
            identity: Default::default(),
        };
        assert_eq!(loader.preflight(&image), Ok(()));
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
        // Two links is one more than `SHEBANG_FOLLOW`.
        assert_eq!(SHEBANG_FOLLOW, 1);
        assert_eq!(fs.run(b"#!/sh2\n"), Err(LxError::ELOOP));
        // `PT_INTERP` goes through up to four, as `libc.musl` -> `ld-musl`
        // and friends need.
        fs.symlink("ld1", "/ld.so");
        fs.symlink("ld2", "/ld1");
        assert_eq!(fs.run(&dynamic("/ld2")), Ok(()));
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
}
