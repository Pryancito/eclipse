use crate::scheme::{Scheme, Stat};
use crate::scheme::error::*;
use alloc::vec::Vec;
use alloc::format;
use crate::process::current_process_id;

pub struct ProcScheme;

impl ProcScheme {
    pub fn new() -> Self {
        Self
    }
}

impl Scheme for ProcScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let normalized = path.trim_start_matches('/');
        
        if normalized.is_empty() {
            return Ok(0x1000); // /proc root dir
        }
        
        if normalized == "meminfo" {
            return Ok(0x2001);
        }
        
        if normalized == "cpuinfo" {
            return Ok(0x2002);
        }
        
        if normalized == "self/exe" {
             return Ok(0x3001); // Symbolic link
        }
        
        if normalized.starts_with("self/") {
             if let Some(pid) = current_process_id() {
                 let rest = &normalized[5..];
                 if rest == "status" {
                     return Ok(0x4000 | (pid as usize));
                 }
                 if rest == "maps" {
                     return Ok(0x5000 | (pid as usize));
                 }
             }
        }
        
        // Manejar /proc/[pid]/status
        if let Some(slash_pos) = normalized.find('/') {
            let pid_str = &normalized[..slash_pos];
            let rest = &normalized[slash_pos+1..];
            if let Ok(pid) = pid_str.parse::<u32>() {
                 if rest == "status" {
                     return Ok(0x4000 | (pid as usize));
                 }
                 if rest == "maps" {
                     return Ok(0x5000 | (pid as usize));
                 }
            }
        } else if let Ok(pid) = normalized.parse::<u32>() {
             return Ok(0x1000 | (pid as usize)); // Directory for PID
        }

        Err(ENOENT)
    }

    fn read(&self, id: usize, buf: &mut [u8], offset: u64) -> Result<usize, usize> {
        let mut content = Vec::new();
        
        if id == 0x2001 { // meminfo
            let (total_frames, used_frames) = crate::memory::get_memory_stats();
            content.extend_from_slice(format!("MemTotal:       {:8} kB\n", total_frames * 4).as_bytes());
            content.extend_from_slice(format!("MemFree:        {:8} kB\n", (total_frames - used_frames) * 4).as_bytes());
            content.extend_from_slice(b"MemAvailable:   ");
            content.extend_from_slice(format!("{:8} kB\n", (total_frames - used_frames) * 4).as_bytes());
        } else if id == 0x2002 { // cpuinfo
             content.extend_from_slice(b"processor\t: 0\nvendor_id\t: EclipseOS\nmodel name\t: Eclipse Optimized CPU\n");
        } else if id >= 0x4000 { // /proc/[pid]/status
            let pid = (id & 0x0FFF) as u32;
            if let Some(p) = crate::process::get_process(pid) {
                let proc = p.proc.lock();
                let name = core::str::from_utf8(&proc.name).unwrap_or("unknown").trim_matches('\0');
                content.extend_from_slice(format!("Name:\t{}\n", name).as_bytes());
                content.extend_from_slice(format!("State:\t{:?}\n", p.state).as_bytes());
                content.extend_from_slice(format!("Pid:\t{}\n", p.id).as_bytes());
                content.extend_from_slice(format!("PPid:\t{}\n", proc.parent_pid.unwrap_or(0)).as_bytes());
                content.extend_from_slice(format!("VmSize:\t{} kB\n", proc.mem_frames * 4).as_bytes());
                content.extend_from_slice(format!("Cwd:\t{}\n", core::str::from_utf8(&proc.cwd[..proc.cwd_len]).unwrap_or("/")).as_bytes());
            } else {
                return Err(ENOENT);
            }
        } else if id >= 0x5000 { // /proc/[pid]/maps
            let pid = (id & 0x0FFF) as u32;
            if let Some(p) = crate::process::get_process(pid) {
                let proc = p.proc.lock();
                let r_lock = proc.resources.lock();
                for vma in r_lock.vmas.iter() {
                    let r = if (vma.flags & 1) != 0 { "r" } else { "-" };
                    let w = if (vma.flags & 2) != 0 { "w" } else { "-" };
                    let x = if (vma.flags & 4) != 0 { "x" } else { "-" };
                    let obj = vma.object.lock();
                    let p_mode = if vma.is_shared { "s" } else { "p" };
                    let name = match obj.obj_type {
                        crate::vm_object::VMObjectType::Anonymous => "",
                        crate::vm_object::VMObjectType::Physical { .. } => " [phys]",
                        crate::vm_object::VMObjectType::File { .. } => " [file]",
                    };
                    content.extend_from_slice(format!("{:012x}-{:012x} {}{}{}{} {:08x} 00:00 0 {}\n", 
                        vma.start, vma.end, r, w, x, p_mode, vma.offset, name).as_bytes());
                }
            } else {
                return Err(ENOENT);
            }
        } else if id >= 0x1000 { // Directory read
             return Ok(0);
        } else if id == 0x3001 { // Symbolic link self/exe
             content.extend_from_slice(b"/bin/init");
        } else {
            return Err(EBADF);
        }
        
        if offset as usize >= content.len() {
            return Ok(0);
        }
        
        let count = core::cmp::min(buf.len(), content.len() - offset as usize);
        buf[..count].copy_from_slice(&content[offset as usize..offset as usize + count]);
        Ok(count)
    }

    fn write(&self, _id: usize, _buf: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(EROFS)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        // En una implementación real, manejaríamos el offset para archivos grandes.
        // Para /proc, que suele ser pequeño, podemos simplificar.
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o444; 
        stat.uid = 0;
        stat.gid = 0;
        Ok(0)
    }
}
