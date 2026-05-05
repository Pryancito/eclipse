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
    /// load a Linux ElfFile and return a tuple of (entry,sp)
    pub fn load(
        &self,
        vmar: &Arc<VmAddressRegion>,
        data: &[u8],
        args: Vec<String>,
        envs: Vec<String>,
        path: String,
    ) -> LxResult<(VirtAddr, VirtAddr)> {
        self.load_impl(vmar, data, args, envs, path, 0)
    }

    /// Maximum number of interpreter levels (shebang + ELF PT_INTERP combined).
    const MAX_INTERP_DEPTH: usize = 4;

    /// Internal recursive loader that tracks interpreter depth.
    fn load_impl(
        &self,
        vmar: &Arc<VmAddressRegion>,
        data: &[u8],
        args: Vec<String>,
        envs: Vec<String>,
        path: String,
        depth: usize,
    ) -> LxResult<(VirtAddr, VirtAddr)> {
        debug!(
            "load: vmar.addr & size: {:#x?}, data {:#x?}, args: {:?}, envs: {:?}",
            vmar.get_info(),
            data.as_ptr(),
            args,
            envs
        );

        if depth > Self::MAX_INTERP_DEPTH {
            error!("load: interpreter chain too deep (depth={})", depth);
            return Err(ZxError::INVALID_ARGS.into());
        }

        // Handle shebang scripts (#!).
        // Limit scan to the first 512 bytes to match typical OS shebang length restrictions.
        if data.starts_with(b"#!") {
            let scan_limit = data.len().min(512);
            let newline = data[..scan_limit]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(scan_limit);
            let line = core::str::from_utf8(&data[2..newline])
                .map_err(|_| ZxError::INVALID_ARGS)?
                .trim_end_matches('\r')
                .trim();
            // Split only on ASCII space/tab (POSIX shebang convention).
            let mut parts = line.splitn(2, |c: char| c == ' ' || c == '\t');
            let interp = match parts.next() {
                Some(i) if !i.is_empty() => i,
                _ => return Err(ZxError::INVALID_ARGS.into()),
            };
            let interp_arg = parts.next().map(|s| s.trim()).filter(|s| !s.is_empty());
            info!(
                "shebang: interp={:?}, arg={:?}, script={:?}",
                interp, interp_arg, path
            );
            let inode = self.root_inode.lookup(interp)?;
            let interp_data = inode.read_as_vec()?;
            let interp_path: String = interp.into();
            let mut new_args = vec![interp_path.clone()];
            if let Some(arg) = interp_arg {
                new_args.push(arg.into());
            }
            new_args.push(path);
            new_args.extend_from_slice(args.get(1..).unwrap_or_default());
            return self.load_impl(vmar, &interp_data, new_args, envs, interp_path, depth + 1);
        }

        let elf = ElfFile::new(data).map_err(|_| ZxError::INVALID_ARGS)?;

        debug!("elf info:  {:#x?}", elf.header.pt2);

        if let Ok(interp) = elf.get_interpreter() {
            info!("interp: {:?}, path: {:?}", interp, path);
            let inode = self.root_inode.lookup(interp)?;
            let data = inode.read_as_vec()?;
            let mut new_args = vec![interp.into(), path.clone()];
            new_args.extend_from_slice(args.get(1..).unwrap_or_default());
            return self.load_impl(vmar, &data, new_args, envs, path, depth + 1);
        }

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

        Ok((entry, sp))
    }
}
