//! Linux ELF Program Loader
#![deny(missing_docs)]

use {
    crate::error::LxResult,
    crate::fs::INodeExt,
    crate::process::Abi,
    alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec},
    core::convert::TryFrom,
    rcore_fs::vfs::INode,
    xmas_elf::ElfFile,
    zircon_object::{util::elf_loader::*, vm::*, ZxError},
};

/// `__FreeBSD_version` advertised through `AT_OSRELDATE` (FreeBSD 14.0). Kept in
/// step with the value the syscall layer reports via `sysctl`.
#[cfg(target_arch = "x86_64")]
const FREEBSD_OSRELDATE: usize = 1_400_097;

/// Detect the ABI personality of an ELF image from its header and notes.
///
/// The primary signal is `EI_OSABI == ELFOSABI_FREEBSD` (9), which FreeBSD's
/// toolchain stamps on static executables. As a fallback — some FreeBSD
/// binaries leave `EI_OSABI` at `SYSV` and instead carry a `PT_NOTE` whose
/// vendor name is "FreeBSD" — the note segments are scanned for that name.
/// Non-x86_64 targets always report [`Abi::Linux`]: the ABI this kernel
/// implements is FreeBSD/amd64.
fn detect_abi(data: &[u8], _elf: &ElfFile) -> Abi {
    #[cfg(target_arch = "x86_64")]
    {
        const ELFOSABI_FREEBSD: u8 = 9;
        if data.get(7) == Some(&ELFOSABI_FREEBSD) {
            return Abi::Freebsd;
        }
        for ph in _elf.program_iter() {
            if ph.get_type() == Ok(xmas_elf::program::Type::Note) {
                let off = ph.offset() as usize;
                let end = off.saturating_add(ph.file_size() as usize);
                if let Some(seg) = data.get(off..end.min(data.len())) {
                    if seg.windows(7).any(|w| w == b"FreeBSD") {
                        return Abi::Freebsd;
                    }
                }
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = data;
    }
    Abi::Linux
}

/// Split a shebang line into the interpreter and its single optional argument.
///
/// POSIX splits on ASCII space/tab only, and passes everything after the first
/// separator as ONE argument, however many spaces it contains.
fn split_shebang(line: &str) -> Option<(&str, Option<&str>)> {
    let mut parts = line.splitn(2, [' ', '\t']);
    let interp = match parts.next() {
        Some(i) if !i.is_empty() => i,
        _ => return None,
    };
    let arg = parts.next().map(|s| s.trim()).filter(|s| !s.is_empty());
    Some((interp, arg))
}

/// The interpreter named by a `#!` line at the head of `data`, if it is a
/// script at all. Scans at most the first 512 bytes, as the loader does.
fn shebang_interp(data: &[u8]) -> Option<&str> {
    if !data.starts_with(b"#!") {
        return None;
    }
    let scan_limit = data.len().min(512);
    let newline = data[..scan_limit]
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(scan_limit);
    let line = core::str::from_utf8(data.get(2..newline)?)
        .ok()?
        .trim_end_matches('\r')
        .trim();
    split_shebang(line).map(|(interp, _)| interp)
}

/// Where an image's entry point lands once it is loaded at `base`.
///
/// `e_entry` is a `u64` the file chose freely, and the sum used to be an
/// unchecked `usize` add: an entry point near the top of the range wrapped
/// round to a small address, and the process was started on whatever happened
/// to be mapped there instead of being rejected. Linux rejects the same
/// binaries, `BAD_ADDR(elf_entry)` in `load_elf_binary`.
fn entry_address(base: usize, entry_point: u64) -> LxResult<usize> {
    let entry = usize::try_from(entry_point)
        .ok()
        .and_then(|entry| base.checked_add(entry))
        .ok_or(ZxError::INVALID_ARGS)?;
    if entry >= STACK_TOP {
        warn!(
            "elf: entry point {:#x} is outside the user address space",
            entry
        );
        return Err(ZxError::INVALID_ARGS.into());
    }
    Ok(entry)
}

/// Stack top: place the user stack at the very top of the user address space so
/// that the heap (at `initial_brk` just after the loaded image) never collides
/// with the stack.  Linux uses a similar high-address default for the stack.
const STACK_TOP: usize = USER_ASPACE_BASE as usize + USER_ASPACE_SIZE as usize;

/// Base of the program break, well away from the mmap arena.
///
/// The heap used to start immediately after the loaded image, which reads
/// naturally but does not survive contact with the allocator: sub-VMARs and
/// anonymous `mmap`s are placed bottom-up by first fit, so the very first
/// `mmap` the dynamic linker makes takes the range the heap was about to grow
/// into. Every later `brk` growth then fails -- glibc's first heap extension
/// on Firefox failed exactly this way (INVALID_ARGS at 0x438000) -- and malloc
/// silently falls back to mmap for everything.
///
/// Linux keeps the two apart by construction: the heap grows up from the image
/// and the mmap arena grows DOWN from just below the stack. This tree's
/// allocator is bottom-up, so the equivalent is to start the heap high above
/// where mmap will be working: a quarter of the way up leaves the mmap arena
/// the whole bottom of the address space and the heap the other three
/// quarters to grow into, so neither can reach the other in any realistic
/// program. On x86_64 and aarch64 that is the same 32 TiB this used to spell
/// out, with a 128 TiB space below the stack.
///
/// A fraction of the process's own root VMAR rather than a literal, and
/// measured at runtime rather than compiled in, because neither half of that
/// is the same everywhere. riscv64 runs Sv39, whose user half is 256 GiB: 32
/// TiB is not an address there at all, so `brk` could never place the heap and
/// every growth failed with `brk: failed to map 0x2000 bytes at
/// 0x200000000000: INVALID_ARGS` — on `/bin/busybox ls` as much as on anything
/// else, which is what `Linux Other Test Baremetal (riscv64)` was failing
/// cases on even when the program itself printed the right answer. And a libos
/// build gives each process a window carved out of the host address space
/// (`base=0x2_0000_0000, len=0x100_0000_0000`), which `USER_ASPACE_SIZE` does
/// not describe either, so the same line came back on x86_64 for the whole
/// `Linux Libc Test Libos` suite. There musl falls back to `mmap` and the
/// program still gets its memory, but the line is an `ERROR` and the harness
/// fails any case whose log contains one.
pub fn heap_base(vmar: &VmAddressRegion) -> usize {
    let len = vmar.end_addr() - vmar.addr();
    vmar.addr() + len.next_power_of_two() / 4
}

// The image sub-VMARs below are placed with `allocate(None, ..)`, i.e. at the
// root VMAR's base, and PT_LOAD segments are then mapped at their `p_vaddr`
// relative to that sub-VMAR. Non-PIE (ET_EXEC) binaries carry absolute vaddrs,
// so this only works when the root VMAR starts at address 0. Fail the build,
// not every process at runtime, if that constant ever moves again.
const _: () = assert!(
    USER_ASPACE_BASE == 0,
    "USER_ASPACE_BASE must be 0: the ELF loader maps non-PIE images at their absolute vaddrs"
);

mod abi;

/// Linux ELF Program Loader.
pub struct LinuxElfLoader {
    /// syscall entry
    pub syscall_entry: usize,
    /// stack page number
    pub stack_pages: usize,
    /// root inode of LinuxElfLoader
    pub root_inode: Arc<dyn INode>,
}

impl LinuxElfLoader {
    /// load a Linux ElfFile and return a tuple of (entry, sp, brk)
    ///
    /// `brk` is the initial program break (end of the loaded image, page-aligned).
    /// Callers should store it on the process with `proc.linux().set_brk(brk)`.
    /// load a Linux ElfFile and return a tuple of (entry, sp, brk)
    ///
    /// `brk` is the initial program break (end of the loaded image, page-aligned).
    /// Callers should store it on the process with `proc.linux().set_brk(brk)`.
    pub fn load(
        &self,
        vmar: &Arc<VmAddressRegion>,
        vmo: &Arc<VmObject>,
        args: Vec<String>,
        envs: Vec<String>,
        path: String,
    ) -> LxResult<(VirtAddr, VirtAddr, usize, String, Abi)> {
        let size = zircon_object::vm::roundup_pages(vmo.len());
        let virt_addr = zircon_object::vm::KERNEL_ASPACE.map(
            None,
            vmo.clone(),
            0,
            size,
            zircon_object::vm::MMUFlags::READ | zircon_object::vm::MMUFlags::WRITE,
        )?;
        let data = unsafe { core::slice::from_raw_parts(virt_addr as *const u8, vmo.len()) };

        let res = self.load_impl(vmar, data, args, envs, path, 0);

        zircon_object::vm::KERNEL_ASPACE.unmap(virt_addr, size)?;
        res
    }

    /// Maximum number of interpreter levels (shebang + ELF PT_INTERP combined).
    const MAX_INTERP_DEPTH: usize = 4;

    /// Walk the shebang interpreter chain, resolving every level, WITHOUT
    /// loading or mapping anything.
    ///
    /// `execve` has a point of no return: `vmar.clear()` destroys the calling
    /// process's address space, and every step after it is assumed to succeed.
    /// A shebang whose interpreter does not exist used to be discovered
    /// *after* that point, so the ENOENT was returned to a process whose code
    /// was no longer mapped, and it faulted on the instruction after the
    /// syscall:
    ///
    /// ```text
    /// shebang: lookup interp "usr/bin/env" failed: EntryNotFound
    /// execve: LinuxElfLoader::load failed: ENOENT
    /// unhandled page fault @ 0x55991a(EXECUTE | USER) [unmapped] -> SIGSEGV
    /// ```
    ///
    /// Linux resolves the whole chain while building the `bprm`, before
    /// `begin_new_exec` commits, so a missing interpreter is an ordinary
    /// ENOENT and the shell simply reports "not found". Call this before the
    /// clear and let its error propagate while the caller is still whole.
    ///
    /// Only the head of each file is read -- a shebang line is at most 512
    /// bytes -- so this costs one `read_at` per interpreter level, and nothing
    /// at all for the ELF case, which is the overwhelmingly common one.
    pub fn preflight_interpreters(&self, head: &[u8]) -> LxResult<()> {
        let mut buf = [0u8; 512];
        let mut n = head.len().min(buf.len());
        buf[..n].copy_from_slice(&head[..n]);
        // One more level than the loader accepts: going deeper is the loader's
        // error to report, with its own message, not this check's.
        for _ in 0..=Self::MAX_INTERP_DEPTH {
            let interp = match shebang_interp(&buf[..n]) {
                Some(i) => i,
                // Not a script: an ELF (or garbage the loader will reject).
                None => return Ok(()),
            };
            let inode = self
                .root_inode
                .lookup_follow(interp.trim_start_matches('/'), 1)
                .map_err(|e| {
                    warn!("execve: interpreter {:?} does not resolve: {:?}", interp, e);
                    e
                })?;
            // An interpreter may itself be a script, so keep walking.
            n = inode.read_at(0, &mut buf)?;
        }
        Ok(())
    }

    /// Internal recursive loader that tracks interpreter depth.
    fn load_impl(
        &self,
        vmar: &Arc<VmAddressRegion>,
        data: &[u8],
        args: Vec<String>,
        envs: Vec<String>,
        path: String,
        recursion: u8,
    ) -> LxResult<(VirtAddr, VirtAddr, usize, String, Abi)> {
        debug!(
            "elf: load_impl recursion={} len={:#x} path={:?}",
            recursion,
            data.len(),
            path
        );
        debug!(
            "load: vmar.addr & size: {:#x?}, data {:#x?}, args: {:?}, envs: {:?}",
            vmar.get_info(),
            data.as_ptr(),
            args,
            envs
        );

        if recursion as usize > Self::MAX_INTERP_DEPTH {
            error!("load: interpreter chain too deep (depth={})", recursion);
            return Err(ZxError::INVALID_ARGS.into());
        }

        // Handle shebang scripts (#!).
        // Limit scan to the first 512 bytes to match typical OS shebang length restrictions.
        if data.starts_with(b"\x7fELF") {
            debug!("elf: detected ELF for {:?}", path);
            if data.len() < 64 {
                error!("elf: truncated header for {:?}", path);
                return Err(ZxError::INVALID_ARGS.into());
            }
        } else if data.starts_with(b"#!") {
            debug!("elf: detected shebang for {:?}", path);
            let scan_limit = data.len().min(512);
            let newline = data[..scan_limit]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(scan_limit);
            let line = core::str::from_utf8(&data[2..newline])
                .map_err(|_| ZxError::INVALID_ARGS)?
                .trim_end_matches('\r')
                .trim();
            let (interp, interp_arg) = match split_shebang(line) {
                Some(pair) => pair,
                None => return Err(ZxError::INVALID_ARGS.into()),
            };
            debug!(
                "shebang: interp={:?}, arg={:?}, script={:?}",
                interp, interp_arg, path
            );
            // hunter P7: audit the shebang interpreter through the exec-path
            // policy so `#!/tmp/evil` is recorded (or blocked in Enforce). Use
            // the path-only check — the interpreter may itself be a script, for
            // which ELF-magic validation would be inappropriate.
            if !hunter::check_exec_path(interp) {
                return Err(ZxError::ACCESS_DENIED.into());
            }
            let interp_rel = interp.trim_start_matches('/');
            let inode = self.root_inode.lookup_follow(interp_rel, 1).map_err(|e| {
                error!("shebang: lookup interp {:?} failed: {:?}", interp_rel, e);
                e
            })?;
            let interp_vmo = inode.read_as_vmo_cached().map_err(|e| {
                error!("shebang: read interp {:?} failed: {:?}", interp_rel, e);
                e
            })?;
            let interp_size = zircon_object::vm::roundup_pages(interp_vmo.len());
            let interp_virt = zircon_object::vm::KERNEL_ASPACE.map(
                None,
                interp_vmo.clone(),
                0,
                interp_size,
                zircon_object::vm::MMUFlags::READ | zircon_object::vm::MMUFlags::WRITE,
            )?;
            let interp_data =
                unsafe { core::slice::from_raw_parts(interp_virt as *const u8, interp_vmo.len()) };

            let interp_path: String = interp.into();
            let mut new_args = vec![interp_path.clone()];
            if let Some(arg) = interp_arg {
                new_args.push(arg.into());
            }
            new_args.push(path);
            new_args.extend_from_slice(args.get(1..).unwrap_or_default());
            let res = self.load_impl(
                vmar,
                interp_data,
                new_args,
                envs,
                interp_path,
                recursion + 1,
            );

            zircon_object::vm::KERNEL_ASPACE.unmap(interp_virt, interp_size)?;
            return res;
        }

        // `xmas_elf` slices the header, the two header tables and every
        // segment out of `data` without checking any of them against its
        // length, so a malformed image panics the parser -- and `data` here is
        // whatever file the calling process named.
        let elf = parse_checked_elf(data).inspect_err(|&e| {
            error!("elf: cannot parse {:?}: {:?}", path, e);
        })?;

        debug!("elf info:  {:#x?}", elf.header.pt2);

        // Which OS ABI does this image speak? Consulted below to build the right
        // initial stack, and returned so the caller can set the process's
        // syscall personality.
        let abi = detect_abi(data, &elf);
        if abi == Abi::Freebsd {
            info!("elf: detected FreeBSD ABI for {:?}", path);
        }

        if let Ok(interp) = elf.get_interpreter() {
            info!("interp: {:?}, path: {:?}", interp, path);

            // A PIE (ET_DYN) executable bases its LOAD segments at vaddr 0.
            // Loading one at VMAR offset 0 maps its first page — and the whole
            // low range including the null page — at address 0. That breaks the
            // null-pointer-faults invariant that libc and allocators such as
            // Firefox's mozjemalloc rely on, and puts AT_PHDR at a near-null
            // address. Reserve the low range with a guard sub-VMAR so the
            // program, interpreter, mmap and stack all load above it and any
            // access below it faults, matching Linux's non-zero PIE load base.
            // A non-PIE (ET_EXEC) binary already carries high absolute vaddrs
            // (0x400000+ on x86-64), so it needs no bias and gets none.
            const PIE_LOAD_BASE: usize = 0x40_0000;
            let is_pie = elf.header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject;
            if is_pie {
                vmar.allocate_at(0, PIE_LOAD_BASE, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
                    .inspect_err(|&e| {
                        error!("elf: reserve PIE low-address guard failed: {:?}", e);
                    })?;
            }

            // Load the main program into the first free sub-VMAR. With the PIE
            // guard in place this lands at PIE_LOAD_BASE; for a non-PIE binary
            // there is no guard and app_base is 0 (segments carry their own
            // absolute vaddrs).
            let app_size = elf.load_segment_size();
            let app_vmar = vmar
                .allocate(None, app_size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
                .inspect_err(|&e| {
                    error!(
                        "elf: allocate vmar for app size {:#x} failed: {:?}",
                        app_size, e
                    );
                })?;
            let app_base = app_vmar.addr();
            let _app_vmo = app_vmar.load_from_elf(&elf).inspect_err(|&e| {
                error!("elf: load app from elf failed: {:?}", e);
            })?;
            let app_entry = entry_address(app_base, elf.header.pt2.entry_point())?;

            // Patch any in-binary syscall-entry trampoline present in the main program.
            // Write through the VMAR (which resolves the per-segment VMO): the symbol
            // usually lives in .data/.rodata, NOT in the first LOAD segment's VMO.
            if let Some(offset) = elf.get_symbol_address("rcore_syscall_entry") {
                app_vmar.write_memory(
                    app_base + offset as usize,
                    &self.syscall_entry.to_ne_bytes(),
                )?;
            }

            // Load the interpreter (ld.so) into a second sub-VMAR placed right after the
            // main program.  Because app_vmar occupies [0, app_size), the allocator places
            // interp_vmar at interp_base = app_size (> 0).
            //
            // A non-zero AT_BASE tells musl/glibc it is running as a PT_INTERP interpreter
            // rather than in standalone mode.  In interpreter mode the dynamic linker uses
            // the already-kernel-mapped binary via AT_PHDR / AT_ENTRY instead of calling
            // mmap() from user space to re-load it – which is the path that breaks in the
            // fork+execve case and causes a page fault at the raw e_entry (e.g. 0x423a7).
            // hunter P7: audit the dynamic linker (PT_INTERP) through the
            // exec-path policy so a `/tmp/ld.so` interpreter is recorded (or
            // blocked in Enforce) before it is mapped and executed.
            if !hunter::check_exec_path(interp) {
                return Err(ZxError::ACCESS_DENIED.into());
            }
            let interp_rel = interp.trim_start_matches('/');
            let inode = self.root_inode.lookup_follow(interp_rel, 4).map_err(|e| {
                error!(
                    "elf: lookup PT_INTERP {:?} failed: {:?} (check if file exists in rootfs)",
                    interp, e
                );
                e
            })?;
            let interp_vmo = inode.read_as_vmo_cached().map_err(|e| {
                error!("elf: read interp {:?} failed: {:?}", interp, e);
                e
            })?;
            let interp_size_aligned = zircon_object::vm::roundup_pages(interp_vmo.len());
            let interp_virt = zircon_object::vm::KERNEL_ASPACE.map(
                None,
                interp_vmo.clone(),
                0,
                interp_size_aligned,
                zircon_object::vm::MMUFlags::READ | zircon_object::vm::MMUFlags::WRITE,
            )?;
            let interp_data =
                unsafe { core::slice::from_raw_parts(interp_virt as *const u8, interp_vmo.len()) };

            let interp_elf = parse_checked_elf(interp_data).inspect_err(|&e| {
                error!("elf: cannot parse interp {:?}: {:?}", interp, e);
            })?;
            let interp_size = interp_elf.load_segment_size();
            let interp_vmar = vmar
                .allocate(None, interp_size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
                .inspect_err(|&e| {
                    error!(
                        "elf: allocate vmar for interp {:?} size {:#x} failed: {:?}",
                        interp, interp_size, e
                    );
                })?;
            let interp_base = interp_vmar.addr();
            let _interp_vmo = interp_vmar.load_from_elf(&interp_elf).inspect_err(|&e| {
                error!("elf: load interp {:?} from elf failed: {:?}", interp, e);
            })?;
            let interp_entry = entry_address(interp_base, interp_elf.header.pt2.entry_point())?;

            match interp_elf.relocate(interp_vmar.clone(), vmar) {
                Ok(()) => info!("interp relocate passed!"),
                Err(e) => {
                    debug!(
                        "interp relocate Err: {:?}, keeping base {:#x}",
                        e, interp_base
                    )
                }
            }

            // The interpreter needs the same patch the main program got above,
            // and in a dynamically linked program it is the only one that
            // matters: `rcore_syscall_entry` lives in musl -- which *is* the
            // interpreter here -- and not in the executable at all, so the
            // patch above finds no symbol to write. On libos the guest reaches
            // the kernel by jumping through that pointer instead of executing
            // `syscall`, and it ships initialised to 0xdead_beaf, so the very
            // first call made through it (`__init_tp` -> `__set_thread_area`,
            // before `main`) jumped to that address and took the host process
            // down with it. Every one of the 302 cases of `Linux Libc Test
            // Libos` died there, with no output at all to say so.
            //
            // After the relocation pass, not before: an interpreter whose
            // relocations cover this slot would otherwise put 0xdead_beaf back.
            if let Some(offset) = interp_elf.get_symbol_address("rcore_syscall_entry") {
                interp_vmar
                    .write_memory(
                        interp_base + offset as usize,
                        &self.syscall_entry.to_ne_bytes(),
                    )
                    .inspect_err(|&e| {
                        error!(
                            "elf: patching the interpreter's syscall entry failed: {:?}",
                            e
                        )
                    })?;
            }

            zircon_object::vm::KERNEL_ASPACE.unmap(interp_virt, interp_size_aligned)?;

            let stack_vmo = VmObject::new_paged(self.stack_pages);
            let stack_flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
            // Place the stack at the top of the process VMAR (which equals the
            // top of the user address space on bare metal, but is a smaller
            // window in aspace-separate/libos builds) so the heap, which grows
            // up from initial_brk, never collides with the stack.
            let stack_top = vmar.end_addr().min(STACK_TOP);
            let stack_bottom = stack_top - stack_vmo.len();
            // map_range=false: don't commit all 128 stack pages eagerly on every
            // exec — the argv/env/auxv tail is committed by the `stack_vmo.write`
            // below and the rest demand-zeroes on first touch like any anon
            // mmap. Eager commit cost ~0.5 MB zero-fill + 128 PTE installs per
            // spawn, and every later fork re-walked those committed pages.
            vmar.map_ext(
                Some(stack_bottom - vmar.addr()),
                stack_vmo.clone(),
                0,
                stack_vmo.len(),
                MMUFlags::RXW,
                stack_flags,
                false,
                false,
                false,
            )?;
            let mut sp = stack_top;
            // The vDSO is a Linux ABI object. A FreeBSD binary gets a
            // FreeBSD-shaped stack that never carries `AT_SYSINFO_EHDR`, so
            // mapping it there would leave an executable region in the address
            // space that nothing can ever reach.
            let vdso_base = (abi == Abi::Linux)
                .then(|| crate::vdso::map_into(vmar, stack_bottom))
                .flatten();

            let info = abi::ProcInitInfo {
                args,
                envs,
                auxv: {
                    let mut map = BTreeMap::new();
                    // AT_SYSINFO_EHDR: where the C library looks for the vDSO.
                    // Absent when this kernel has none, which is how musl is
                    // told to keep issuing the syscall.
                    if let Some(base) = vdso_base {
                        map.insert(crate::vdso::AT_SYSINFO_EHDR, base);
                    }
                    #[cfg(target_arch = "x86_64")]
                    {
                        // AT_BASE: interpreter load address; non-zero triggers interpreter
                        // mode in musl/glibc.
                        map.insert(abi::AT_BASE, interp_base);
                        // AT_PHDR: virtual address of the main program's program-header
                        // table in memory.  Use get_phdr_vaddr() which handles both PIE
                        // (vaddr relative to load base) and non-PIE (absolute vaddr)
                        // correctly, unlike the raw ph_offset() file field.
                        let phdr_vaddr =
                            elf.get_phdr_vaddr().unwrap_or(elf.header.pt2.ph_offset()) as usize;
                        map.insert(abi::AT_PHDR, app_base + phdr_vaddr);
                        // AT_ENTRY: main program's entry point.
                        map.insert(abi::AT_ENTRY, app_entry);
                    }
                    #[cfg(target_arch = "riscv64")]
                    {
                        map.insert(abi::AT_BASE, interp_base);
                        map.insert(abi::AT_ENTRY, app_entry);
                        if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                            map.insert(abi::AT_PHDR, app_base + phdr_vaddr as usize);
                        }
                    }
                    #[cfg(target_arch = "aarch64")]
                    {
                        map.insert(abi::AT_BASE, interp_base);
                        map.insert(abi::AT_ENTRY, app_entry);
                        if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                            map.insert(abi::AT_PHDR, app_base + phdr_vaddr as usize);
                        }
                    }
                    map.insert(abi::AT_PHENT, elf.header.pt2.ph_entry_size() as usize);
                    map.insert(abi::AT_PHNUM, elf.header.pt2.ph_count() as usize);
                    map.insert(abi::AT_PAGESZ, PAGE_SIZE);
                    // Identity + AT_SECURE block. musl computes `libc.secure`
                    // at startup as "AT_UID/EUID/GID/EGID not all present, or
                    // ruid != euid, or rgid != egid, or AT_SECURE != 0"; glib's
                    // g_check_setuid() treats an unreadable AT_SECURE the same
                    // way. Omitting these made EVERY process run in secure
                    // mode: musl silently dropped LD_PRELOAD/LD_LIBRARY_PATH
                    // and GLib refused to autolaunch a D-Bus session bus
                    // ("Cannot spawn a message bus when AT_SECURE is set",
                    // which killed waybar). Everything runs as root (uid 0)
                    // and nothing is setuid, so publish 0s explicitly.
                    map.insert(abi::AT_UID, 0usize);
                    map.insert(abi::AT_EUID, 0usize);
                    map.insert(abi::AT_GID, 0usize);
                    map.insert(abi::AT_EGID, 0usize);
                    map.insert(abi::AT_SECURE, 0usize);
                    map
                },
            };
            let init_stack = info.push_at(sp);
            stack_vmo.write(self.stack_pages * PAGE_SIZE - init_stack.len(), &init_stack)?;
            sp -= init_stack.len();

            // Initial brk: the dedicated heap base, not the end of the
            // interpreter -- see [`heap_base`].
            //
            // NOTE: dynamically-linked FreeBSD binaries reach here and are built
            // with the Linux-style stack above; running them additionally needs
            // the FreeBSD dynamic linker (`/libexec/ld-elf.so.1`), which this
            // tree does not ship — so in practice only *static* FreeBSD binaries
            // (handled in the no-interpreter path below) get a FreeBSD stack.
            let initial_brk = heap_base(vmar);
            return Ok((interp_entry, sp, initial_brk, path, abi));
        }

        let size = elf.load_segment_size();
        let image_vmar = vmar
            .allocate(None, size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
            .inspect_err(|&e| {
                error!("elf: allocate vmar for size {:#x} failed: {:?}", size, e);
            })?;
        let base = image_vmar.addr();
        let _vmo = image_vmar.load_from_elf(&elf).inspect_err(|&e| {
            error!("elf: load_from_elf failed: {:?}", e);
        })?;
        let entry = entry_address(base, elf.header.pt2.entry_point())?;

        debug!(
            "load: vmar.addr & size: {:#x?}, base: {:#x?}, entry: {:#x?}",
            vmar.get_info(),
            base,
            entry
        );

        // fill syscall entry
        // Write through the VMAR (which resolves the per-segment VMO): the symbol
        // usually lives in .data/.rodata, NOT in the first LOAD segment's VMO.
        if let Some(offset) = elf.get_symbol_address("rcore_syscall_entry") {
            image_vmar.write_memory(base + offset as usize, &self.syscall_entry.to_ne_bytes())?;
        }

        match elf.relocate(image_vmar, vmar) {
            Ok(()) => info!("elf relocate passed !"),
            Err(error) => {
                // Segments stay mapped under `image_vmar.addr()`; do not clobber `base` with the
                // first program header vaddr (often not PT_LOAD). Wrong AT_BASE breaks PIE/musl
                // (e.g. user PC stuck at raw e_entry like 0x423a7 -> page fault NOT_FOUND).
                // A missing `.rela.dyn` is the normal case for non-PIE static
                // binaries, so this is a debug note, not a warning (it fired on
                // every program load at the default LOG=warn).
                debug!(
                    "elf relocate Err:{:?}, keeping load base {:#x}",
                    error, base
                );
            }
        }

        let stack_vmo = VmObject::new_paged(self.stack_pages);
        let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
        // Place the stack at the top of the process VMAR (which equals the top
        // of the user address space on bare metal, but is a smaller window in
        // aspace-separate/libos builds) so the heap, which grows up from
        // initial_brk, never collides with the stack.
        let stack_top = vmar.end_addr().min(STACK_TOP);
        let stack_bottom = stack_top - stack_vmo.len();
        // map_range=false: lazy stack, same rationale as the interpreter path
        // above — the init_stack tail is committed by `stack_vmo.write` below,
        // everything else demand-zeroes on first touch.
        vmar.map_ext(
            Some(stack_bottom - vmar.addr()),
            stack_vmo.clone(),
            0,
            stack_vmo.len(),
            MMUFlags::RXW,
            flags,
            false,
            false,
            false,
        )?;
        let mut sp = stack_top;
        debug!("load stack bottom: {:#x}", stack_bottom);
        let vdso_base = (abi == Abi::Linux)
            .then(|| crate::vdso::map_into(vmar, stack_bottom))
            .flatten();

        let info = abi::ProcInitInfo {
            args,
            envs,
            auxv: {
                let mut map = BTreeMap::new();
                // AT_SYSINFO_EHDR — see the interpreter path above. Static
                // binaries need it just as much: musl resolves
                // `__vdso_clock_gettime` lazily on the first `clock_gettime`,
                // out of the aux vector it saved at startup, and how the
                // program was linked never enters into it.
                if let Some(base) = vdso_base {
                    map.insert(crate::vdso::AT_SYSINFO_EHDR, base);
                }
                #[cfg(target_arch = "x86_64")]
                {
                    // AT_BASE: interpreter load address; 0 means no interpreter (static binary).
                    map.insert(abi::AT_BASE, 0usize);
                    // AT_PHDR: virtual address of program headers in memory.
                    // Use get_phdr_vaddr() which handles both PIE and non-PIE correctly.
                    // If None, the ELF has no loadable segment covering the program headers
                    // (degenerate case warned about inside get_phdr_vaddr()); fall back to
                    // ph_offset() as a best-effort value — AT_PHDR is optional for static
                    // binaries and musl only uses it for TLS initialisation.
                    let phdr_vaddr =
                        elf.get_phdr_vaddr().unwrap_or(elf.header.pt2.ph_offset()) as usize;
                    map.insert(abi::AT_PHDR, base + phdr_vaddr);
                    map.insert(abi::AT_ENTRY, entry);
                }
                #[cfg(target_arch = "riscv64")]
                if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                    map.insert(abi::AT_PHDR, base + phdr_vaddr as usize);
                }
                #[cfg(target_arch = "aarch64")]
                {
                    // AT_BASE: 0 means no interpreter (static binary).
                    map.insert(abi::AT_BASE, 0usize);
                    map.insert(abi::AT_ENTRY, entry);
                    if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                        map.insert(abi::AT_PHDR, base + phdr_vaddr as usize);
                    }
                }
                map.insert(abi::AT_PHENT, elf.header.pt2.ph_entry_size() as usize);
                map.insert(abi::AT_PHNUM, elf.header.pt2.ph_count() as usize);
                map.insert(abi::AT_PAGESZ, PAGE_SIZE);
                // Identity + AT_SECURE block — same rationale as the sys_execve
                // path above: without it musl flips every process into secure
                // mode (LD_PRELOAD dropped, GLib refuses D-Bus autolaunch).
                map.insert(abi::AT_UID, 0usize);
                map.insert(abi::AT_EUID, 0usize);
                map.insert(abi::AT_GID, 0usize);
                map.insert(abi::AT_EGID, 0usize);
                map.insert(abi::AT_SECURE, 0usize);
                map
            },
        };
        // A FreeBSD static binary needs a FreeBSD-shaped stack (different auxv
        // types, an SSP canary, a `ps_strings` block); everything else keeps the
        // Linux layout untouched.
        let init_stack = match abi {
            #[cfg(target_arch = "x86_64")]
            Abi::Freebsd => {
                let phdr_vaddr =
                    elf.get_phdr_vaddr().unwrap_or(elf.header.pt2.ph_offset()) as usize;
                let fbsd = abi::FreebsdAuxv {
                    phdr: base + phdr_vaddr,
                    phent: elf.header.pt2.ph_entry_size() as usize,
                    phnum: elf.header.pt2.ph_count() as usize,
                    base: 0, // static binary: no interpreter
                    entry,
                    pagesz: PAGE_SIZE,
                    ehdrflags: 0,
                    osreldate: FREEBSD_OSRELDATE,
                    ncpus: kernel_hal::vdso::vdso_constants().max_num_cpus.max(1) as usize,
                    execpath: path.clone(),
                };
                info.push_at_freebsd(sp, &fbsd)
            }
            _ => info.push_at(sp),
        };
        stack_vmo.write(self.stack_pages * PAGE_SIZE - init_stack.len(), &init_stack)?;
        sp -= init_stack.len();

        debug!(
            "ProcInitInfo auxv: {:#x?}\nentry:{:#x}, sp:{:#x}",
            info.auxv, entry, sp
        );

        // Initial brk: the same dedicated heap base as the dynamic case. A
        // static binary has no interpreter mapping its own libraries, but it
        // still mmaps, and the collision is the same one.
        let initial_brk = heap_base(vmar);
        Ok((entry, sp, initial_brk, path, abi))
    }
}

#[cfg(test)]
mod shebang_tests {
    use super::*;

    /// The interpreter is everything up to the first space or tab, and the
    /// rest of the line is ONE argument however many spaces it holds -- that
    /// is what makes `#!/usr/bin/env python3` work at all.
    #[test]
    fn a_shebang_splits_into_interpreter_and_one_argument() {
        assert_eq!(split_shebang("/bin/sh"), Some(("/bin/sh", None)));
        assert_eq!(
            split_shebang("/usr/bin/env python3"),
            Some(("/usr/bin/env", Some("python3")))
        );
        // Everything after the first separator is a single argument.
        assert_eq!(
            split_shebang("/usr/bin/awk -f -v x=1"),
            Some(("/usr/bin/awk", Some("-f -v x=1")))
        );
        assert_eq!(split_shebang("/bin/sh\t-e"), Some(("/bin/sh", Some("-e"))));
        assert_eq!(split_shebang(""), None);
    }

    /// `preflight_interpreters` must say "not a script" for anything that is
    /// not a `#!` file, because the preflight runs on EVERY exec and an ELF
    /// must not pay for it -- nor be rejected by it.
    #[test]
    fn only_a_hash_bang_file_names_an_interpreter() {
        assert_eq!(shebang_interp(b"#!/bin/sh\necho hi\n"), Some("/bin/sh"));
        assert_eq!(
            shebang_interp(b"#!/usr/bin/env bash\n"),
            Some("/usr/bin/env")
        );
        // A trailing CR (a script written on Windows) is not part of the path.
        assert_eq!(shebang_interp(b"#!/bin/sh\r\n"), Some("/bin/sh"));
        // Leading blanks after the `#!` are allowed and skipped.
        assert_eq!(shebang_interp(b"#!  /bin/sh\n"), Some("/bin/sh"));
        assert_eq!(shebang_interp(b"\x7fELF\x02\x01\x01"), None);
        assert_eq!(shebang_interp(b"echo not a script\n"), None);
        assert_eq!(shebang_interp(b""), None);
        // No newline at all: the whole (bounded) file is the line.
        assert_eq!(shebang_interp(b"#!/bin/sh"), Some("/bin/sh"));
    }
}

#[cfg(test)]
mod elf_bounds_tests {
    use super::*;

    /// `EI_CLASS` = 64-bit, `EI_DATA` = little-endian.
    const CLASS64: u8 = 2;
    const DATA_LSB: u8 = 1;
    /// Size of an ELF64 header and of one ELF64 program header entry.
    const EHDR_SIZE: usize = 64;
    const PHDR_SIZE: usize = 56;
    const SHDR_SIZE: usize = 64;

    /// One program header entry, in the fields this loader reads.
    #[derive(Clone, Copy, Default)]
    struct Phdr {
        p_type: u32,
        /// `p_flags`: the R/W/X bits the mapper turns into page permissions.
        flags: u32,
        offset: u64,
        virtual_addr: u64,
        physical_addr: u64,
        file_size: u64,
        mem_size: u64,
    }

    /// One section header entry, likewise.
    #[derive(Clone, Copy, Default)]
    struct Shdr {
        sh_type: u32,
        /// Offset of this section's name in the name table.
        name: u32,
        offset: u64,
        size: u64,
        /// Set instead of `offset` to place the section in the payload area,
        /// whose position depends on how many headers there turn out to be.
        at_payload: Option<u64>,
    }

    /// An ELF64 image built field by field, so a test can put one value out of
    /// range and leave everything else well formed.
    #[derive(Default)]
    struct Elf {
        class: u8,
        data_encoding: u8,
        entry_point: u64,
        ph_offset: Option<u64>,
        ph_entry_size: u16,
        sh_offset: Option<u64>,
        sh_entry_size: u16,
        sh_str_index: u16,
        phdrs: Vec<Phdr>,
        shdrs: Vec<Shdr>,
        /// Bytes appended after both tables, so a segment has somewhere to
        /// point at.
        payload: Vec<u8>,
        /// Length to cut the finished image down to.
        truncate_to: Option<usize>,
    }

    impl Elf {
        fn new() -> Self {
            Self {
                class: CLASS64,
                data_encoding: DATA_LSB,
                ph_entry_size: PHDR_SIZE as u16,
                sh_entry_size: SHDR_SIZE as u16,
                ..Default::default()
            }
        }

        fn phdr(mut self, ph: Phdr) -> Self {
            self.phdrs.push(ph);
            self
        }

        fn shdr(mut self, sh: Shdr) -> Self {
            self.shdrs.push(sh);
            self
        }

        /// Append `bytes` to the payload and return where they start, as an
        /// offset within it.
        fn blob(&mut self, bytes: &[u8]) -> u64 {
            let at = self.payload.len() as u64;
            self.payload.extend_from_slice(bytes);
            at
        }

        fn build(&self) -> Vec<u8> {
            let ph_table = EHDR_SIZE;
            let sh_table = ph_table + self.phdrs.len() * PHDR_SIZE;
            let payload_at = sh_table + self.shdrs.len() * SHDR_SIZE;
            let mut v = vec![0u8; payload_at];
            v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
            v[4] = self.class;
            v[5] = self.data_encoding;
            v[6] = 1;
            v[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
            v[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
            v[24..32].copy_from_slice(&self.entry_point.to_le_bytes());
            let ph_offset = self.ph_offset.unwrap_or(ph_table as u64);
            let sh_offset = self.sh_offset.unwrap_or(sh_table as u64);
            v[32..40].copy_from_slice(&ph_offset.to_le_bytes());
            v[40..48].copy_from_slice(&sh_offset.to_le_bytes());
            v[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
            v[54..56].copy_from_slice(&self.ph_entry_size.to_le_bytes());
            v[56..58].copy_from_slice(&(self.phdrs.len() as u16).to_le_bytes());
            v[58..60].copy_from_slice(&self.sh_entry_size.to_le_bytes());
            v[60..62].copy_from_slice(&(self.shdrs.len() as u16).to_le_bytes());
            v[62..64].copy_from_slice(&self.sh_str_index.to_le_bytes());

            for (i, ph) in self.phdrs.iter().enumerate() {
                let at = ph_table + i * PHDR_SIZE;
                v[at..at + 4].copy_from_slice(&ph.p_type.to_le_bytes());
                v[at + 4..at + 8].copy_from_slice(&ph.flags.to_le_bytes());
                v[at + 8..at + 16].copy_from_slice(&ph.offset.to_le_bytes());
                v[at + 16..at + 24].copy_from_slice(&ph.virtual_addr.to_le_bytes());
                v[at + 24..at + 32].copy_from_slice(&ph.physical_addr.to_le_bytes());
                v[at + 32..at + 40].copy_from_slice(&ph.file_size.to_le_bytes());
                v[at + 40..at + 48].copy_from_slice(&ph.mem_size.to_le_bytes());
            }
            for (i, sh) in self.shdrs.iter().enumerate() {
                let at = sh_table + i * SHDR_SIZE;
                let offset = match sh.at_payload {
                    Some(within) => payload_at as u64 + within,
                    None => sh.offset,
                };
                v[at..at + 4].copy_from_slice(&sh.name.to_le_bytes());
                v[at + 4..at + 8].copy_from_slice(&sh.sh_type.to_le_bytes());
                v[at + 24..at + 32].copy_from_slice(&offset.to_le_bytes());
                v[at + 32..at + 40].copy_from_slice(&sh.size.to_le_bytes());
            }
            v.extend_from_slice(&self.payload);
            if let Some(len) = self.truncate_to {
                v.truncate(len);
            }
            v
        }
    }

    /// Offset of the payload area of an image with `phdrs` program headers and
    /// no section headers.
    fn payload_at(phdrs: usize) -> u64 {
        (EHDR_SIZE + phdrs * PHDR_SIZE) as u64
    }

    /// The whole point: `execve` hands the parser a file the calling process
    /// chose, and `xmas_elf` slices `data[16..64]` out of it the moment the
    /// first sixteen bytes look like an ELF. A twenty-byte file used to be
    /// enough to panic the kernel from an unprivileged process.
    #[test]
    fn a_file_too_short_to_hold_its_own_header_is_rejected() {
        let full = Elf::new().build();
        assert_eq!(full.len(), EHDR_SIZE);
        assert_eq!(check_elf_bounds(&full), Ok(()));
        for len in 0..EHDR_SIZE {
            assert_eq!(
                check_elf_bounds(&full[..len]),
                Err(ZxError::INVALID_ARGS),
                "a {}-byte image passed the bounds check",
                len
            );
        }
    }

    /// `parse_program_header` indexes the file at `e_phoff` with no bounds
    /// check of any kind, so a header table said to live past the end of the
    /// file panicked the parser on the first entry.
    #[test]
    fn a_header_table_outside_the_file_is_rejected() {
        let phdr = Phdr {
            p_type: 1, // PT_LOAD
            ..Default::default()
        };
        // Well formed: the table follows the ELF header.
        assert_eq!(check_elf_bounds(&Elf::new().phdr(phdr).build()), Ok(()));

        // Past the end of the file.
        let mut elf = Elf::new().phdr(phdr);
        elf.ph_offset = Some(0x1000);
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        // Starting inside the file but running off the end of it.
        let mut elf = Elf::new().phdr(phdr);
        elf.ph_offset = Some(EHDR_SIZE as u64 + 8);
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        // Far enough out that `e_phoff + e_phnum * e_phentsize` wraps.
        let mut elf = Elf::new().phdr(phdr);
        elf.ph_offset = Some(u64::MAX - 8);
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        // The same for the section header table, which `get_symbol_address`
        // walks on every exec.
        let mut elf = Elf::new().shdr(Shdr::default());
        elf.sh_offset = Some(0x1000);
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));
    }

    /// An entry smaller than the struct the parser reads out of it makes
    /// `zero::read` assert, which is the same kernel panic by another route.
    #[test]
    fn a_header_entry_smaller_than_its_struct_is_rejected() {
        let mut elf = Elf::new().phdr(Phdr::default());
        elf.ph_entry_size = PHDR_SIZE as u16 - 1;
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        let mut elf = Elf::new().shdr(Shdr::default());
        elf.sh_entry_size = SHDR_SIZE as u16 - 1;
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        // A count of zero means there is no table at all, so an entry size of
        // zero is not a problem and a real linker does emit one.
        let mut elf = Elf::new();
        elf.ph_entry_size = 0;
        elf.sh_entry_size = 0;
        assert_eq!(check_elf_bounds(&elf.build()), Ok(()));
    }

    /// `ProgramHeader::raw_data` slices `p_offset .. p_offset + p_filesz`, so
    /// any truncated binary -- a copy interrupted half way -- panicked the
    /// kernel rather than failing the exec.
    #[test]
    fn a_segment_whose_contents_run_past_the_end_of_the_file_is_rejected() {
        let load = Phdr {
            p_type: 1,
            offset: payload_at(1),
            file_size: 16,
            mem_size: 16,
            ..Default::default()
        };
        let mut elf = Elf::new().phdr(load);
        elf.payload = vec![0u8; 16];
        assert_eq!(check_elf_bounds(&elf.build()), Ok(()));

        // The same image with the last byte missing.
        let image = elf.build();
        assert_eq!(
            check_elf_bounds(&image[..image.len() - 1]),
            Err(ZxError::INVALID_ARGS)
        );

        // A file size that wraps when added to the offset.
        let mut elf = Elf::new().phdr(Phdr {
            file_size: u64::MAX,
            ..load
        });
        elf.payload = vec![0u8; 16];
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));
    }

    /// Section contents are sliced the same way by `get_symbol_address` and
    /// `dynsym`, with one exception: `.bss` declares a size but stores no
    /// bytes, so its `sh_offset` says nothing about the file.
    #[test]
    fn a_section_outside_the_file_is_rejected_unless_it_holds_no_bytes() {
        const SHT_PROGBITS: u32 = 1;
        const SHT_NOBITS: u32 = 8;
        let mut elf = Elf::new().shdr(Shdr {
            sh_type: SHT_PROGBITS,
            offset: payload_at(0) + SHDR_SIZE as u64,
            size: 8,
            ..Default::default()
        });
        elf.payload = vec![0u8; 8];
        assert_eq!(check_elf_bounds(&elf.build()), Ok(()));

        let image = elf.build();
        assert_eq!(
            check_elf_bounds(&image[..image.len() - 1]),
            Err(ZxError::INVALID_ARGS)
        );

        // `.bss`: a size far larger than the file, and legitimate.
        let elf = Elf::new().shdr(Shdr {
            sh_type: SHT_NOBITS,
            offset: payload_at(0) + SHDR_SIZE as u64,
            size: 0x10000,
            ..Default::default()
        });
        assert_eq!(check_elf_bounds(&elf.build()), Ok(()));

        // Its offset still has to be inside the file: `get_shstr_table` slices
        // `input[sh_offset..]` before anything has looked at the type.
        let elf = Elf::new().shdr(Shdr {
            sh_type: SHT_NOBITS,
            offset: 0x1000,
            size: 0,
            ..Default::default()
        });
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));
    }

    /// `e_shstrndx` names the section holding every other section's name, and
    /// `ElfFile::section_header` reads it without consulting the table's
    /// length: it slices `e_shoff + index * e_shentsize` out of the file
    /// whatever `e_shnum` says, and `assert!`s outright at `SHN_LORESERVE`,
    /// where section indices stop and the format's escape values begin.
    /// `get_symbol_address` reaches it on every exec, looking for the syscall
    /// trampoline, and `dynsym` on every dynamically linked program.
    ///
    /// Linux never reads `e_shstrndx` at all -- `binfmt_elf` works from the
    /// program headers -- so an index naming no section is not a reason to
    /// refuse the exec. The lookup finds nothing, which is the answer a
    /// stripped binary gets too.
    #[test]
    fn a_name_table_index_the_table_does_not_contain_names_no_section() {
        // Three sections: the two asked for and the name table they are named
        // from, which `with_sections` puts last and points `e_shstrndx` at.
        let good = with_sections(&[
            (".symtab", SHT_SYMTAB, &symbol(1, 0x1234)),
            (".strtab", SHT_STRTAB, b"\0entry\0"),
        ]);
        let found = |image: &[u8]| {
            assert_eq!(check_elf_bounds(image), Ok(()));
            parse_checked_elf(image)
                .unwrap()
                .get_symbol_address("entry")
        };
        assert_eq!(found(&good), Some(0x1234));
        // The last index the table holds is the name table itself, and a bound
        // one too tight would lose it -- along with every well-formed binary's.
        assert_eq!(found(&with_name_table_index(good.clone(), 2)), Some(0x1234));

        // One past the end of the table, and the reserved index above which
        // the parser asserts rather than answering.
        for index in [3u16, 4, 0xff00, u16::MAX] {
            assert_eq!(
                found(&with_name_table_index(good.clone(), index)),
                None,
                "e_shstrndx = {:#x} named a section the table does not hold",
                index
            );
        }

        // The off-by-one on its own, with nothing at all after the table, so
        // an index one past the end is read from outside the file or not at
        // all -- there is no third answer to mistake for the right one.
        let mut elf = Elf::new().shdr(Shdr::default()).shdr(Shdr::default());
        elf.sh_str_index = 1; // the last entry there is
        assert_eq!(found(&elf.build()), None);
        elf.sh_str_index = 2; // one past it
        assert_eq!(found(&elf.build()), None);
    }

    /// The file that panicked is the one with no section table at all.
    /// `check_elf_bounds` measures a table by walking its entries, so with
    /// `e_shnum == 0` there is nothing to measure `e_shoff` and `e_shentsize`
    /// against -- and `e_shstrndx` is read outside that walk, so it reached
    /// `section_header` with all three numbers unchecked. Sixty-four bytes was
    /// enough to panic the kernel from `execve`, four different ways.
    #[test]
    fn a_file_with_no_section_table_reads_no_section_header() {
        let mut shapes: Vec<(&str, Elf)> = Vec::new();

        let mut elf = Elf::new();
        elf.sh_str_index = 7;
        shapes.push(("e_shstrndx past the end of a table that is not there", elf));

        let mut elf = Elf::new();
        elf.sh_str_index = 0xff00; // SHN_LORESERVE: the parser asserts on it
        shapes.push(("e_shstrndx is a reserved index", elf));

        // The last two leave `e_shstrndx` at 0, which is what a stripped
        // binary carries: the bound that matters is on `e_shnum`, and an
        // index of zero reaches `section_header` exactly like any other.
        let mut elf = Elf::new();
        elf.sh_entry_size = u16::MAX;
        shapes.push(("e_shentsize far larger than the file", elf));

        let mut elf = Elf::new();
        elf.sh_offset = Some(0x1_0000);
        shapes.push(("e_shoff past the end of the file", elf));

        for (what, elf) in shapes {
            let image = elf.build();
            assert_eq!(image.len(), EHDR_SIZE, "{}", what);
            assert_eq!(check_elf_bounds(&image), Ok(()), "{}", what);
            let parsed = parse_checked_elf(&image).unwrap();
            // The three section reads an exec makes.
            assert_eq!(
                parsed.get_symbol_address("rcore_syscall_entry"),
                None,
                "{}",
                what
            );
            assert!(parsed.dynsym().is_err(), "{}", what);
            let (root, image) = image_vmar();
            assert_eq!(
                parsed.relocate(image, &root),
                Err(".rela.dyn not found"),
                "{}",
                what
            );
        }
    }

    /// The point of answering instead of rejecting: a binary with its sections
    /// stripped is an ordinary thing to run, and it has to load. `e_shnum` of
    /// zero is what `strip` leaves behind, and the loader's three section reads
    /// are all optional -- the syscall trampoline is a patch this tree's own
    /// binaries carry, and relocations belong to dynamically linked ones.
    #[test]
    fn a_stripped_binary_still_loads() {
        let mut elf = Elf::new().phdr(Phdr {
            p_type: 1,    // PT_LOAD
            flags: 0b101, // R+X
            offset: payload_at(1),
            // Above the host's `vm.mmap_min_addr`, like the flags test below:
            // this one really does `mmap` at the address the header names, and
            // a runner with the usual 64 KiB answers EPERM where a container
            // with 4 KiB maps it happily.
            virtual_addr: 0x40_0000,
            file_size: 4,
            mem_size: 0x1000,
            ..Default::default()
        });
        elf.payload = vec![0x90u8; 4];
        // No section table at all, and an `e_shstrndx` left pointing at a
        // section that was stripped away with the rest.
        elf.sh_str_index = 3;
        let image = elf.build();
        let parsed = parse_checked_elf(&image).unwrap();
        let vmar = VmAddressRegion::new_root();
        assert!(vmar.load_from_elf(&parsed).is_ok());
    }

    /// A 32-bit image reaches `section_header` through the same door and is
    /// sliced with 32-bit entries, so the bound has to hold for it too -- and
    /// nothing in the loader's own 64-bit-only section handling gets that far.
    #[test]
    fn a_32_bit_image_with_no_section_table_reads_no_section_header() {
        // An ELF32 header and nothing else: `e_shnum` is zero, so `e_shoff`,
        // `e_shentsize` and `e_shstrndx` are all unmeasured.
        let mut v = vec![0u8; 52];
        v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        v[4] = 1; // ELFCLASS32
        v[5] = DATA_LSB;
        v[6] = 1;
        v[32..36].copy_from_slice(&0x1_0000u32.to_le_bytes()); // e_shoff
        v[46..48].copy_from_slice(&u16::MAX.to_le_bytes()); // e_shentsize
        v[50..52].copy_from_slice(&9u16.to_le_bytes()); // e_shstrndx
        assert_eq!(check_elf_bounds(&v), Ok(()));
        let parsed = parse_checked_elf(&v).unwrap();
        assert_eq!(parsed.get_symbol_address("rcore_syscall_entry"), None);
        assert!(parsed.dynsym().is_err());
    }

    /// `e_shnum` is a `u16`, but section indices stop at `SHN_LORESERVE`:
    /// everything from there up is an escape value, and the parser `assert!`s
    /// rather than answering for one. A count above that describes a table
    /// that cannot be walked to the end -- and `get_symbol_address` walks the
    /// whole table on every exec, so it used to panic part way through.
    #[test]
    fn a_section_table_longer_than_the_index_space_stops_where_indices_do() {
        const RESERVED: usize = 0xff00;
        // Entries of nothing: the only question this asks is whether the walk
        // reaches the end of the table it was given.
        let empty_table = |count: usize| {
            let mut v = vec![0u8; EHDR_SIZE + count * SHDR_SIZE];
            v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
            v[4] = CLASS64;
            v[5] = DATA_LSB;
            v[6] = 1;
            v[40..48].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes()); // e_shoff
            v[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes()); // e_shentsize
            v[60..62].copy_from_slice(&(count as u16).to_le_bytes()); // e_shnum
            v
        };
        // The largest table the index space holds, and one entry more. Both
        // fit in the file, so the bounds check has nothing to say about them.
        // The third puts `e_shstrndx` in the reserved range as well, which a
        // table this long is the only way to reach: the point read is bounded
        // by the same rule as the walk, not merely by `e_shnum`.
        for (count, name_table) in [
            (RESERVED, 0u16),
            (RESERVED + 1, 0),
            (RESERVED + 1, RESERVED as u16),
        ] {
            let mut image = empty_table(count);
            image[62..64].copy_from_slice(&name_table.to_le_bytes());
            let what = alloc::format!("{} sections, e_shstrndx {:#x}", count, name_table);
            assert_eq!(check_elf_bounds(&image), Ok(()), "{}", what);
            assert_eq!(
                parse_checked_elf(&image).unwrap().get_symbol_address("x"),
                None,
                "{}",
                what
            );
        }
    }

    /// And the walk has to reach the last entry of the table. Section order is
    /// the linker's to choose, so the `.symtab` that `get_symbol_address`
    /// looks for on every exec can be the one sitting there -- a bound one
    /// entry short would find nothing and quietly leave the syscall
    /// trampoline unpatched.
    #[test]
    fn the_last_entry_of_the_section_table_is_walked() {
        let image = symbol_table_last(0);
        assert_eq!(check_elf_bounds(&image), Ok(()));
        assert_eq!(
            parse_checked_elf(&image)
                .unwrap()
                .get_symbol_address("entry"),
            Some(0x77)
        );
    }

    /// An image whose three sections are the name table, a `.strtab` and a
    /// `.symtab`, in that order, with the symbol table `skew` bytes off its
    /// natural eight-byte alignment.
    fn symbol_table_last(skew: usize) -> Vec<u8> {
        let mut elf = Elf::new();
        // `\0.shstrtab\0.strtab\0.symtab\0`: names at 1, 11 and 19.
        let names = b"\0.shstrtab\0.strtab\0.symtab\0";
        let names_at = elf.blob(names);
        let strtab = b"\0entry\0";
        let strtab_at = elf.blob(strtab);
        // A real linker gives `.symtab` an `sh_addralign` of 8 and places it
        // accordingly; the payload of an image with no program headers starts
        // on an eight-byte boundary, so pad to one here the same way.
        let payload_start = EHDR_SIZE + 3 * SHDR_SIZE;
        assert_eq!(payload_start % 8, 0);
        let pad = (8 - elf.payload.len() % 8) % 8 + skew;
        elf.blob(&vec![0u8; pad]);
        let symtab = symbol(1, 0x77);
        let symtab_at = elf.blob(&symtab);
        assert_eq!((payload_start + symtab_at as usize) % 8, skew % 8);
        for (name, sh_type, at, size) in [
            (1u32, SHT_STRTAB, names_at, names.len()),
            (11, SHT_STRTAB, strtab_at, strtab.len()),
            (19, SHT_SYMTAB, symtab_at, symtab.len()),
        ] {
            elf.shdrs.push(Shdr {
                name,
                sh_type,
                at_payload: Some(at),
                size: size as u64,
                offset: 0,
            });
        }
        elf.sh_str_index = 0; // the name table first, the symbol table last
        elf.build()
    }

    /// `zero::read_array` turns a section's bytes into a `&[Entry64]` with
    /// `slice::from_raw_parts`, which needs the section to be aligned for the
    /// type as well as a whole number of entries. Only the second was
    /// checked, and `sh_offset` is a number the file chooses: a `.symtab` at
    /// an odd offset built a misaligned slice, which is undefined behaviour.
    ///
    /// It is worse than the panic it sits beside. The debug build's
    /// precondition check for it does NOT unwind -- `thread caused
    /// non-unwinding panic. aborting.` -- so there is nothing for a kernel to
    /// report or recover from, and `get_symbol_address` runs on every exec.
    #[test]
    fn a_symbol_table_at_an_odd_offset_is_not_read() {
        // The same image, one byte over and back on the boundary.
        for skew in 1..8 {
            let image = symbol_table_last(skew);
            assert_eq!(check_elf_bounds(&image), Ok(()), "skew {}", skew);
            assert_eq!(
                parse_checked_elf(&image)
                    .unwrap()
                    .get_symbol_address("entry"),
                None,
                "a .symtab {} bytes off its alignment was read anyway",
                skew
            );
        }
        assert_eq!(
            parse_checked_elf(&symbol_table_last(8))
                .unwrap()
                .get_symbol_address("entry"),
            Some(0x77)
        );
    }

    /// The identification bytes decide how everything after them is read. The
    /// parser maps its structs straight onto the file, so it reads every field
    /// in the host's byte order and a big-endian image would be read as
    /// garbage, lengths included.
    #[test]
    fn only_a_little_endian_image_of_a_known_class_is_accepted() {
        let mut elf = Elf::new();
        elf.data_encoding = 2; // ELFDATA2MSB
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        let mut elf = Elf::new();
        elf.class = 0; // ELFCLASSNONE
        assert_eq!(check_elf_bounds(&elf.build()), Err(ZxError::INVALID_ARGS));

        // Not an ELF at all: the same error the parser itself would give, so a
        // shell script still fails the way it always did.
        assert_eq!(check_elf_bounds(b"#!/bin/sh\n"), Err(ZxError::INVALID_ARGS));
        assert_eq!(check_elf_bounds(b""), Err(ZxError::INVALID_ARGS));
    }

    /// A 32-bit image has a smaller header and smaller entries, and the check
    /// has to know both or it rejects every one of them.
    #[test]
    fn a_32_bit_image_is_measured_with_32_bit_offsets() {
        // An ELF32 header is 52 bytes, and its program headers are 32.
        let mut v = vec![0u8; 52 + 32];
        v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        v[4] = 1; // ELFCLASS32
        v[5] = DATA_LSB;
        v[28..32].copy_from_slice(&52u32.to_le_bytes()); // e_phoff
        v[42..44].copy_from_slice(&32u16.to_le_bytes()); // e_phentsize
        v[44..46].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        assert_eq!(check_elf_bounds(&v), Ok(()));

        // The same image one byte short of its program header.
        assert_eq!(
            check_elf_bounds(&v[..v.len() - 1]),
            Err(ZxError::INVALID_ARGS)
        );

        // With no tables at all, the header's own 52 bytes are the only thing
        // that has to be there -- and the parser slices all 52 of them.
        let mut bare = v[..52].to_vec();
        bare[44..46].copy_from_slice(&0u16.to_le_bytes()); // e_phnum
        assert_eq!(check_elf_bounds(&bare), Ok(()));
        for len in 0..52 {
            assert_eq!(
                check_elf_bounds(&bare[..len]),
                Err(ZxError::INVALID_ARGS),
                "a {}-byte 32-bit image passed the bounds check",
                len
            );
        }

        // A segment past the end of the file, read through 32-bit fields:
        // `p_offset` sits four bytes into the entry and `p_filesz` sixteen.
        v[56..60].copy_from_slice(&64u32.to_le_bytes()); // p_offset
        v[68..72].copy_from_slice(&64u32.to_le_bytes()); // p_filesz
        assert_eq!(check_elf_bounds(&v), Err(ZxError::INVALID_ARGS));
    }

    /// `p_type` is a `u32` the file chooses, and `xmas_elf` knows only a
    /// handful of values. Unwrapping the `Err` panicked the kernel on a
    /// one-byte edit; Linux's loader switches on `p_type` and ignores what it
    /// does not recognise.
    #[test]
    fn a_program_header_of_an_unknown_type_is_skipped_not_unwrapped() {
        let elf = Elf::new().phdr(Phdr {
            p_type: 8, // between PT_TLS and PT_GNU_RELRO: no name at all
            virtual_addr: 0x1000,
            mem_size: 0x1000,
            ..Default::default()
        });
        let image = elf.build();
        assert_eq!(check_elf_bounds(&image), Ok(()));
        let parsed = ElfFile::new(&image).unwrap();
        assert!(parsed.program_iter().next().unwrap().get_type().is_err());
        // No LOAD segment, so nothing to size -- and no panic getting there.
        assert_eq!(parsed.load_segment_size(), 0);
    }

    /// `p_vaddr + p_memsz` was an unchecked `u64` add fed into a `pages()`
    /// that rounds up with a `wrapping_add`, so a segment at the top of the
    /// address space came back as a handful of pages and the image was mapped
    /// into a region far too small for it.
    #[test]
    fn a_segment_at_the_top_of_the_address_space_does_not_wrap_to_nothing() {
        let build = |virtual_addr, mem_size| {
            let image = Elf::new()
                .phdr(Phdr {
                    p_type: 1,
                    virtual_addr,
                    mem_size,
                    ..Default::default()
                })
                .build();
            assert_eq!(check_elf_bounds(&image), Ok(()));
            ElfFile::new(&image).unwrap().load_segment_size()
        };

        // An ordinary segment still measures the way it always did.
        assert_eq!(build(0x1000, 0x2000), 0x3000);
        assert_eq!(build(0x1100, 0x1), 0x2000);

        // Measured on its own as well, for the same reason.
        assert_eq!(segment_end_pages(0x1000, 0x2000), 3);
        assert_eq!(segment_end_pages(0x1100, 1), 2);
        assert_eq!(segment_end_pages(0, 0), 0);
        assert_eq!(segment_end_pages(u64::MAX, 1), usize::MAX / PAGE_SIZE);
        assert_eq!(segment_end_pages(1, u64::MAX), usize::MAX / PAGE_SIZE);

        // These used to wrap round to a size of zero, or to one page.
        assert_eq!(build(u64::MAX, 1), usize::MAX / PAGE_SIZE * PAGE_SIZE);
        assert_eq!(
            build(u64::MAX - 0xfff, 0x1000),
            usize::MAX / PAGE_SIZE * PAGE_SIZE
        );
        assert_eq!(build(1, u64::MAX), usize::MAX / PAGE_SIZE * PAGE_SIZE);
    }

    /// The interpreter path is read by scanning a segment for a nul byte. The
    /// scan had no end, so a `PT_INTERP` whose contents are not terminated ran
    /// off the segment and panicked the kernel.
    #[test]
    fn an_interpreter_path_without_a_terminator_is_an_error_not_a_panic() {
        let interp = |bytes: &[u8]| {
            let mut elf = Elf::new().phdr(Phdr {
                p_type: 3, // PT_INTERP
                offset: payload_at(1),
                file_size: bytes.len() as u64,
                mem_size: bytes.len() as u64,
                ..Default::default()
            });
            elf.payload = bytes.to_vec();
            let image = elf.build();
            assert_eq!(check_elf_bounds(&image), Ok(()));
            ElfFile::new(&image)
                .unwrap()
                .get_interpreter()
                .map(String::from)
                .map_err(String::from)
        };

        assert_eq!(
            interp(b"/lib/ld-musl-x86_64.so.1\0").as_deref(),
            Ok("/lib/ld-musl-x86_64.so.1")
        );
        // Every byte of the segment is path: there is no terminator to find.
        assert!(interp(b"/lib/ld.so").is_err());
        assert!(interp(b"").is_err());
    }

    /// `e_entry` is a `u64` the file chooses, and it used to be added to the
    /// load base with no check: an entry point near the top of the range
    /// wrapped round to a small address and the process was started on
    /// whatever happened to be mapped there.
    #[test]
    fn an_entry_point_that_does_not_fit_is_rejected() {
        assert_eq!(entry_address(0x40_0000, 0x1000), Ok(0x40_1000));
        assert_eq!(entry_address(0, 0), Ok(0));

        assert!(entry_address(0x40_0000, u64::MAX).is_err());
        assert!(entry_address(usize::MAX, 1).is_err());
        // In range as a number, outside the user address space.
        assert!(entry_address(0, STACK_TOP as u64).is_err());
        assert_eq!(entry_address(0, STACK_TOP as u64 - 1), Ok(STACK_TOP - 1));
    }

    /// `sh_type` values this tree's loader meets.
    const SHT_PROGBITS_: u32 = 1;
    const SHT_SYMTAB: u32 = 2;
    const SHT_STRTAB: u32 = 3;
    const SHT_RELA: u32 = 4;
    const SHT_DYNSYM: u32 = 11;
    const SHT_GROUP: u32 = 17;

    /// An image with a section table: a name table, and the sections the
    /// caller asks for as `(name, sh_type, contents)`, named from it.
    fn with_sections(sections: &[(&str, u32, &[u8])]) -> Vec<u8> {
        with_named_sections(sections, None)
    }

    /// The same, but the name table's own contents can be supplied whole --
    /// which is how a table with no terminator gets built.
    fn with_named_sections(sections: &[(&str, u32, &[u8])], names: Option<&[u8]>) -> Vec<u8> {
        let mut table = vec![0u8];
        let mut offsets = Vec::new();
        for (name, _, _) in sections {
            offsets.push(table.len() as u32);
            table.extend_from_slice(name.as_bytes());
            table.push(0);
        }
        let table_name = table.len() as u32;
        table.extend_from_slice(b".shstrtab\0");
        let table = names.map(<[u8]>::to_vec).unwrap_or(table);

        let mut elf = Elf::new();
        let mut placed = Vec::new();
        for (i, (_, sh_type, data)) in sections.iter().enumerate() {
            let at = elf.blob(data);
            placed.push(Shdr {
                name: offsets[i],
                sh_type: *sh_type,
                at_payload: Some(at),
                size: data.len() as u64,
                offset: 0,
            });
        }
        let table_at = elf.blob(&table);
        elf.shdrs.extend(placed);
        elf.shdrs.push(Shdr {
            name: table_name,
            sh_type: SHT_STRTAB,
            at_payload: Some(table_at),
            size: table.len() as u64,
            offset: 0,
        });
        elf.sh_str_index = sections.len() as u16;
        elf.build()
    }

    /// The same image with `e_shstrndx` pointed somewhere else.
    fn with_name_table_index(mut image: Vec<u8>, index: u16) -> Vec<u8> {
        image[62..64].copy_from_slice(&index.to_le_bytes());
        image
    }

    /// One `Elf64_Sym`: name offset at 0, value at 8.
    fn symbol(name: u32, value: u64) -> [u8; 24] {
        let mut sym = [0u8; 24];
        sym[..4].copy_from_slice(&name.to_le_bytes());
        // `st_shndx` of 1: a defined symbol, so `relocate` does not skip it.
        sym[6..8].copy_from_slice(&1u16.to_le_bytes());
        sym[8..16].copy_from_slice(&value.to_le_bytes());
        sym
    }

    /// Every name in an ELF is an offset into a table of nul-terminated
    /// strings, and both the offset and the bytes come from the file.
    /// `zero::read_str`, which every name lookup in the parser goes through,
    /// panics on a table that does not terminate and again on bytes that are
    /// not UTF-8.
    #[test]
    fn a_name_read_from_a_string_table_is_bounded_at_both_ends() {
        let table = b"\0first\0second\0";
        assert_eq!(str_at(table, 1), Some("first"));
        assert_eq!(str_at(table, 7), Some("second"));
        assert_eq!(str_at(table, 0), Some(""));
        // A name may start part way through another, which is how a linker
        // shares the tail of a string.
        assert_eq!(str_at(table, 10), Some("ond"));

        // Past the end of the table.
        assert_eq!(str_at(table, table.len() as u32 + 1), None);
        assert_eq!(str_at(table, u32::MAX), None);
        // No terminator: the scan used to run off the end of the table.
        assert_eq!(str_at(b"no terminator", 0), None);
        // Not UTF-8.
        assert_eq!(str_at(b"\xff\xfe\0", 0), None);
        assert_eq!(str_at(b"", 0), None);
    }

    /// `get_data` hands back a symbol table as a slice with
    /// `zero::read_array`, which ASSERTS that the section divides exactly into
    /// entries. A `.symtab` one byte long panicked the kernel, and
    /// `get_symbol_address` runs on every exec looking for the syscall
    /// trampoline.
    #[test]
    fn a_symbol_table_that_does_not_divide_into_symbols_is_not_read() {
        let names = b"\0entry\0";
        let good = with_sections(&[
            (".symtab", SHT_SYMTAB, &symbol(1, 0x1234)),
            (".strtab", SHT_STRTAB, names),
        ]);
        let elf = parse_checked_elf(&good).unwrap();
        assert_eq!(elf.get_symbol_address("entry"), Some(0x1234));
        assert_eq!(elf.get_symbol_address("missing"), None);

        // The same table with one byte too many.
        let mut ragged = symbol(1, 0x1234).to_vec();
        ragged.push(0);
        let bad = with_sections(&[
            (".symtab", SHT_SYMTAB, &ragged),
            (".strtab", SHT_STRTAB, names),
        ]);
        let elf = parse_checked_elf(&bad).unwrap();
        assert_eq!(elf.get_symbol_address("entry"), None);
    }

    /// The same for `.dynsym`, which `relocate` reads on every dynamically
    /// linked program.
    #[test]
    fn a_ragged_dynsym_is_an_error_not_a_panic() {
        let good = with_sections(&[(".dynsym", SHT_DYNSYM, &symbol(1, 0x20))]);
        assert_eq!(
            parse_checked_elf(&good).unwrap().dynsym().map(<[_]>::len),
            Ok(1)
        );

        let bad = with_sections(&[(".dynsym", SHT_DYNSYM, &symbol(1, 0x20)[..23])]);
        assert!(parse_checked_elf(&bad).unwrap().dynsym().is_err());

        // And an image with no `.dynsym` at all, which is most of them.
        let none = with_sections(&[(".text", 1, b"\x90")]);
        assert!(parse_checked_elf(&none).unwrap().dynsym().is_err());
    }

    /// `get_symbol_address` asked every section for its contents whatever its
    /// type, so the parser's whole dispatch ran on file-chosen bytes: an empty
    /// group section indexed `data[0]`, and a 32-bit note reached an
    /// `unimplemented!()`. Only symbol tables are of any interest here, and
    /// the search has to carry on past the rest.
    #[test]
    fn a_section_type_the_parser_cannot_handle_is_skipped() {
        let image = with_sections(&[
            (".group", SHT_GROUP, b""),
            (".symtab", SHT_SYMTAB, &symbol(1, 0x5678)),
            (".strtab", SHT_STRTAB, b"\0entry\0"),
        ]);
        let elf = parse_checked_elf(&image).unwrap();
        assert_eq!(elf.get_symbol_address("entry"), Some(0x5678));
    }

    /// A section may declare a size and store nothing (`.bss` is the usual
    /// one), and that size is the one the bounds check cannot bound: it
    /// describes memory, not the file. So a string table declared that way
    /// holds no names, whatever bytes happen to sit where it points.
    #[test]
    fn a_string_table_that_stores_no_bytes_holds_no_names() {
        const SHT_NOBITS: u32 = 8;
        let names = b"\0entry\0";
        // The very same image, with `.strtab` the one way and the other.
        let real = with_sections(&[
            (".symtab", SHT_SYMTAB, &symbol(1, 0x4321)),
            (".strtab", SHT_STRTAB, names),
        ]);
        assert_eq!(
            parse_checked_elf(&real)
                .unwrap()
                .get_symbol_address("entry"),
            Some(0x4321)
        );

        let nobits = with_sections(&[
            (".symtab", SHT_SYMTAB, &symbol(1, 0x4321)),
            (".strtab", SHT_NOBITS, names),
        ]);
        assert_eq!(
            parse_checked_elf(&nobits)
                .unwrap()
                .get_symbol_address("entry"),
            None
        );
    }

    /// The section names themselves live in a table the file chose, so a
    /// lookup by name is a string read like any other. With nothing readable
    /// there, no section is found -- which is the answer for a stripped
    /// binary too, and not a panic.
    #[test]
    fn a_name_table_that_does_not_terminate_finds_no_sections() {
        let image = with_named_sections(
            &[(".dynsym", SHT_DYNSYM, &symbol(1, 0x20))],
            Some(b"\0.dynsym"),
        );
        let elf = parse_checked_elf(&image).unwrap();
        assert!(elf.dynsym().is_err());
        assert_eq!(elf.get_symbol_address("entry"), None);
    }

    /// A relocation names its symbol by index into `.dynsym`, and the index is
    /// a `u32` from the file: indexing the slice with it panicked the kernel
    /// on any entry pointing past the end of the table.
    #[test]
    fn a_relocation_naming_a_symbol_outside_dynsym_is_skipped() {
        // One `Elf64_Rela`: r_offset, then r_info as (symbol index << 32) |
        // type, then r_addend. Type 6 is R_X86_64_GLOB_DAT, which resolves a
        // symbol and so reaches the table.
        let mut rela = [0u8; 24];
        rela[..8].copy_from_slice(&0x1000u64.to_le_bytes());
        rela[8..16].copy_from_slice(&(((0xffff_u64) << 32) | 6).to_le_bytes());
        let image = with_sections(&[
            (".rela.dyn", SHT_RELA, &rela),
            (".dynsym", SHT_DYNSYM, &symbol(1, 0x20)),
            (".dynstr", SHT_STRTAB, b"\0entry\0"),
        ]);
        let elf = parse_checked_elf(&image).unwrap();
        let vmar = VmAddressRegion::new_root();
        assert_eq!(elf.relocate(vmar.clone(), &vmar), Ok(()));
    }

    /// Every check here rejects something, and a check that rejects too much
    /// is worse than the panic it replaced: nothing on the system would run.
    /// So: a real ELF, produced by a real linker, with a full section table,
    /// a `.bss`, a section name table and program headers of half a dozen
    /// types. The test binary itself is one.
    #[test]
    fn a_real_binary_still_passes_every_check() {
        extern crate std;
        let image = match std::fs::read("/proc/self/exe") {
            Ok(image) => image,
            // Not Linux, or /proc is not mounted: nothing to say either way.
            Err(_) => return,
        };
        assert_eq!(check_elf_bounds(&image), Ok(()));
        let elf = parse_checked_elf(&image).expect("the test binary is an ELF");
        assert!(elf.load_segment_size() > 0);
        // And the section-name path, which is what `dynsym` walks.
        assert!(elf.find_section_by_name(".text").is_some());
    }

    /// `parse_checked_elf` is the only way an image from userspace becomes an
    /// `ElfFile`, so the bounds check cannot be left out of a call site by
    /// forgetting it. Every file that used to panic the parser comes back as
    /// an error from here.
    #[test]
    fn the_checked_parser_rejects_what_the_parser_would_have_panicked_on() {
        let full = Elf::new().build();
        // Twenty bytes: `parse_header` would have sliced `data[16..64]`.
        assert_eq!(
            parse_checked_elf(&full[..20]).err(),
            Some(ZxError::INVALID_ARGS)
        );
        // A program header table outside the file.
        let mut elf = Elf::new().phdr(Phdr {
            p_type: 1,
            ..Default::default()
        });
        elf.ph_offset = Some(0x1000);
        assert_eq!(
            parse_checked_elf(&elf.build()).err(),
            Some(ZxError::INVALID_ARGS)
        );
        // A well-formed image still parses.
        assert!(parse_checked_elf(&full).is_ok());
    }

    /// An ELF with no LOAD segment at all maps nothing, so there is no first
    /// VMO to hand back. Unwrapping the `None` panicked the kernel; an image
    /// with only a `PT_NOTE` is enough to reach it.
    #[test]
    fn an_image_with_nothing_to_load_is_rejected_by_the_mapper() {
        let image = Elf::new()
            .phdr(Phdr {
                p_type: 4, // PT_NOTE
                ..Default::default()
            })
            .build();
        let parsed = ElfFile::new(&image).unwrap();
        let vmar = VmAddressRegion::new_root();
        assert_eq!(
            vmar.load_from_elf(&parsed).err(),
            Some(ZxError::INVALID_ARGS)
        );
    }

    /// `make_vmo` sized its VMO with `pages(mem_size + page_offset)`, and
    /// `pages()` rounds up with a `wrapping_add`: a segment whose `p_memsz`
    /// sits at the top of the range asked for a handful of pages, and the
    /// segment's own contents were then written past the end of them.
    #[test]
    fn a_segment_too_large_to_measure_is_rejected_by_the_mapper() {
        let load = |virtual_addr, mem_size| {
            let image = Elf::new()
                .phdr(Phdr {
                    p_type: 1, // PT_LOAD
                    virtual_addr,
                    mem_size,
                    ..Default::default()
                })
                .build();
            let parsed = ElfFile::new(&image).unwrap();
            VmAddressRegion::new_root().load_from_elf(&parsed).err()
        };

        // Measured on its own, because through `load_from_elf` every way of
        // getting this wrong comes back as the same error from a later step.
        assert_eq!(segment_pages(0x1000, 0), Ok(1));
        assert_eq!(segment_pages(0x1001, 0), Ok(2));
        assert_eq!(segment_pages(0, 0), Ok(0));
        assert_eq!(segment_pages(1, PAGE_SIZE - 1), Ok(1));
        assert_eq!(segment_pages(2, PAGE_SIZE - 1), Ok(2));
        // A segment larger than the whole user address space could not be
        // mapped whatever else is in the way, and sizing a VMO for it is what
        // used to wrap round to a handful of pages.
        assert_eq!(segment_pages(u64::MAX, 0), Err(ZxError::INVALID_ARGS));
        assert_eq!(
            segment_pages(USER_ASPACE_SIZE, 0),
            Ok(USER_ASPACE_SIZE as usize / PAGE_SIZE)
        );
        assert_eq!(
            segment_pages(USER_ASPACE_SIZE, 1),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            segment_pages(USER_ASPACE_SIZE + 1, 0),
            Err(ZxError::INVALID_ARGS)
        );
        // And the sum itself does not fit.
        assert_eq!(
            segment_pages(u64::MAX - 0x100, 0x200),
            Err(ZxError::INVALID_ARGS)
        );

        assert_eq!(load(0x1000, u64::MAX), Some(ZxError::INVALID_ARGS));
        // The size alone fits; it is the part of the first page below
        // `p_vaddr` that tips it over, and that sum used to wrap to 0xff --
        // one page for a segment claiming the whole address space.
        assert_eq!(load(0x1200, u64::MAX - 0x100), Some(ZxError::INVALID_ARGS));
        // An ordinary segment still loads.
        assert_eq!(load(0x1000, 0x2000), None);
    }

    /// One `Elf64_Rela`: `r_offset`, then `r_info` as `(symbol index << 32) |
    /// type`, then the signed `r_addend`.
    fn rela(offset: u64, sym: u32, ty: u32, addend: i64) -> [u8; 24] {
        let mut e = [0u8; 24];
        e[..8].copy_from_slice(&offset.to_le_bytes());
        e[8..16].copy_from_slice(&(((sym as u64) << 32) | ty as u64).to_le_bytes());
        e[16..24].copy_from_slice(&addend.to_le_bytes());
        e
    }

    /// R_X86_64_RELATIVE and R_X86_64_GLOB_DAT: one relocation that writes
    /// `B + A` and one that writes `S + A`.
    const R_RELATIVE: u32 = 8;
    const R_GLOB_DAT: u32 = 6;

    /// An image mapped somewhere other than zero, which is where the ELF
    /// interpreter of every dynamically linked program ends up: it is loaded
    /// after the main image, so its base is not zero, and that is what it
    /// takes for the relocation arithmetic to go past the end.
    fn image_vmar() -> (Arc<VmAddressRegion>, Arc<VmAddressRegion>) {
        let root = VmAddressRegion::new_root();
        let image = root
            .allocate_at(0x10_0000, 0x4000, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
            .unwrap();
        image
            .map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, MMUFlags::RXW)
            .unwrap();
        (root, image)
    }

    /// The word a relocation left at `image.addr() + offset`.
    fn word_at(image: &Arc<VmAddressRegion>, offset: usize) -> usize {
        let mut buf = [0u8; core::mem::size_of::<usize>()];
        image.read_memory(image.addr() + offset, &mut buf).unwrap();
        usize::from_ne_bytes(buf)
    }

    /// `base + r_offset` says WHERE a relocation writes, and both halves come
    /// from outside: `r_offset` is a `u64` the file chose and `base` is
    /// wherever the image was mapped. It was an unchecked add, reached once
    /// per entry by every dynamically linked program, and a `u64` near the top
    /// of the range panicked the kernel from `execve` on. In release it
    /// wrapped instead, and a wrapped address can still land inside another of
    /// the process's own mappings, which is a word written somewhere nobody
    /// asked for.
    #[test]
    fn a_relocation_target_that_does_not_fit_is_an_error_not_a_panic() {
        let (root, image) = image_vmar();
        for entry in [
            rela(u64::MAX, 0, R_RELATIVE, 0),
            rela(u64::MAX, 0, R_GLOB_DAT, 0),
        ] {
            let bytes = with_sections(&[
                (".rela.dyn", SHT_RELA, &entry),
                (".dynsym", SHT_DYNSYM, &symbol(1, 0x20)),
                (".dynstr", SHT_STRTAB, b"\0entry\0"),
            ]);
            let elf = parse_checked_elf(&bytes).unwrap();
            assert_eq!(
                elf.relocate(image.clone(), &root),
                Err("relocation target outside the address space")
            );
        }
        // And one that does fit still lands.
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &rela(0x10, 0, R_RELATIVE, 0x20))]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), image.addr() + 0x20);
    }

    /// The VALUE a relocation writes is ABI arithmetic, not an address, and it
    /// is allowed to wrap: `r_addend` is SIGNED, a negative one is ordinary in
    /// a real object, and the dynamic linkers this loader stands in for
    /// compute `B + A` in plain C. Bounding it like the target would reject
    /// objects that work everywhere else -- so the two are checked
    /// differently, on purpose.
    #[test]
    fn a_negative_addend_is_ordinary_arithmetic_not_an_error() {
        let (root, image) = image_vmar();
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &rela(0x10, 0, R_RELATIVE, -8))]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), image.addr().wrapping_sub(8));

        // Same for the symbol-resolving form: `S + A` with the symbol at 0x20.
        let bytes = with_sections(&[
            (".rela.dyn", SHT_RELA, &rela(0x18, 0, R_GLOB_DAT, -8)),
            (".dynsym", SHT_DYNSYM, &symbol(1, 0x20)),
            (".dynstr", SHT_STRTAB, b"\0entry\0"),
        ]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x18), image.addr() + 0x20 - 8);
    }

    /// `st_value` is a `u64` from the file too, and it is added to the base
    /// the same way. It is a value, so it wraps rather than being rejected --
    /// but it used to panic the kernel before it could do either.
    #[test]
    fn a_symbol_value_that_does_not_fit_wraps_instead_of_panicking() {
        let (root, image) = image_vmar();
        let bytes = with_sections(&[
            (".rela.dyn", SHT_RELA, &rela(0x10, 0, R_GLOB_DAT, 0)),
            (".dynsym", SHT_DYNSYM, &symbol(1, u64::MAX)),
            (".dynstr", SHT_STRTAB, b"\0entry\0"),
        ]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), image.addr().wrapping_sub(1));
    }

    /// `get_phdr_vaddr` answers the `AT_PHDR` the dynamic linker reads its own
    /// program headers from. When the image carries no `PT_PHDR` it is
    /// inferred as `p_vaddr + e_phoff` of the segment at file offset 0 -- two
    /// more numbers from the file, added without a check. A `PT_LOAD` at
    /// offset 0 with a `p_vaddr` near the top panicked the kernel; not having
    /// an address to give is a case this already handles.
    #[test]
    fn an_inferred_phdr_address_that_does_not_fit_is_no_phdr() {
        let inferred = |virtual_addr| {
            let image = Elf::new()
                .phdr(Phdr {
                    p_type: 1, // PT_LOAD
                    offset: 0,
                    virtual_addr,
                    ..Default::default()
                })
                .build();
            ElfFile::new(&image).unwrap().get_phdr_vaddr()
        };
        // `e_phoff` is the header size, since the table follows the header.
        assert_eq!(inferred(0x40_0000), Some(0x40_0000 + EHDR_SIZE as u64));
        assert_eq!(inferred(u64::MAX), None);
        assert_eq!(inferred(u64::MAX - EHDR_SIZE as u64 + 1), None);
        assert_eq!(
            inferred(u64::MAX - EHDR_SIZE as u64),
            Some(u64::MAX - EHDR_SIZE as u64 + EHDR_SIZE as u64)
        );

        // A declared `PT_PHDR` is used as it stands, with no arithmetic at all.
        let image = Elf::new()
            .phdr(Phdr {
                p_type: 6, // PT_PHDR
                virtual_addr: 0x1234,
                ..Default::default()
            })
            .phdr(Phdr {
                p_type: 1,
                offset: 0,
                virtual_addr: 0x40_0000,
                ..Default::default()
            })
            .build();
        assert_eq!(ElfFile::new(&image).unwrap().get_phdr_vaddr(), Some(0x1234));

        // And an image with neither has no phdr to name.
        let image = Elf::new()
            .phdr(Phdr {
                p_type: 1,
                offset: 0x40,
                virtual_addr: 0x40_0000,
                ..Default::default()
            })
            .build();
        assert_eq!(ElfFile::new(&image).unwrap().get_phdr_vaddr(), None);
    }

    /// A symbol the file declares but does not define (`st_shndx == 0`): the
    /// dynamic linker resolves it in user space, so the kernel leaves it.
    fn undefined_symbol(name: u32) -> [u8; 24] {
        let mut sym = symbol(name, 0x20);
        sym[6..8].copy_from_slice(&0u16.to_le_bytes());
        sym
    }

    /// `.rela.plt` holds the JUMP_SLOT entries behind the procedure linkage
    /// table. Walking only `.rela.dyn` leaves every call going through an
    /// unrelocated stub, which showed up as a jump to a low address and an
    /// Invalid Opcode fault.
    #[test]
    fn the_plt_relocations_are_applied_as_well_as_the_dynamic_ones() {
        let (root, image) = image_vmar();
        let bytes = with_sections(&[
            (".rela.dyn", SHT_RELA, &rela(0x10, 0, R_RELATIVE, 0x11)),
            (".rela.plt", SHT_RELA, &rela(0x20, 0, R_RELATIVE, 0x22)),
        ]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), image.addr() + 0x11);
        assert_eq!(word_at(&image, 0x20), image.addr() + 0x22);
    }

    /// Everything this loop can be handed and does not act on. None of it is
    /// fatal: a relocation type it does not implement (a TLS one, say) used to
    /// be `unimplemented!()`, which panicked the whole kernel over one user
    /// program, and a symbol relocation in an object with no `.dynsym` has
    /// nothing to resolve against.
    #[test]
    fn what_the_relocation_loop_cannot_act_on_is_skipped_not_fatal() {
        let (root, image) = image_vmar();

        // An unsupported type, next to one that works.
        let mut entries = rela(0x10, 0, 99, 0x11).to_vec();
        entries.extend_from_slice(&rela(0x20, 0, R_RELATIVE, 0x22));
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &entries)]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), 0);
        assert_eq!(word_at(&image, 0x20), image.addr() + 0x22);

        // A symbol relocation with no `.dynsym` to resolve against. The
        // base-relative entry beside it still lands: an object that carries
        // only those is the ordinary case for a PIE with no imports.
        let (root, image) = image_vmar();
        let mut entries = rela(0x10, 0, R_GLOB_DAT, 0x11).to_vec();
        entries.extend_from_slice(&rela(0x20, 0, R_RELATIVE, 0x22));
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &entries)]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), 0);
        assert_eq!(word_at(&image, 0x20), image.addr() + 0x22);

        // A symbol the object declares but does not define is user space's to
        // resolve, not this loader's.
        let (root, image) = image_vmar();
        let bytes = with_sections(&[
            (".rela.dyn", SHT_RELA, &rela(0x10, 0, R_GLOB_DAT, 0)),
            (".dynsym", SHT_DYNSYM, &undefined_symbol(1)),
            (".dynstr", SHT_STRTAB, b"\0memcpy\0"),
        ]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x10), 0);
    }

    /// What the caller is told when there was nothing to do, and when there
    /// was something it could not read. The difference matters: a missing
    /// `.rela.dyn` is the normal case for a non-PIE static binary, and the
    /// caller logs it and carries on.
    #[test]
    fn an_object_with_no_relocations_is_told_apart_from_a_corrupted_one() {
        let (root, image) = image_vmar();

        // Nothing to relocate at all.
        let bytes = with_sections(&[(".text", SHT_PROGBITS_, b"\x90")]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(
            elf.relocate(image.clone(), &root),
            Err(".rela.dyn not found")
        );

        // A `.rela.dyn` that does not divide into whole entries: the parser
        // asserts on that, so it never gets to.
        let ragged = &rela(0x10, 0, R_RELATIVE, 0x11)[..23];
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, ragged)]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(
            elf.relocate(image.clone(), &root),
            Err("corrupted relocation section")
        );

        // An empty one is not corrupt, it is empty.
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &[])]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
    }

    /// Relocation targets cluster in one or two mappings, so the loop keeps
    /// the last mapping it wrote through instead of re-scanning the VMAR for
    /// every entry. The cache has to be asked whether it still contains the
    /// address, not assumed: a stale hit would write a word into a mapping
    /// that the relocation had nothing to do with.
    #[test]
    fn the_mapping_cache_never_writes_through_the_wrong_mapping() {
        let root = VmAddressRegion::new_root();
        let image = root
            .allocate_at(0x10_0000, 0x4000, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
            .unwrap();
        // Two separate mappings, a page apart, with a hole between them.
        image
            .map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, MMUFlags::RXW)
            .unwrap();
        image
            .map_at(0x2000, VmObject::new_paged(1), 0, PAGE_SIZE, MMUFlags::RXW)
            .unwrap();

        // Alternate between them so a cache that never re-checks gets it wrong
        // on the second entry, and finish in the hole, which belongs to
        // neither.
        let mut entries = rela(0x0010, 0, R_RELATIVE, 0xa1).to_vec();
        entries.extend_from_slice(&rela(0x2010, 0, R_RELATIVE, 0xb2));
        entries.extend_from_slice(&rela(0x0020, 0, R_RELATIVE, 0xc3));
        entries.extend_from_slice(&rela(0x2020, 0, R_RELATIVE, 0xd4));
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &entries)]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        assert_eq!(word_at(&image, 0x0010), image.addr() + 0xa1);
        assert_eq!(word_at(&image, 0x2010), image.addr() + 0xb2);
        assert_eq!(word_at(&image, 0x0020), image.addr() + 0xc3);
        assert_eq!(word_at(&image, 0x2020), image.addr() + 0xd4);

        // An address in the hole is in no mapping at all, cache or not.
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &rela(0x1010, 0, R_RELATIVE, 0))]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Err("Invalid Vmar"));
    }

    /// `p_flags` is what the file asks for and `to_mmu_flags` is the whole
    /// translation: a segment mapped writable that asked to be read-only is a
    /// W^X hole, and one mapped without `USER` is a segment the program cannot
    /// reach at all.
    #[test]
    fn a_segments_page_permissions_are_the_ones_its_header_asked_for() {
        // PF_X = 1, PF_W = 2, PF_R = 4.
        //
        // The segment has to sit above the host's `vm.mmap_min_addr`: under
        // libos this really does `mmap` at the address the header names, and
        // a runner with the usual 64 KiB answers EPERM where a container with
        // 4 KiB maps it happily. 4 MiB is where a non-PIE image starts anyway.
        const BASE: usize = 0x40_0000;
        let flags_of = |p_flags: u32| {
            let image = Elf::new()
                .phdr(Phdr {
                    p_type: 1, // PT_LOAD
                    flags: p_flags,
                    virtual_addr: BASE as u64,
                    mem_size: 0x1000,
                    ..Default::default()
                })
                .build();
            let elf = ElfFile::new(&image).unwrap();
            let vmar = VmAddressRegion::new_root();
            vmar.load_from_elf(&elf).unwrap();
            vmar.find_mapping(BASE).unwrap().get_flags(BASE).unwrap()
        };
        let user = MMUFlags::USER;
        assert_eq!(flags_of(4), user | MMUFlags::READ);
        assert_eq!(flags_of(6), user | MMUFlags::READ | MMUFlags::WRITE);
        assert_eq!(flags_of(5), user | MMUFlags::READ | MMUFlags::EXECUTE);
        assert_eq!(
            flags_of(7),
            user | MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE
        );
        // Read-only means read-only: no write bit arrives from anywhere else.
        assert!(!flags_of(4).contains(MMUFlags::WRITE));
        assert!(!flags_of(4).contains(MMUFlags::EXECUTE));
        // Every segment belongs to the program, whatever else it asked for.
        assert!(flags_of(0).contains(MMUFlags::USER));
        // A segment that asked for nothing gets nothing but that.
        assert_eq!(flags_of(0), user);
    }

    /// A relocation whose word straddles the end of its mapping. The fast path
    /// refuses it (it writes all or nothing), so the loop falls back to the
    /// VMAR, which clamps and writes the part that fits -- half a relocated
    /// word, reported as success. Pinned rather than changed: it is the
    /// behaviour every caller has had, and a straddling relocation is a
    /// malformed object either way.
    #[test]
    fn a_relocation_straddling_the_end_of_its_mapping_is_written_in_part() {
        let root = VmAddressRegion::new_root();
        let image = root
            .allocate_at(0x10_0000, 0x4000, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
            .unwrap();
        image
            .map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, MMUFlags::RXW)
            .unwrap();
        // Four bytes from the end of the only page, so a `usize` runs over.
        let at = PAGE_SIZE - 4;
        let bytes = with_sections(&[(".rela.dyn", SHT_RELA, &rela(at as u64, 0, R_RELATIVE, 0))]);
        let elf = parse_checked_elf(&bytes).unwrap();
        assert_eq!(elf.relocate(image.clone(), &root), Ok(()));
        // The low half of the value landed; the high half went nowhere.
        let mut buf = [0u8; 4];
        image.read_memory(image.addr() + at, &mut buf).unwrap();
        assert_eq!(buf, image.addr().to_ne_bytes()[..4]);
    }
}
