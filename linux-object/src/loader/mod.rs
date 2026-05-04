//! Linux ELF Program Loader
#![deny(missing_docs)]

use {
    crate::error::LxResult,
    crate::fs::INodeExt,
    alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec},
    rcore_fs::vfs::INode,
    xmas_elf::ElfFile,
    zircon_object::{util::elf_loader::*, vm::*, ZxError},
};

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
    /// load a Linux ElfFile and return a tuple of (entry,sp,brk)
    pub fn load(
        &self,
        vmar: &Arc<VmAddressRegion>,
        data: &[u8],
        args: Vec<String>,
        envs: Vec<String>,
        path: String,
    ) -> LxResult<(VirtAddr, VirtAddr, VirtAddr)> {
        debug!(
            "load: vmar.addr & size: {:#x?}, data {:#x?}, args: {:?}, envs: {:?}",
            vmar.get_info(),
            data.as_ptr(),
            args,
            envs
        );

        let elf = ElfFile::new(data).map_err(|_| ZxError::INVALID_ARGS)?;

        debug!("elf info:  {:#x?}", elf.header.pt2);

        // ── Dynamically-linked binary ────────────────────────────────────────
        // Load the main binary AND the interpreter into separate child VMARs,
        // then hand control to the interpreter with a correctly populated auxv.
        if let Ok(interp) = elf.get_interpreter() {
            info!("interp: {:?}, path: {:?}", interp, path);

            // 1. Load the main application binary.
            let app_size = elf.load_segment_size();
            let app_vmar =
                vmar.allocate(None, app_size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)?;
            let app_base = app_vmar.addr();
            app_vmar.load_from_elf(&elf)?;
            let app_entry = app_base + elf.header.pt2.entry_point() as usize;

            match elf.relocate(app_vmar) {
                Ok(()) => info!("app elf relocate passed!"),
                Err(e) => warn!("app elf relocate: {:?} (may be OK for static binary)", e),
            }

            // 2. Load the interpreter (dynamic linker).
            let interp_inode = self.root_inode.lookup(interp)?;
            let interp_data = interp_inode.read_as_vec()?;
            let interp_elf =
                ElfFile::new(&interp_data).map_err(|_| ZxError::INVALID_ARGS)?;

            let interp_size = interp_elf.load_segment_size();
            let interp_vmar =
                vmar.allocate(None, interp_size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)?;
            let interp_base = interp_vmar.addr();
            interp_vmar.load_from_elf(&interp_elf)?;
            let interp_entry =
                interp_base + interp_elf.header.pt2.entry_point() as usize;

            match interp_elf.relocate(interp_vmar) {
                Ok(()) => info!("interp elf relocate passed!"),
                Err(e) => warn!("interp elf relocate: {:?}", e),
            }

            // 3. Build initial stack with auxv pointing at the main binary.
            let stack_vmo = VmObject::new_paged(self.stack_pages);
            let stack_flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
            let stack_bottom =
                vmar.map(None, stack_vmo.clone(), 0, stack_vmo.len(), stack_flags)?;
            let mut sp = stack_bottom + stack_vmo.len();

            let info = abi::ProcInitInfo {
                args,
                envs,
                auxv: {
                    let mut map = BTreeMap::new();
                    #[cfg(target_arch = "x86_64")]
                    {
                        // AT_BASE  = interpreter load base (for its own relocation)
                        // AT_PHDR  = virtual address of the app's PHDR table
                        // AT_ENTRY = app entry point (ld.so jumps here after reloc)
                        map.insert(abi::AT_BASE, interp_base);
                        map.insert(
                            abi::AT_PHDR,
                            app_base + elf.header.pt2.ph_offset() as usize,
                        );
                        map.insert(abi::AT_ENTRY, app_entry);
                    }
                    #[cfg(target_arch = "riscv64")]
                    {
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
                    map.insert(
                        abi::AT_PHENT,
                        elf.header.pt2.ph_entry_size() as usize,
                    );
                    map.insert(abi::AT_PHNUM, elf.header.pt2.ph_count() as usize);
                    map.insert(abi::AT_PAGESZ, PAGE_SIZE);
                    map
                },
            };
            let init_stack = info.push_at(sp);
            stack_vmo
                .write(self.stack_pages * PAGE_SIZE - init_stack.len(), &init_stack)?;
            sp -= init_stack.len();

            let brk = app_base + elf.load_segment_size();
            debug!(
                "dynamic load: interp_entry={:#x}, sp={:#x}, brk={:#x}",
                interp_entry, sp, brk
            );
            return Ok((interp_entry, sp, brk));
        }

        // ── Statically-linked (or no-interpreter) binary ─────────────────────
        let size = elf.load_segment_size();
        let image_vmar = vmar.allocate(None, size, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)?;
        let base = image_vmar.addr();
        let vmo = image_vmar.load_from_elf(&elf)?;
        let entry = base + elf.header.pt2.entry_point() as usize;

        debug!(
            "load: vmar.addr & size: {:#x?}, base: {:#x?}, entry: {:#x?}",
            vmar.get_info(),
            base,
            entry
        );

        // fill syscall entry
        if let Some(offset) = elf.get_symbol_address("rcore_syscall_entry") {
            vmo.write(offset as usize, &self.syscall_entry.to_ne_bytes())?;
        }

        match elf.relocate(image_vmar) {
            Ok(()) => info!("elf relocate passed !"),
            Err(error) => {
                // Segments stay mapped under `image_vmar.addr()`; do not clobber `base` with the
                // first program header vaddr (often not PT_LOAD). Wrong AT_BASE breaks PIE/musl
                // (e.g. user PC stuck at raw e_entry like 0x423a7 → page fault NOT_FOUND).
                warn!("elf relocate Err:{:?}, keeping load base {:#x}", error, base);
            }
        }

        let stack_vmo = VmObject::new_paged(self.stack_pages);
        let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
        let stack_bottom = vmar.map(None, stack_vmo.clone(), 0, stack_vmo.len(), flags)?;
        let mut sp = stack_bottom + stack_vmo.len();
        debug!("load stack bottom: {:#x}", stack_bottom);

        let info = abi::ProcInitInfo {
            args,
            envs,
            auxv: {
                let mut map = BTreeMap::new();
                #[cfg(target_arch = "x86_64")]
                {
                    map.insert(abi::AT_BASE, base);
                    map.insert(abi::AT_PHDR, base + elf.header.pt2.ph_offset() as usize);
                    map.insert(abi::AT_ENTRY, entry);
                }
                #[cfg(target_arch = "riscv64")]
                if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                    map.insert(abi::AT_PHDR, phdr_vaddr as usize);
                }
                #[cfg(target_arch = "aarch64")]
                {
                    map.insert(abi::AT_BASE, base);
                    map.insert(abi::AT_ENTRY, entry);
                    if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                        map.insert(abi::AT_PHDR, phdr_vaddr as usize);
                    }
                }
                map.insert(abi::AT_PHENT, elf.header.pt2.ph_entry_size() as usize);
                map.insert(abi::AT_PHNUM, elf.header.pt2.ph_count() as usize);
                map.insert(abi::AT_PAGESZ, PAGE_SIZE);
                map
            },
        };
        let init_stack = info.push_at(sp);
        stack_vmo.write(self.stack_pages * PAGE_SIZE - init_stack.len(), &init_stack)?;
        sp -= init_stack.len();

        debug!(
            "ProcInitInfo auxv: {:#x?}\nentry:{:#x}, sp:{:#x}",
            info.auxv, entry, sp
        );

        let brk = base + elf.load_segment_size();
        Ok((entry, sp, brk))
    }
}
