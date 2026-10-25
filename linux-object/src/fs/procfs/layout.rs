//! Where a process's memory is, as `/proc/<pid>/stat` and `maps` say it:
//! fields 26-28 and 45-51 of `stat`, and the `[heap]` and `[stack]` names of
//! `maps`. Linux writes both from `mm_struct`; here they come from what the
//! loader recorded ([`ImageLayout`], [`ArgArea`]) and the current break.
//! And how much of it there is of each kind, as `status` and `statm` say it
//! ([`Sizes`]). Split out of `procfs.rs` for the size ratchet.

use super::*;
use crate::loader::{ArgArea, ImageLayout};
use zircon_object::vm::{round_down_pages, roundup_pages, MMUFlags, MappingDump};

/// A process's `mm` as these two files read it. A zombie has none, and
/// neither has a process no loader started; for that one `stat` reads 0 and
/// `maps` names nothing.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Mm {
    pub layout: Option<ImageLayout>,
    pub args: Option<ArgArea>,
    /// The current break, `mm->brk`.
    pub brk: usize,
}

impl Mm {
    /// `proc`'s, or `None` once it has exited.
    pub fn of(proc: &Process) -> Option<Self> {
        if matches!(proc.status(), Status::Exited(_)) {
            return None;
        }
        let lp = proc.try_linux()?;
        Some(Mm {
            layout: lp.image_layout(),
            args: lp.arg_area(),
            brk: lp.brk(),
        })
    }
}

/// Fields 26-28 and 45-51 of `stat` into `rest` (indexed by `field - 5`),
/// as `do_task_stat` writes them: 0 without an `mm`; for a reader that may
/// not look inside the process (`permitted`, which is
/// `PTRACE_MODE_READ_FSCREDS`), 1 for the two code bounds and 0 for the rest,
/// so it learns there is code and not where.
pub(super) fn fill_stat(rest: &mut [i64; 48], mm: Option<Mm>, permitted: bool) {
    let Some(mm) = mm else {
        return;
    };
    if !permitted {
        rest[26 - 5] = 1;
        rest[27 - 5] = 1;
        return;
    }
    let l = mm.layout.unwrap_or_default();
    let a = mm.args.unwrap_or_default();
    for (field, value) in [
        (26, l.start_code),
        (27, l.end_code),
        (28, l.start_stack),
        (45, l.start_data),
        (46, l.end_data),
        (47, l.start_brk),
        (48, a.arg_start),
        (49, a.arg_end),
        (50, a.env_start),
        (51, a.env_end),
    ] {
        rest[field - 5] = value as i64;
    }
}

/// What `maps` calls `row`: its object's name, which is a file's path for a
/// file and `[vdso]` for the vDSO; for anonymous memory, what `get_vma_name`
/// calls it -- `[heap]` if it overlaps the break's range (`vma_is_initial_heap`),
/// `[stack]` if it holds the first stack pointer (`vma_is_initial_stack`),
/// and nothing otherwise.
pub(super) fn row_name<'a>(row: &'a MappingDump, mm: Option<&Mm>) -> &'a str {
    if !row.name.is_empty() {
        return &row.name;
    }
    let Some((l, brk)) = mm.and_then(|mm| Some((mm.layout?, mm.brk))) else {
        return "";
    };
    if row.start < brk && row.end > l.start_brk {
        "[heap]"
    } else if row.start <= l.start_stack && row.end >= l.start_stack {
        "[stack]"
    } else {
        ""
    }
}

/// The sizes Linux keeps per kind of mapping, which `status` writes as
/// `VmData:`, `VmStk:`, `VmExe:` and `VmLib:` and `statm` as its text and
/// data, in bytes. `vm_stat_account` sorts each VMA by its flags: the stack
/// is `stack_vm`; otherwise writable and private is `data_vm`, and
/// executable and not writable is `exec_vm`. Here a row is the stack if it
/// is of the object [`row_name`] calls `[stack]`, so the stack stays the
/// stack after an `mprotect` splits it, as `VM_GROWSDOWN` does in Linux.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Sizes {
    /// `data_vm`: what a program's own allocations grow (`VmData`).
    pub data: usize,
    /// `stack_vm` (`VmStk`).
    pub stack: usize,
    /// `exec_vm`: the executable's code and every library's (`VmExe` plus
    /// `VmLib`).
    pub exec: usize,
    /// `PAGE_ALIGN(end_code) - (start_code & PAGE_MASK)`: the executable's
    /// own code, which `statm` writes as it is.
    pub text: usize,
}

impl Sizes {
    /// What `rows` hold, kind by kind. Without an `mm` (a zombie) there is
    /// nothing, and without a record there is no code range and no stack.
    pub fn of(rows: &[MappingDump], mm: Option<&Mm>) -> Self {
        let Some(mm) = mm else {
            return Self::default();
        };
        let l = mm.layout.unwrap_or_default();
        let stack = rows
            .iter()
            .find(|r| row_name(r, Some(mm)) == "[stack]")
            .map(|r| r.vmo_id);
        let mut sizes = Sizes {
            text: roundup_pages(l.end_code).saturating_sub(round_down_pages(l.start_code)),
            ..Self::default()
        };
        for r in rows {
            let len = r.end - r.start;
            let write = r.flags.contains(MMUFlags::WRITE);
            if Some(r.vmo_id) == stack {
                sizes.stack += len;
            } else if write && !r.shared {
                sizes.data += len;
            } else if !write && r.flags.contains(MMUFlags::EXECUTE) {
                sizes.exec += len;
            }
        }
        sizes
    }

    /// `VmExe`: the text, but never more than what is executable.
    pub fn exe(&self) -> usize {
        self.text.min(self.exec)
    }

    /// `VmLib`: the executable memory that is not the text.
    pub fn lib(&self) -> usize {
        self.exec - self.exe()
    }
}

#[cfg(test)]
mod tests;
