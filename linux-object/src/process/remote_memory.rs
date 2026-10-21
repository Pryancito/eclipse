//! Another process's memory, as `process_vm_readv(2)` and
//! `process_vm_writev(2)` reach it: through its mappings, a page at a time,
//! and only where the process itself may read, or write, that page.
//!
//! Linux goes through `get_user_pages_remote()` without `FOLL_FORCE`, so a
//! page the target could not read itself (`PROT_NONE`, a hole) stops a read,
//! and one it could not write (`PROT_READ`) stops a write. What was copied
//! before the stop stays copied, and the caller is told how much.

use zircon_object::vm::{MMUFlags, VmAddressRegion, PAGE_SIZE};

/// Copy from `vmar` at `addr` into `buf`, as far as the process could read
/// that memory itself. Returns how many bytes were copied: `buf.len()`, or
/// fewer when a page on the way is missing or not readable.
pub fn read(vmar: &VmAddressRegion, addr: usize, buf: &mut [u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        let Some(n) = room(vmar, addr, done, buf.len(), MMUFlags::READ) else {
            break;
        };
        match vmar.read_memory(addr + done, &mut buf[done..done + n]) {
            Ok(got) if got == n => done += n,
            _ => break,
        }
    }
    done
}

/// Copy `buf` into `vmar` at `addr`, as far as the process could write that
/// memory itself. Returns how many bytes were copied, as [`read`] does.
pub fn write(vmar: &VmAddressRegion, addr: usize, buf: &[u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        let Some(n) = room(vmar, addr, done, buf.len(), MMUFlags::WRITE) else {
            break;
        };
        match vmar.write_memory(addr + done, &buf[done..done + n]) {
            Ok(put) if put == n => done += n,
            _ => break,
        }
    }
    done
}

/// How many of the `len - done` bytes left from `addr + done` can be copied
/// in one go: to the end of the page, if the page is mapped with `need`.
fn room(
    vmar: &VmAddressRegion,
    addr: usize,
    done: usize,
    len: usize,
    need: MMUFlags,
) -> Option<usize> {
    let at = addr.checked_add(done)?;
    let mapping = vmar.find_mapping(at)?;
    if !mapping.get_flags(at).ok()?.contains(need) {
        return None;
    }
    Some((len - done).min(PAGE_SIZE - at % PAGE_SIZE))
}

#[cfg(test)]
mod tests;
