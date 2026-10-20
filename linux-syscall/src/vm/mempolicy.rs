//! `get_mempolicy(2)` on a kernel with one memory node.
//!
//! `/proc/<pid>/status` says `Mems_allowed: 1` and sysfs lists `node0`, so a
//! program sees a NUMA kernel with a single node. libcuda reads that from
//! `/proc/self/status` and then asks `get_mempolicy` in `cuInit`; answered
//! ENOSYS, it never opened `/dev/nvidia-uvm` and `cuCtxCreate` failed with
//! CUDA_ERROR_OUT_OF_MEMORY.
//!
//! No task or mapping here has a policy of its own: `set_mempolicy` and
//! `mbind` are still ENOSYS, so every policy is `default_policy`, and that is
//! what this answers, as `do_get_mempolicy` in `mm/mempolicy.c` does for it.

use super::*;

/// `MPOL_DEFAULT`, the mode `default_policy` reports.
const MPOL_DEFAULT: i32 = 0;
/// Report the node of a page, or of the next interleave allocation.
const MPOL_F_NODE: usize = 1 << 0;
/// Report the policy of the mapping at `addr`, not the task's.
const MPOL_F_ADDR: usize = 1 << 1;
/// Report the nodes the task may allocate on (`cpuset_current_mems_allowed`).
const MPOL_F_MEMS_ALLOWED: usize = 1 << 2;
/// `nr_node_ids`: node 0 only.
const NR_NODE_IDS: usize = 1;
/// The node mask with node 0 in it.
const NODE0: u64 = 1;

/// `do_get_mempolicy`: the policy word and the node mask for `flags`, where
/// `mapped` says whether an address lies in one of the caller's mappings.
fn get_mempolicy(
    addr: usize,
    flags: usize,
    mapped: impl FnOnce(usize) -> bool,
) -> LxResult<(i32, u64)> {
    if flags & !(MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & MPOL_F_MEMS_ALLOWED != 0 {
        if flags & (MPOL_F_NODE | MPOL_F_ADDR) != 0 {
            return Err(LxError::EINVAL);
        }
        return Ok((0, NODE0));
    }
    if flags & MPOL_F_ADDR != 0 {
        if !mapped(addr) {
            return Err(LxError::EFAULT);
        }
    } else if addr != 0 {
        return Err(LxError::EINVAL);
    }
    // `default_policy` writes an empty mask (`get_policy_nodemask`).
    match (flags & MPOL_F_NODE != 0, flags & MPOL_F_ADDR != 0) {
        // The node the page at `addr` is on: every page is on node 0.
        (true, true) => Ok((0, 0)),
        // The next node of an interleave policy, which no task has.
        (true, false) => Err(LxError::EINVAL),
        (false, _) => Ok((MPOL_DEFAULT, 0)),
    }
}

/// `copy_nodes_to_user`: the bytes written at `nmask` for a caller's
/// `maxnode`, `ALIGN(maxnode - 1, 64) / 8` of them, the mask first and zeros
/// after its one `long`. More than a page past that is EINVAL.
fn node_mask_bytes(maxnode: usize, nodes: u64) -> LxResult<Vec<u8>> {
    // Unsigned long arithmetic, wrapping as it does in C.
    let copy = (maxnode.wrapping_sub(1).wrapping_add(63) & !63) / 8;
    let nbytes = NR_NODE_IDS.div_ceil(64) * 8;
    if copy > nbytes && copy > PAGE_SIZE {
        return Err(LxError::EINVAL);
    }
    let mut out = vec![0u8; copy];
    let n = copy.min(nbytes);
    out[..n].copy_from_slice(&nodes.to_le_bytes()[..n]);
    Ok(out)
}

impl Syscall<'_> {
    /// `get_mempolicy(2)` (`kernel_get_mempolicy`): the policy the caller or
    /// the mapping at `addr` allocates under, and its nodes.
    pub fn sys_get_mempolicy(
        &self,
        mut policy: UserOutPtr<i32>,
        mut nmask: UserOutPtr<u8>,
        maxnode: usize,
        addr: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "get_mempolicy: maxnode={} addr={:#x} flags={:#x}",
            maxnode, addr, flags
        );
        if !nmask.is_null() && maxnode < NR_NODE_IDS {
            return Err(LxError::EINVAL);
        }
        let vmar = self.zircon_process().vmar();
        let (mode, nodes) = get_mempolicy(addr, flags, |addr| vmar.find_mapping(addr).is_some())?;
        if !policy.is_null() {
            policy.write(mode)?;
        }
        if !nmask.is_null() {
            nmask.write_array(&node_mask_bytes(maxnode, nodes)?)?;
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(_: usize) -> bool {
        true
    }

    fn unmapped(_: usize) -> bool {
        false
    }

    /// What libcuda asks in `cuInit`: `get_mempolicy(&mode, mask, 4, NULL, 0)`,
    /// which Linux answers MPOL_DEFAULT with an empty mask.
    #[test]
    fn the_task_policy_is_the_default_one_with_no_nodes() {
        assert_eq!(get_mempolicy(0, 0, unmapped), Ok((MPOL_DEFAULT, 0)));
        assert_eq!(node_mask_bytes(4, 0), Ok(vec![0; 8]));
    }

    #[test]
    fn the_nodes_a_task_may_use_are_node_0() {
        assert_eq!(
            get_mempolicy(0, MPOL_F_MEMS_ALLOWED, unmapped),
            Ok((0, NODE0))
        );
        assert_eq!(node_mask_bytes(64, NODE0), Ok(vec![1, 0, 0, 0, 0, 0, 0, 0]));
        for with in [MPOL_F_NODE, MPOL_F_ADDR] {
            assert_eq!(
                get_mempolicy(0, MPOL_F_MEMS_ALLOWED | with, mapped),
                Err(LxError::EINVAL)
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_einval() {
        assert_eq!(get_mempolicy(0, 1 << 3, mapped), Err(LxError::EINVAL));
    }

    /// An address is only looked at with MPOL_F_ADDR, and then it must be
    /// mapped.
    #[test]
    fn an_address_needs_mpol_f_addr_and_a_mapping() {
        assert_eq!(get_mempolicy(0x1000, 0, mapped), Err(LxError::EINVAL));
        assert_eq!(
            get_mempolicy(0x1000, MPOL_F_ADDR, unmapped),
            Err(LxError::EFAULT)
        );
        assert_eq!(
            get_mempolicy(0x1000, MPOL_F_ADDR, mapped),
            Ok((MPOL_DEFAULT, 0))
        );
    }

    /// MPOL_F_NODE with an address is the page's node; without one it is
    /// the next node of an interleave policy, and the default policy is not
    /// one.
    #[test]
    fn a_page_is_on_node_0_and_no_task_interleaves() {
        assert_eq!(
            get_mempolicy(0x1000, MPOL_F_NODE | MPOL_F_ADDR, mapped),
            Ok((0, 0))
        );
        assert_eq!(get_mempolicy(0, MPOL_F_NODE, mapped), Err(LxError::EINVAL));
    }

    /// `ALIGN(maxnode - 1, 64) / 8` bytes: none for one node, one `long`
    /// up to 65, then zeros, and EINVAL past a page.
    #[test]
    fn the_mask_is_as_long_as_maxnode_asks() {
        assert_eq!(node_mask_bytes(1, NODE0), Ok(vec![]));
        assert_eq!(node_mask_bytes(2, NODE0).map(|b| b.len()), Ok(8));
        assert_eq!(node_mask_bytes(65, NODE0).map(|b| b.len()), Ok(8));
        let wide = node_mask_bytes(66, NODE0).unwrap();
        assert_eq!(wide, [&[1, 0, 0, 0, 0, 0, 0, 0][..], &[0; 8]].concat());
        assert_eq!(
            node_mask_bytes(PAGE_SIZE * 8 + 1, NODE0).map(|b| b.len()),
            Ok(PAGE_SIZE)
        );
        assert_eq!(
            node_mask_bytes(PAGE_SIZE * 8 + 2, NODE0),
            Err(LxError::EINVAL)
        );
        // The C arithmetic wraps: the largest maxnode copies nothing.
        assert_eq!(node_mask_bytes(usize::MAX, NODE0), Ok(vec![]));
    }

    /// The call as a program makes it, from a thread of a process: what
    /// libcuda asks, then a mask too short for node 0, which is EINVAL
    /// before anything is written.
    #[test]
    fn the_syscall_writes_the_mode_and_the_mask() {
        use crate::on_a_thread::on_a_thread;
        use alloc::boxed::Box;
        use linux_object::process::LinuxProcess;
        use rcore_fs_ramfs::RamFS;
        use zircon_object::task::{Process, ROOT_JOB};

        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            45_401,
            "numa",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let (libcuda, short) = on_a_thread(&proc, |sys| {
            Box::pin(async move {
                let mut mode = -1i32;
                let mut mask = [0xffu8; 16];
                let mode_at = &mut mode as *mut i32 as usize;
                let mask_at = mask.as_mut_ptr() as usize;
                let r = sys.sys_get_mempolicy(mode_at.into(), mask_at.into(), 4, 0, 0);
                let libcuda = (r, mode, mask);
                let mut mode = -1i32;
                let mode_at = &mut mode as *mut i32 as usize;
                let r = sys.sys_get_mempolicy(mode_at.into(), mask_at.into(), 0, 0, 0);
                (libcuda, (r, mode))
            })
        });
        let mut want = [0xffu8; 16];
        want[..8].fill(0);
        assert_eq!(libcuda, (Ok(0), MPOL_DEFAULT, want), "8 bytes: one long");
        assert_eq!(short, (Err(LxError::EINVAL), -1));
    }
}
