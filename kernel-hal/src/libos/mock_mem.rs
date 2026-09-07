use std::os::unix::io::RawFd;

use nix::fcntl::{self, OFlag};
use nix::sys::mman::{self, MapFlags, ProtFlags};
use nix::{sys::stat::Mode, unistd};

use super::mem::PMEM_MAP_VADDR;
use crate::{MMUFlags, PhysAddr, VirtAddr};

pub struct MockMemory {
    size: usize,
    fd: RawFd,
}

impl MockMemory {
    pub fn new(size: usize) -> Self {
        let dir = tempfile::tempdir().expect("failed to create pmem directory");
        let path = dir.path().join("zcore_libos_pmem");

        let fd = fcntl::open(
            &path,
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
            Mode::S_IRWXU,
        )
        .expect("faild to open");
        unistd::ftruncate(fd, size as _).expect("failed to set size of shared memory!");

        let mem = Self { size, fd };
        mem.mmap(PMEM_MAP_VADDR, size, 0, MMUFlags::READ | MMUFlags::WRITE);
        mem
    }

    /// Mmap `paddr` to `vaddr` in frame file.
    pub fn mmap(&self, vaddr: VirtAddr, len: usize, paddr: PhysAddr, prot: MMUFlags) {
        assert!(paddr < self.size);
        assert!(paddr + len <= self.size);

        // Intel macOS permits writable executable aliases and historically
        // needed them for the hosted vDSO. Apple Silicon enforces W^X, while
        // the AArch64 prebuilts keep writable and executable segments on
        // separate 16 KiB pages.
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        let prot = if prot.contains(MMUFlags::EXECUTE) {
            prot | MMUFlags::WRITE
        } else {
            prot
        };

        let prot_noexec = ProtFlags::from(prot) - ProtFlags::PROT_EXEC;
        let flags = MapFlags::MAP_SHARED | MapFlags::MAP_FIXED;
        let fd = self.fd;
        let offset = paddr as _;
        trace!(
            "mmap file: fd={}, offset={:#x}, len={:#x}, vaddr={:#x}, prot={:?}",
            fd,
            offset,
            len,
            vaddr,
            prot,
        );

        unsafe { mman::mmap(vaddr as _, len, prot_noexec, flags, fd, offset) }.unwrap_or_else(
            |err| {
                panic!(
                    "failed to mmap: fd={}, offset={:#x}, len={:#x}, vaddr={:#x}, prot={:?}: {:?}",
                    fd, offset, len, vaddr, prot, err
                )
            },
        );
        if prot.contains(MMUFlags::EXECUTE) {
            // The range was mapped just above, so a "not mapped" answer here
            // would be a mock bug, not a lazy page.
            assert!(
                self.mprotect(vaddr, len, prot),
                "mprotect(EXECUTE) right after mmap found no mapping at {:#x}",
                vaddr
            );
        }
    }

    pub fn munmap(&self, vaddr: VirtAddr, len: usize) {
        unsafe { mman::munmap(vaddr as _, len) }
            .unwrap_or_else(|err| panic!("failed to munmap: vaddr={:#x}: {:?}", vaddr, err));
    }

    /// Change the host protection of `[vaddr, vaddr+len)`.
    ///
    /// Returns `false` when the host has no mapping there (`ENOMEM`): the
    /// guest page was reserved but never faulted in, which on bare metal is
    /// simply a not-present PTE that `GenericPageTable::update` reports as
    /// `NotMapped` and callers ignore. Panicking on it made `VmMapping::
    /// protect` over a lazy `mmap(PROT_NONE)` reservation abort under libos
    /// while the same code is correct on hardware. Any other failure is a
    /// genuine mock bug and still panics.
    pub fn mprotect(&self, vaddr: VirtAddr, len: usize, prot: MMUFlags) -> bool {
        match unsafe { mman::mprotect(vaddr as _, len, prot.into()) } {
            Ok(()) => true,
            Err(nix::errno::Errno::ENOMEM) => false,
            Err(err) => panic!(
                "failed to mprotect: vaddr={:#x}, prot={:?}: {:?}",
                vaddr, prot, err
            ),
        }
    }

    pub fn phys_to_virt(&self, paddr: PhysAddr) -> VirtAddr {
        assert!(paddr < self.size);
        PMEM_MAP_VADDR + paddr
    }

    pub fn as_ptr<T>(&self, paddr: PhysAddr) -> *const T {
        self.phys_to_virt(paddr) as _
    }

    pub fn as_mut_ptr<T>(&self, paddr: PhysAddr) -> *mut T {
        self.phys_to_virt(paddr) as _
    }
}

impl Drop for MockMemory {
    fn drop(&mut self) {
        trace!("Drop MockMemory: fd={:?}", self.fd);
        unistd::close(self.fd).expect("failed to close shared memory file!");
    }
}

impl From<MMUFlags> for ProtFlags {
    fn from(f: MMUFlags) -> Self {
        let mut flags = Self::empty();
        if f.contains(MMUFlags::READ) {
            flags |= ProtFlags::PROT_READ;
        }
        if f.contains(MMUFlags::WRITE) {
            flags |= ProtFlags::PROT_WRITE;
        }
        if f.contains(MMUFlags::EXECUTE) {
            flags |= ProtFlags::PROT_EXEC;
        }
        flags
    }
}
