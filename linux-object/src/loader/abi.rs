//! Process init info

use alloc::collections::btree_map::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{align_of, size_of_val};
use core::ops::Deref;

/// process init information
pub struct ProcInitInfo {
    /// args strings
    pub args: Vec<String>,
    /// environment strings
    pub envs: Vec<String>,
    /// auxiliary
    pub auxv: BTreeMap<u8, usize>,
}

impl ProcInitInfo {
    /// push process init information into stack
    pub fn push_at(&self, stack_top: usize) -> Stack {
        // We will build the stack from top to bottom.
        // The strings and random bytes go first (highest addresses).
        //
        // Size the buffer to the exact image up front (upper bound on every
        // push below). A fixed 0x4000 overran and asserted -> kernel panic on a
        // big argv/envp; total argv+envp is separately capped to E2BIG in
        // sys_execve, so this bound stays modest.
        // `argv[0]` is what `AT_EXECFN` points at. `sys_execve` refuses an
        // empty argv before it ever gets here, but this is a `pub fn` and
        // indexing `args[0]` on an empty vector is a kernel panic, not an
        // error -- so name the empty case rather than rely on the caller.
        let execfn: &str = self.args.first().map(|s| s.as_str()).unwrap_or("");
        let table_entries = 1 + self.args.len() + 1 + self.envs.len() + 1 + self.auxv.len() * 2 + 6;
        let strings: usize = execfn.len()
            + 1
            + self.args.iter().map(|s| s.len() + 1).sum::<usize>()
            + self.envs.iter().map(|s| s.len() + 1).sum::<usize>();
        let needed = 16 + strings + table_entries * 8 + 32;
        let mut writer = Stack::new(stack_top, needed.max(0x4000));

        // 1. Random bytes for AT_RANDOM (16 bytes)
        let random_bytes = [0u8; 16]; // TODO: use real random
        writer.push_slice(&random_bytes);
        let random_ptr = writer.sp;

        // 2. Program name for AT_EXECFN
        writer.push_str(execfn);
        let execfn_ptr = writer.sp;

        // 3. Environment strings
        let env_ptrs: Vec<_> = self
            .envs
            .iter()
            .map(|arg| {
                writer.push_str(arg.as_str());
                writer.sp
            })
            .collect();

        // 4. Argv strings
        let arg_ptrs: Vec<_> = self
            .args
            .iter()
            .map(|arg| {
                writer.push_str(arg.as_str());
                writer.sp
            })
            .collect();

        // Now we prepare the pointer arrays (auxv, envp, argv, argc).
        // These must be contiguous and the final sp (at argc) must be 16-byte aligned.
        let mut table = Vec::new();

        // Argc
        table.push(self.args.len());
        // Argv
        for ptr in arg_ptrs {
            table.push(ptr);
        }
        table.push(0); // NULL
                       // Envp
        for ptr in env_ptrs {
            table.push(ptr);
        }
        table.push(0); // NULL

        // Auxv
        for (&type_, &value) in self.auxv.iter() {
            table.push(type_ as usize);
            table.push(value);
        }
        table.push(AT_RANDOM as usize);
        table.push(random_ptr);
        table.push(AT_EXECFN as usize);
        table.push(execfn_ptr);
        table.push(0); // AT_NULL type
        table.push(0); // AT_NULL value

        // To ensure the final sp is 16-byte aligned:
        // Current sp is where strings ended.
        // We will push `table.len()` usize elements.
        // final_sp = sp - table.len() * 8.
        // We want final_sp % 16 == 0.
        let mut sp = writer.sp;
        sp &= !0x7; // Ensure 8-byte alignment first
        let table_size = table.len() * 8;
        if !(sp - table_size).is_multiple_of(16) {
            sp -= 8; // Add 8 bytes of padding
        }
        writer.sp = sp;

        // Push the table. Since push_usize_slice decrements sp first and then
        // copies the slice, table[0] (argc) will end up at the lowest address (the new sp).
        writer.push_usize_slice(&table);

        writer
    }
}

/// program stack
pub struct Stack {
    /// stack pointer
    sp: usize,
    /// stack top
    stack_top: usize,
    /// stack data buffer
    data: Vec<u8>,
}

impl Stack {
    /// Create a stack image buffer of `capacity` bytes.
    ///
    /// `capacity` MUST be an upper bound on everything the caller will push
    /// (strings + pointer table + alignment): the copy in `push_slice_aligned`
    /// indexes `data` from its END, so an undersized buffer trips the assert
    /// there. It used to be a fixed 0x4000 (16 KiB), which a large argv/envp
    /// (a glob like `rm dir/*` expanding to thousands of paths) overran --
    /// panicking the whole kernel from an ordinary userspace command. Callers
    /// now size it to the actual image.
    #[allow(clippy::uninit_vec, unsafe_code)]
    fn new(sp: usize, capacity: usize) -> Self {
        let mut data = Vec::with_capacity(capacity);
        unsafe { data.set_len(capacity) };
        Stack {
            sp,
            stack_top: sp,
            data,
        }
    }
    /// push slice into stack
    #[allow(unsafe_code)]
    fn push_slice<T: Copy>(&mut self, vs: &[T]) {
        self.push_slice_aligned(vs, align_of::<T>());
    }

    #[allow(unsafe_code)]
    fn push_slice_aligned<T: Copy>(&mut self, vs: &[T], align: usize) {
        self.sp -= size_of_val(vs);
        self.sp -= self.sp % align;
        assert!(self.stack_top - self.sp <= self.data.len());
        let offset = self.data.len() - (self.stack_top - self.sp);
        // Copy as bytes, which has no alignment requirement.
        //
        // The `align` above is about the address the NEW PROCESS will see --
        // `sp` -- and says nothing about where that byte lands inside `data`.
        // This buffer is addressed from its END (`data.len() - (stack_top -
        // sp)`), so the alignment of the destination is the alignment of
        // `data.len()` plus whatever the allocator chose for a `Vec<u8>`,
        // which has alignment 1. Building a `&mut [usize]` over that address
        // is undefined behaviour whenever it does not happen to be 8-aligned,
        // and it does not happen to be as soon as the buffer is sized to its
        // contents rather than rounded to 16 KiB -- which is to say, on a
        // large argv. On x86 the store merely runs slower; on the aarch64 and
        // riscv64 targets a misaligned store faults.
        #[allow(unsafe_code)]
        unsafe {
            core::ptr::copy_nonoverlapping(
                vs.as_ptr() as *const u8,
                self.data.as_mut_ptr().add(offset),
                size_of_val(vs),
            );
        }
    }

    fn push_usize_slice(&mut self, vs: &[usize]) {
        self.push_slice_aligned(vs, align_of::<usize>());
    }

    /// push str into stack
    fn push_str(&mut self, s: &str) {
        self.push_slice(b"\0");
        self.push_slice(s.as_bytes());
    }

    /// Overwrite already-pushed bytes at absolute user address `addr`.
    ///
    /// Used by the FreeBSD stack builder to back-patch the `ps_strings` block
    /// (pushed high, before the argv/envp arrays exist) once the addresses of
    /// those arrays are known. `addr` must lie within the region already
    /// written (`[sp, stack_top)`).
    #[allow(dead_code)]
    fn write_at(&mut self, addr: usize, bytes: &[u8]) {
        debug_assert!(addr >= self.sp && addr + bytes.len() <= self.stack_top);
        let off = self.data.len() - (self.stack_top - addr);
        self.data[off..off + bytes.len()].copy_from_slice(bytes);
    }
}

/// FreeBSD auxiliary-vector types (`sys/sys/elf_common.h`). Only the entries a
/// FreeBSD `crt1`/libc reads to reach `main` are defined; the low ones
/// (`AT_PHDR`..`AT_ENTRY`) share their numbers with Linux, the rest are
/// FreeBSD-specific.
#[cfg(target_arch = "x86_64")]
mod fbsd_at {
    pub const AT_PHDR: usize = 3;
    pub const AT_PHENT: usize = 4;
    pub const AT_PHNUM: usize = 5;
    pub const AT_PAGESZ: usize = 6;
    pub const AT_BASE: usize = 7;
    pub const AT_ENTRY: usize = 9;
    pub const AT_EXECPATH: usize = 15;
    pub const AT_CANARY: usize = 16;
    pub const AT_CANARYLEN: usize = 17;
    pub const AT_OSRELDATE: usize = 18;
    pub const AT_NCPUS: usize = 19;
    pub const AT_PAGESIZES: usize = 20;
    pub const AT_PAGESIZESLEN: usize = 21;
    pub const AT_STACKPROT: usize = 23;
    pub const AT_EHDRFLAGS: usize = 24;
    pub const AT_HWCAP: usize = 25;
    pub const AT_PS_STRINGS: usize = 32;
    pub const AT_USRSTACKBASE: usize = 35;
    pub const AT_USRSTACKLIM: usize = 36;
    pub const AT_NULL: usize = 0;
}

/// The machine/process values the FreeBSD auxiliary vector carries that the ELF
/// loader computes per image (program headers, entry, interpreter base) plus
/// the few static ones (`__FreeBSD_version`, CPU count).
#[cfg(target_arch = "x86_64")]
pub struct FreebsdAuxv {
    /// `AT_PHDR`: user address of the program-header table.
    pub phdr: usize,
    /// `AT_PHENT`: size of one program-header entry.
    pub phent: usize,
    /// `AT_PHNUM`: number of program-header entries.
    pub phnum: usize,
    /// `AT_BASE`: interpreter load base (0 for a static binary).
    pub base: usize,
    /// `AT_ENTRY`: the program's own entry point.
    pub entry: usize,
    /// `AT_PAGESZ`: page size in bytes.
    pub pagesz: usize,
    /// `AT_EHDRFLAGS`: `e_flags` from the ELF header.
    pub ehdrflags: usize,
    /// `AT_OSRELDATE`: `__FreeBSD_version` we advertise.
    pub osreldate: usize,
    /// `AT_NCPUS`: number of CPUs.
    pub ncpus: usize,
    /// `AT_EXECPATH`: path the program was executed as.
    pub execpath: String,
}

#[cfg(target_arch = "x86_64")]
impl ProcInitInfo {
    /// Build a FreeBSD/amd64 initial stack and return the [`Stack`] image.
    ///
    /// The layout mirrors what FreeBSD's `exec_copyout_strings` produces
    /// (`sys/kern/kern_exec.c`): from the top down — the `ps_strings` block, the
    /// SSP canary, the `execpath` string, the page-size array, the environment
    /// and argument strings, then the `argc / argv[] / NULL / envp[] / NULL /
    /// auxv[]` table. `argc` ends up at the lowest written address, which is the
    /// value the caller loads into both `%rdi` and (aligned) `%rsp`.
    ///
    /// `ps_strings` is pushed first (highest address) but its `argv`/`envp`
    /// pointers are only known after the table is laid out, so it is back-patched
    /// via [`Stack::write_at`].
    pub fn push_at_freebsd(&self, stack_top: usize, aux: &FreebsdAuxv) -> Stack {
        use fbsd_at::*;
        // Size the buffer to the exact image (see push_at). ps_strings(32) +
        // canary(16) + pagesizes(8) + strings + a 19-entry auxv + tables.
        let table_entries = 1 + self.args.len() + 1 + self.envs.len() + 1 + 19 * 2 + 2;
        let strings: usize = aux.execpath.len()
            + 1
            + self.args.iter().map(|s| s.len() + 1).sum::<usize>()
            + self.envs.iter().map(|s| s.len() + 1).sum::<usize>();
        let needed = 32 + 16 + 8 + strings + table_entries * 8 + 32;
        let mut w = Stack::new(stack_top, needed.max(0x4000));

        // 1. ps_strings placeholder (32 bytes: argvstr, nargv, envstr, nenv).
        w.push_slice(&[0u8; 32]);
        let ps_strings_ptr = w.sp;

        // 2. SSP canary (16 random bytes).
        let mut canary = [0u8; 16];
        kernel_hal::rand::fill_random(&mut canary);
        w.push_slice(&canary);
        let canary_ptr = w.sp;

        // 3. execpath string.
        w.push_str(&aux.execpath);
        let execpath_ptr = w.sp;

        // 4. page-size array (a single entry: the base page size).
        w.push_slice(&[aux.pagesz as u64]);
        let pagesizes_ptr = w.sp;

        // 5. environment strings, then argument strings.
        let env_ptrs: Vec<usize> = self
            .envs
            .iter()
            .map(|e| {
                w.push_str(e);
                w.sp
            })
            .collect();
        let arg_ptrs: Vec<usize> = self
            .args
            .iter()
            .map(|a| {
                w.push_str(a);
                w.sp
            })
            .collect();

        // 6. The pointer/aux table. AT_PS_STRINGS is already known; AT_ARGV /
        //    AT_ENVV are deliberately omitted (FreeBSD csu reads argc/argv/envp
        //    straight off the stack), which avoids needing post-table addresses
        //    here.
        let mut table: Vec<usize> = Vec::new();
        table.push(self.args.len()); // argc
        table.extend_from_slice(&arg_ptrs);
        table.push(0); // argv NULL
        table.extend_from_slice(&env_ptrs);
        table.push(0); // envp NULL

        let auxv: [(usize, usize); 19] = [
            (AT_EXECPATH, execpath_ptr),
            (AT_PHDR, aux.phdr),
            (AT_PHENT, aux.phent),
            (AT_PHNUM, aux.phnum),
            (AT_BASE, aux.base),
            (AT_ENTRY, aux.entry),
            (AT_PAGESZ, aux.pagesz),
            (AT_OSRELDATE, aux.osreldate),
            (AT_NCPUS, aux.ncpus),
            (AT_CANARY, canary_ptr),
            (AT_CANARYLEN, canary.len()),
            (AT_PAGESIZES, pagesizes_ptr),
            (AT_PAGESIZESLEN, core::mem::size_of::<u64>()),
            (AT_STACKPROT, 0b11), // PROT_READ | PROT_WRITE
            (AT_EHDRFLAGS, aux.ehdrflags),
            (AT_HWCAP, 0),
            (AT_PS_STRINGS, ps_strings_ptr),
            (AT_USRSTACKBASE, stack_top),
            (AT_USRSTACKLIM, 8 * 1024 * 1024),
        ];
        for (ty, val) in auxv {
            table.push(ty);
            table.push(val);
        }
        table.push(AT_NULL);
        table.push(0);

        // 16-byte-align the final argc address, matching push_at().
        let mut sp = w.sp & !0x7;
        let table_size = table.len() * 8;
        if !(sp - table_size).is_multiple_of(16) {
            sp -= 8;
        }
        w.sp = sp;
        w.push_usize_slice(&table);
        let argc_addr = w.sp;

        // 7. Back-patch ps_strings now that the argv/envp arrays have addresses.
        let argv_addr = (argc_addr + 8) as u64;
        // argc(1) + argv[argc] + argv NULL(1), each 8 bytes, lands at envp[0].
        let envp_addr = (argc_addr + 8 * (1 + self.args.len() + 1)) as u64;
        let mut ps = [0u8; 32];
        ps[0..8].copy_from_slice(&argv_addr.to_le_bytes());
        ps[8..12].copy_from_slice(&(self.args.len() as u32).to_le_bytes());
        ps[16..24].copy_from_slice(&envp_addr.to_le_bytes());
        ps[24..28].copy_from_slice(&(self.envs.len() as u32).to_le_bytes());
        w.write_at(ps_strings_ptr, &ps);

        w
    }
}

impl Deref for Stack {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        let offset = self.data.len() - (self.stack_top - self.sp);
        &self.data[offset..]
    }
}

pub const AT_PHDR: u8 = 3;
pub const AT_PHENT: u8 = 4;
pub const AT_PHNUM: u8 = 5;
pub const AT_PAGESZ: u8 = 6;
pub const AT_BASE: u8 = 7;
pub const AT_ENTRY: u8 = 9;
pub const AT_UID: u8 = 11;
pub const AT_EUID: u8 = 12;
pub const AT_GID: u8 = 13;
pub const AT_EGID: u8 = 14;
pub const AT_SECURE: u8 = 23;
pub const AT_RANDOM: u8 = 25;
pub const AT_EXECFN: u8 = 31;

/// The identity block of the aux vector: who the kernel says this process is,
/// and whether the `execve` that started it raised its privileges.
///
/// Every C library reads these five before `main` and decides from them
/// whether the environment it was handed can be trusted. glibc sets
/// `__libc_enable_secure` from `AT_SECURE` alone; musl computes `libc.secure`
/// as "the four identity entries are not all there, **or** `AT_UID != AT_EUID`,
/// **or** `AT_GID != AT_EGID`, **or** `AT_SECURE != 0`"; GLib's
/// `g_check_setuid()` follows glibc. Secure mode is what makes the dynamic
/// loader drop `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `GCONV_PATH` and
/// the `MALLOC_*` hooks -- every knob that names a file the caller chose and
/// the privileged image then runs.
///
/// So the block has to be the truth in both directions. Claiming privilege
/// that was not granted puts every ordinary process in secure mode (which is
/// how `waybar` died: GLib refuses to autolaunch a D-Bus session bus under
/// `AT_SECURE`). Claiming safety that is not there hands a set-user-ID image
/// the caller's `LD_PRELOAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuxIdentity {
    /// Real user id (`AT_UID`).
    pub uid: u32,
    /// Effective user id (`AT_EUID`), after any set-user-ID bit on the image.
    pub euid: u32,
    /// Real group id (`AT_GID`).
    pub gid: u32,
    /// Effective group id (`AT_EGID`), after any set-group-ID bit.
    pub egid: u32,
    /// Linux's `bprm->secureexec` (`AT_SECURE`), as computed by
    /// `cap_bprm_creds_from_file()` once the ids above are final.
    pub secure: bool,
}

impl AuxIdentity {
    /// Write the five entries into an aux vector under construction.
    ///
    /// One rule, asked by every path that builds a stack, because the answer
    /// has to agree with itself: a C library that finds `AT_EUID == AT_UID`
    /// next to `AT_SECURE == 1` believes the second, and one that finds them
    /// different next to `AT_SECURE == 0` believes the first.
    pub fn insert_into(&self, auxv: &mut BTreeMap<u8, usize>) {
        auxv.insert(AT_UID, self.uid as usize);
        auxv.insert(AT_EUID, self.euid as usize);
        auxv.insert(AT_GID, self.gid as usize);
        auxv.insert(AT_EGID, self.egid as usize);
        auxv.insert(AT_SECURE, self.secure as usize);
    }
}

#[cfg(test)]
mod initial_stack_tests {
    //! The image `push_at` builds is the very first thing a new process sees:
    //! `_start` reads `argc` off the stack pointer it was given and walks
    //! forward from there. Everything here is arithmetic over one buffer, so
    //! it can be checked exactly -- and it has to be, because the failure mode
    //! is a program that dies in libc before `main`, with nothing in the log
    //! pointing back here.

    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    /// Somewhere plausible and 16-byte aligned, like the real stack top.
    const TOP: usize = 0x7fff_ffff_f000;

    /// A decoded initial stack, addressed the way the new process will.
    struct Image {
        bytes: Vec<u8>,
        sp: usize,
    }

    impl Image {
        fn of(args: &[&str], envs: &[&str], auxv: BTreeMap<u8, usize>) -> Image {
            let info = ProcInitInfo {
                args: args.iter().map(|s| s.to_string()).collect(),
                envs: envs.iter().map(|s| s.to_string()).collect(),
                auxv,
            };
            let stack = info.push_at(TOP);
            let bytes = stack.to_vec();
            Image {
                sp: TOP - bytes.len(),
                bytes,
            }
        }

        /// The machine word at address `addr`.
        fn word(&self, addr: usize) -> usize {
            let off = addr - self.sp;
            let mut w = [0u8; 8];
            w.copy_from_slice(&self.bytes[off..off + 8]);
            usize::from_ne_bytes(w)
        }

        /// The NUL-terminated string at address `addr`.
        fn cstr(&self, addr: usize) -> String {
            let off = addr - self.sp;
            let end = self.bytes[off..]
                .iter()
                .position(|&b| b == 0)
                .expect("a string on the stack must be NUL-terminated")
                + off;
            String::from_utf8_lossy(&self.bytes[off..end]).into_owned()
        }

        fn in_bounds(&self, addr: usize, len: usize) -> bool {
            addr >= self.sp && addr + len <= TOP
        }

        /// `argc`, then the argv strings, the envp strings, and the auxv pairs
        /// -- read exactly as `_start` walks them.
        fn decode(&self) -> (usize, Vec<String>, Vec<String>, Vec<(usize, usize)>) {
            let argc = self.word(self.sp);
            let mut at = self.sp + 8;
            let mut args = Vec::new();
            for _ in 0..argc {
                args.push(self.cstr(self.word(at)));
                at += 8;
            }
            assert_eq!(self.word(at), 0, "argv is not NULL-terminated");
            at += 8;
            let mut envs = Vec::new();
            while self.word(at) != 0 {
                envs.push(self.cstr(self.word(at)));
                at += 8;
            }
            at += 8;
            let mut auxv = Vec::new();
            loop {
                let (k, v) = (self.word(at), self.word(at + 8));
                at += 16;
                if k == 0 {
                    break;
                }
                auxv.push((k, v));
            }
            (argc, args, envs, auxv)
        }
    }

    fn auxv_of(pairs: &[(u8, usize)]) -> BTreeMap<u8, usize> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_process_reads_back_its_own_arguments_and_environment() {
        let img = Image::of(
            &["/bin/sh", "-c", "echo hola"],
            &["PATH=/usr/bin", "HOME=/root"],
            auxv_of(&[(AT_PAGESZ as u8, 4096)]),
        );
        let (argc, args, envs, _) = img.decode();
        assert_eq!(argc, 3);
        assert_eq!(args, vec!["/bin/sh", "-c", "echo hola"]);
        assert_eq!(envs, vec!["PATH=/usr/bin", "HOME=/root"]);
    }

    #[test]
    fn the_stack_pointer_is_sixteen_byte_aligned_whatever_is_on_it() {
        // The x86-64 ABI requires `rsp % 16 == 0` at `_start`, and libc's
        // early code uses SSE: a misaligned stack is a #GP inside the dynamic
        // loader before a single line of the program runs. The padding that
        // guarantees it depends on how many pointers and how many string
        // bytes there are, so walk both parities of each.
        for nargs in 0..6usize {
            for nenvs in 0..6usize {
                for pad in 0..3usize {
                    let args: Vec<String> = (0..nargs).map(|i| "a".repeat(i + pad + 1)).collect();
                    let envs: Vec<String> = (0..nenvs)
                        .map(|i| "E=".to_string() + &"e".repeat(i))
                        .collect();
                    let info = ProcInitInfo {
                        args: args.clone(),
                        envs,
                        auxv: auxv_of(&[(AT_PAGESZ as u8, 4096), (AT_BASE as u8, 0x1000)]),
                    };
                    let stack = info.push_at(TOP);
                    let sp = TOP - stack.len();
                    assert!(
                        sp.is_multiple_of(16),
                        "sp {:#x} is not 16-byte aligned for {} args and {} envs",
                        sp,
                        nargs,
                        nenvs
                    );
                }
            }
        }
    }

    #[test]
    fn the_pointers_point_inside_the_image_at_the_strings_themselves() {
        // A pointer past the end of what was copied into the stack VMO reads
        // as zero in the new process, so `argv[0]` becomes the empty string
        // and everything that logs a program name goes blank.
        let img = Image::of(
            &["/usr/bin/env", "sh"],
            &["LANG=C"],
            auxv_of(&[(AT_ENTRY as u8, 0x400000)]),
        );
        let argc = img.word(img.sp);
        for i in 0..argc {
            let p = img.word(img.sp + 8 + i * 8);
            assert!(img.in_bounds(p, 1), "argv[{}] points outside the image", i);
        }
        let (_, args, envs, _) = img.decode();
        assert_eq!(args[0], "/usr/bin/env");
        assert_eq!(envs[0], "LANG=C");
    }

    #[test]
    fn the_auxiliary_vector_carries_what_the_loader_put_there_and_ends_in_at_null() {
        // libc reads AT_PHDR/AT_PHNUM to find the program headers and
        // AT_PAGESZ for every mmap it rounds; a missing entry reads as zero
        // and the process divides by it.
        let img = Image::of(
            &["/bin/true"],
            &[],
            auxv_of(&[
                (AT_PHDR as u8, 0x400040),
                (AT_PHNUM as u8, 11),
                (AT_PAGESZ as u8, 4096),
                (AT_ENTRY as u8, 0x401000),
            ]),
        );
        let (_, _, _, auxv) = img.decode();
        for (key, want) in [
            (AT_PHDR, 0x400040usize),
            (AT_PHNUM, 11),
            (AT_PAGESZ, 4096),
            (AT_ENTRY, 0x401000),
        ] {
            let got = auxv.iter().find(|(k, _)| *k == key as usize);
            assert_eq!(got.map(|(_, v)| *v), Some(want), "auxv[{}] is wrong", key);
        }
    }

    #[test]
    fn at_random_points_at_sixteen_readable_bytes() {
        // Every glibc and musl start-up reads 16 bytes through this pointer
        // to seed the stack canary. Pointing it anywhere else is a read of
        // whatever happens to be there, or a fault.
        let img = Image::of(&["/bin/true"], &[], BTreeMap::new());
        let (_, _, _, auxv) = img.decode();
        let random = auxv
            .iter()
            .find(|(k, _)| *k == AT_RANDOM as usize)
            .expect("AT_RANDOM must always be supplied")
            .1;
        assert!(
            img.in_bounds(random, 16),
            "AT_RANDOM points at {:#x}, outside the image",
            random
        );
    }

    #[test]
    fn at_execfn_names_the_program() {
        // `AT_EXECFN` is what `/proc/self/comm` and every crash reporter
        // falls back to, and it is a separate copy from `argv[0]` -- a
        // program is free to rewrite its own `argv`.
        let img = Image::of(
            &["/usr/bin/gzdoom", "-iwad", "freedoom1.wad"],
            &[],
            BTreeMap::new(),
        );
        let (_, _, _, auxv) = img.decode();
        let execfn = auxv
            .iter()
            .find(|(k, _)| *k == AT_EXECFN as usize)
            .expect("AT_EXECFN must always be supplied")
            .1;
        assert_eq!(img.cstr(execfn), "/usr/bin/gzdoom");
    }

    #[test]
    fn a_huge_argument_list_does_not_overrun_the_image_buffer() {
        // `rm dir/*` on a directory with thousands of files expands to
        // thousands of argv entries. The buffer used to be a fixed 16 KiB and
        // the copy asserts on overrun -- so an ordinary shell command panicked
        // the kernel. The size has to be computed from the actual contents.
        let args: Vec<String> = (0..4000)
            .map(|i| alloc::format!("/tmp/dir/file{:06}", i))
            .collect();
        let envs: Vec<String> = (0..200)
            .map(|i| alloc::format!("VAR{}={}", i, "v".repeat(80)))
            .collect();
        let info = ProcInitInfo {
            args: args.clone(),
            envs: envs.clone(),
            auxv: auxv_of(&[(AT_PAGESZ as u8, 4096)]),
        };
        let stack = info.push_at(TOP);
        let img = Image {
            sp: TOP - stack.len(),
            bytes: stack.to_vec(),
        };
        let (argc, got_args, got_envs, _) = img.decode();
        assert_eq!(argc, 4000);
        assert_eq!(got_args.len(), 4000);
        assert_eq!(got_args[0], args[0]);
        assert_eq!(got_args[3999], args[3999]);
        assert_eq!(got_envs.len(), 200);
        assert!(img.sp.is_multiple_of(16));
    }

    #[test]
    fn an_empty_argument_list_builds_a_stack_instead_of_panicking() {
        // `sys_execve` refuses this before it gets here, but `push_at` is a
        // public function and indexing `args[0]` on an empty vector is a
        // kernel panic rather than an error.
        let img = Image::of(&[], &["PATH=/bin"], BTreeMap::new());
        let (argc, args, envs, _) = img.decode();
        assert_eq!(argc, 0);
        assert!(args.is_empty());
        assert_eq!(envs, vec!["PATH=/bin"]);
        assert!(img.sp.is_multiple_of(16));
    }

    #[test]
    fn an_empty_environment_is_still_null_terminated() {
        // `envp` with no entries is just the NULL, and a missing one sends
        // every `getenv` walking off the end of the stack.
        let img = Image::of(&["/bin/true"], &[], BTreeMap::new());
        let (argc, _, envs, auxv) = img.decode();
        assert_eq!(argc, 1);
        assert!(envs.is_empty());
        assert!(!auxv.is_empty(), "the auxv follows the envp NULL");
    }

    #[test]
    fn arguments_holding_spaces_and_empty_strings_survive_intact() {
        // `sh -c "echo a b"` passes one argument with spaces in it, and an
        // empty argument is legal too; both are just NUL-terminated strings.
        let img = Image::of(&["/bin/sh", "-c", "echo a  b", ""], &[], BTreeMap::new());
        let (argc, args, _, _) = img.decode();
        assert_eq!(argc, 4);
        assert_eq!(args[2], "echo a  b");
        assert_eq!(args[3], "", "an empty argument was dropped");
    }

    // --- The identity block -------------------------------------------------
    //
    // Five entries that no program reads on purpose and every C library reads
    // before `main`. They are the kernel's answer to "who am I, and can I
    // trust what I was handed", and a wrong answer is not a crash: it is a
    // privileged image quietly honouring the caller's `LD_PRELOAD`, or an
    // ordinary one quietly refusing to autolaunch a session bus.

    /// The identity block out of a decoded image, as (uid, euid, gid, egid,
    /// secure) -- panicking if any of the five is missing, because absent is
    /// itself an answer to a C library and never the one we mean.
    fn identity_of(img: &Image) -> (usize, usize, usize, usize, usize) {
        let (_, _, _, auxv) = img.decode();
        let get = |key: u8| {
            auxv.iter()
                .find(|(k, _)| *k == key as usize)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("the aux vector has no entry {}", key))
        };
        (
            get(AT_UID),
            get(AT_EUID),
            get(AT_GID),
            get(AT_EGID),
            get(AT_SECURE),
        )
    }

    fn image_for(id: AuxIdentity) -> Image {
        let mut auxv = auxv_of(&[(AT_PAGESZ, 4096)]);
        id.insert_into(&mut auxv);
        Image::of(&["/bin/sh"], &["PATH=/bin"], auxv)
    }

    #[test]
    fn the_identity_block_reaches_the_new_programs_stack() {
        let img = image_for(AuxIdentity {
            uid: 1000,
            euid: 0,
            gid: 1001,
            egid: 0,
            secure: true,
        });
        assert_eq!(identity_of(&img), (1000, 0, 1001, 0, 1));
    }

    #[test]
    fn at_uid_is_the_real_id_and_at_euid_the_effective_one() {
        // The one mistake that cannot be caught downstream: swap these two
        // and musl's own rule ("ruid != euid means secure") still fires, so
        // the program looks right while being told the opposite of the truth
        // about which id its file accesses will be checked against.
        let img = image_for(AuxIdentity {
            uid: 1000,
            euid: 0,
            gid: 1001,
            egid: 2,
            secure: true,
        });
        let (uid, euid, gid, egid, _) = identity_of(&img);
        assert_eq!(uid, 1000, "AT_UID is the REAL user id");
        assert_eq!(euid, 0, "AT_EUID is the EFFECTIVE user id");
        assert_eq!(gid, 1001, "AT_GID is the REAL group id");
        assert_eq!(egid, 2, "AT_EGID is the EFFECTIVE group id");
    }

    #[test]
    fn at_secure_is_the_zero_or_one_the_c_library_tests() {
        // glibc: `__libc_enable_secure = av->a_un.a_val != 0`. A bool written
        // as anything but 0 and 1 would still work there and still be wrong,
        // so pin the value rather than its truthiness.
        let safe = image_for(AuxIdentity::default());
        assert_eq!(identity_of(&safe).4, 0);
        let secure = image_for(AuxIdentity {
            secure: true,
            ..Default::default()
        });
        assert_eq!(identity_of(&secure).4, 1);
    }

    #[test]
    fn a_default_identity_is_root_and_not_secure() {
        // What a process the kernel starts by itself gets, and what every
        // process on this machine has got until now. Pinned so the change
        // that gives the block real values cannot quietly move the boot case
        // with it.
        let img = image_for(AuxIdentity::default());
        assert_eq!(identity_of(&img), (0, 0, 0, 0, 0));
    }

    #[test]
    fn the_block_replaces_whatever_was_there_before() {
        // The aux vector is built by accumulation, and the identity block is
        // written into a map that other code has already touched. A block
        // that merged instead of replacing would leave a stale AT_SECURE next
        // to fresh ids -- the exact disagreement that makes a C library pick
        // the wrong one of the two.
        let mut auxv = auxv_of(&[(AT_PAGESZ, 4096)]);
        AuxIdentity {
            uid: 7,
            euid: 7,
            gid: 7,
            egid: 7,
            secure: true,
        }
        .insert_into(&mut auxv);
        AuxIdentity::default().insert_into(&mut auxv);
        let img = Image::of(&["/bin/sh"], &[], auxv);
        assert_eq!(identity_of(&img), (0, 0, 0, 0, 0));
    }

    #[test]
    fn the_identity_block_does_not_displace_the_rest_of_the_aux_vector() {
        // Five more entries is five more pairs of words between the envp NULL
        // and the AT_NULL terminator, and `push_at` sizes its buffer up
        // front. AT_RANDOM and AT_EXECFN are pushed last, after the caller's
        // map, so they are the ones a miscount would truncate.
        let img = image_for(AuxIdentity {
            uid: 1000,
            euid: 0,
            gid: 1000,
            egid: 0,
            secure: true,
        });
        let (_, _, _, auxv) = img.decode();
        for key in [AT_PAGESZ, AT_RANDOM, AT_EXECFN] {
            assert!(
                auxv.iter().any(|(k, _)| *k == key as usize),
                "entry {} was lost",
                key
            );
        }
        let random = auxv
            .iter()
            .find(|(k, _)| *k == AT_RANDOM as usize)
            .expect("AT_RANDOM")
            .1;
        assert!(
            img.in_bounds(random, 16),
            "AT_RANDOM points at {:#x}, outside the image",
            random
        );
    }

    #[test]
    fn the_five_types_are_the_ones_linux_uses() {
        // Pinned against the literals from `include/uapi/linux/auxvec.h`, not
        // against each other. A program asks for these by number through
        // `getauxval`, so a wrong number is not a missing value: it is a
        // value delivered under someone else's name.
        assert_eq!(AT_UID, 11);
        assert_eq!(AT_EUID, 12);
        assert_eq!(AT_GID, 13);
        assert_eq!(AT_EGID, 14);
        assert_eq!(AT_SECURE, 23);
    }
}
