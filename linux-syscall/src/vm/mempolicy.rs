//! `get_mempolicy(2)`, `set_mempolicy(2)` and `mbind(2)` on a kernel with
//! one memory node.
//!
//! `/proc/<pid>/status` says `Mems_allowed: 1` and sysfs lists `node0`, so a
//! program sees a NUMA kernel with a single node. libcuda reads that from
//! `/proc/self/status` and then asks `get_mempolicy` in `cuInit`; answered
//! ENOSYS, it never opened `/dev/nvidia-uvm` and `cuCtxCreate` failed with
//! CUDA_ERROR_OUT_OF_MEMORY.
//!
//! `set_mempolicy` and `mbind` check what they are asked as
//! `kernel_set_mempolicy` and `kernel_mbind` do, and a policy Linux would
//! take is taken. None is kept: with one node, every policy allocates where
//! `default_policy` does, on node 0, and no page has anywhere to move. So
//! the one departure is what `get_mempolicy` reports afterwards, which is
//! still `default_policy`, as `do_get_mempolicy` in `mm/mempolicy.c` reports
//! it. Another is in `mbind`: with MPOL_MF_STRICT and a policy that names no
//! node (MPOL_LOCAL, or MPOL_PREFERRED with an empty mask), Linux answers EIO
//! once a page of the range is present, as no page is on a node of the
//! mask, and this does not.

use super::*;
use linux_object::process::CAP_SYS_NICE;

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

/// `MPOL_DEFAULT` as [`sanitize_mpol_flags`] gives a mode.
const MPOL_DEFAULT_MODE: u32 = MPOL_DEFAULT as u32;
/// `MPOL_PREFERRED`: allocate on the mask's first node, or locally when the
/// mask is empty.
const MPOL_PREFERRED: u32 = 1;
/// `MPOL_BIND`: allocate on the mask's nodes only.
const MPOL_BIND: u32 = 2;
/// `MPOL_LOCAL`: allocate on the node of the allocating CPU.
const MPOL_LOCAL: u32 = 4;
/// `MPOL_PREFERRED_MANY`: prefer the mask's nodes.
const MPOL_PREFERRED_MANY: u32 = 5;
/// `MPOL_MAX`, one past `MPOL_WEIGHTED_INTERLEAVE`.
const MPOL_MAX: u32 = 7;
/// The mask is the nodes themselves, whatever the task may use later.
const MPOL_F_STATIC_NODES: u32 = 1 << 15;
/// The mask is relative to the nodes the task may use.
const MPOL_F_RELATIVE_NODES: u32 = 1 << 14;
/// Let NUMA balancing move pages within the mask.
const MPOL_F_NUMA_BALANCING: u32 = 1 << 13;
/// `MPOL_MODE_FLAGS`: the flags that ride in the mode word.
const MPOL_MODE_FLAGS: u32 = MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES | MPOL_F_NUMA_BALANCING;
/// `mbind`: fail with EIO if a page of the range is not on the policy's
/// nodes.
const MPOL_MF_STRICT: u32 = 1 << 0;
/// `mbind`: move the range's pages that only this process maps.
const MPOL_MF_MOVE: u32 = 1 << 1;
/// `mbind`: move every page of the range, which needs CAP_SYS_NICE.
const MPOL_MF_MOVE_ALL: u32 = 1 << 2;
/// `MPOL_MF_VALID`: the flags `mbind` takes.
const MPOL_MF_VALID: u32 = MPOL_MF_STRICT | MPOL_MF_MOVE | MPOL_MF_MOVE_ALL;
/// `MAX_NUMNODES` of an x86_64 distribution kernel (`NODES_SHIFT` 10): a
/// mask may be that long, and a node past it is EINVAL. A whole number of
/// `long`s, which [`nodes_in`] counts on.
const MAX_NUMNODES: usize = 1 << 10;

/// What a node mask names here, where node 0 is the only node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Nodes {
    /// Node 0 is in it.
    node0: bool,
    /// A node this kernel does not have is in it.
    other: bool,
}

impl Nodes {
    fn is_empty(self) -> bool {
        !self.node0 && !self.other
    }
}

/// `sanitize_mpol_flags`: the mode and its flags out of the caller's mode
/// word, an `int`.
fn sanitize_mpol_flags(word: usize) -> LxResult<(u32, u32)> {
    let word = word as u32;
    let flags = word & MPOL_MODE_FLAGS;
    let mode = word & !MPOL_MODE_FLAGS;
    if mode >= MPOL_MAX {
        return Err(LxError::EINVAL);
    }
    if flags & MPOL_F_STATIC_NODES != 0 && flags & MPOL_F_RELATIVE_NODES != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & MPOL_F_NUMA_BALANCING != 0 && mode != MPOL_BIND && mode != MPOL_PREFERRED_MANY {
        return Err(LxError::EINVAL);
    }
    Ok((mode, flags))
}

/// `get_nodes`: how many bits of the caller's mask to read, `maxnode - 1`
/// (one short of what it says, which Linux keeps for compatibility), or
/// none for no mask. More than a page of them is EINVAL.
fn mask_bits(no_mask: bool, maxnode: usize) -> LxResult<Option<usize>> {
    let bits = maxnode.wrapping_sub(1);
    if bits == 0 || no_mask {
        return Ok(None);
    }
    if bits > PAGE_SIZE * 8 {
        return Err(LxError::EINVAL);
    }
    Ok(Some(bits))
}

/// `get_nodes`: the nodes in the first `bits` bits of `words`, which are the
/// caller's (the rest of the last word is not). A node past
/// [`MAX_NUMNODES`] is EINVAL.
fn nodes_in(words: &[u64], bits: usize) -> LxResult<Nodes> {
    let mut nodes = Nodes::default();
    for (i, &word) in words.iter().enumerate() {
        let first = i * 64;
        let word = match bits.saturating_sub(first) {
            0 => 0,
            n if n < 64 => word & ((1 << n) - 1),
            _ => word,
        };
        if word == 0 {
            continue;
        }
        if first >= MAX_NUMNODES {
            return Err(LxError::EINVAL);
        }
        nodes.node0 |= first == 0 && word & 1 != 0;
        nodes.other |= first != 0 || word & !1 != 0;
    }
    Ok(nodes)
}

/// `mpol_new` and `mpol_set_nodemask`: whether `mode` with `flags` can be
/// the policy over `nodes`, where the only node a task may use is node 0.
fn check_policy(mode: u32, flags: u32, nodes: Nodes) -> LxResult<()> {
    let pinned = flags & (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES) != 0;
    let ok = match mode {
        MPOL_DEFAULT_MODE => nodes.is_empty(),
        // An empty mask makes MPOL_PREFERRED local allocation.
        MPOL_PREFERRED if nodes.is_empty() => !pinned,
        MPOL_LOCAL => nodes.is_empty() && !pinned,
        _ if nodes.is_empty() => false,
        // The nodes it gets are the mask's that the task may use or, with
        // MPOL_F_RELATIVE_NODES, the mask folded onto them, which for one
        // node is node 0 whatever the mask. None is EINVAL.
        _ => flags & MPOL_F_RELATIVE_NODES != 0 || nodes.node0,
    };
    if ok {
        Ok(())
    } else {
        Err(LxError::EINVAL)
    }
}

/// `do_mbind`, up to the policy: the length of the range, or `None` for an
/// empty one, which is done. MPOL_MF_MOVE_ALL needs `may_move_all`
/// (CAP_SYS_NICE).
fn mbind_range(
    start: usize,
    len: usize,
    flags: u32,
    may_move_all: impl FnOnce() -> bool,
) -> LxResult<Option<usize>> {
    if flags & !MPOL_MF_VALID != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & MPOL_MF_MOVE_ALL != 0 && !may_move_all() {
        return Err(LxError::EPERM);
    }
    if !start.is_multiple_of(PAGE_SIZE) {
        return Err(LxError::EINVAL);
    }
    // `PAGE_ALIGN`, wrapping as it does in C.
    let len = len.wrapping_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if start.checked_add(len).is_none() {
        return Err(LxError::EINVAL);
    }
    Ok((len != 0).then_some(len))
}

impl Syscall<'_> {
    /// `get_nodes`: the nodes of the caller's mask, `maxnode - 1` bits at
    /// `nmask`.
    fn get_nodes(&self, nmask: UserInPtr<u64>, maxnode: usize) -> LxResult<Nodes> {
        let Some(bits) = mask_bits(nmask.is_null(), maxnode)? else {
            return Ok(Nodes::default());
        };
        nodes_in(&nmask.read_array(bits.div_ceil(64))?, bits)
    }

    /// `set_mempolicy(2)` (`kernel_set_mempolicy`): the policy the caller's
    /// thread allocates under, checked and not kept (see the module).
    pub fn sys_set_mempolicy(
        &self,
        mode: usize,
        nmask: UserInPtr<u64>,
        maxnode: usize,
    ) -> SysResult {
        info!("set_mempolicy: mode={:#x} maxnode={}", mode, maxnode);
        let (mode, flags) = sanitize_mpol_flags(mode)?;
        let nodes = self.get_nodes(nmask, maxnode)?;
        check_policy(mode, flags, nodes)?;
        Ok(0)
    }

    /// `mbind(2)` (`kernel_mbind`): the policy the pages of a range are
    /// allocated under, checked and not kept (see the module). Every page of
    /// the range must be mapped.
    pub fn sys_mbind(
        &self,
        start: usize,
        len: usize,
        mode: usize,
        nmask: UserInPtr<u64>,
        maxnode: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "mbind: start={:#x} len={:#x} mode={:#x} maxnode={} flags={:#x}",
            start, len, mode, maxnode, flags
        );
        let (mode, mode_flags) = sanitize_mpol_flags(mode)?;
        let nodes = self.get_nodes(nmask, maxnode)?;
        let may_move_all = || self.linux_process().capable(CAP_SYS_NICE);
        let Some(len) = mbind_range(start, len, flags as u32, may_move_all)? else {
            return Ok(0);
        };
        check_policy(mode, mode_flags, nodes)?;
        // `queue_pages_range`: a hole anywhere in the range.
        if !self.zircon_process().vmar().range_is_mapped(start, len) {
            return Err(LxError::EFAULT);
        }
        Ok(0)
    }

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

    /// The mode word is an `int` with the mode flags in it: any mode up to
    /// MPOL_WEIGHTED_INTERLEAVE, the two node flags apart, and NUMA
    /// balancing only for the policies that bind.
    #[test]
    fn the_mode_word_is_sanitized_as_linux_does() {
        for mode in 0..MPOL_MAX {
            assert_eq!(sanitize_mpol_flags(mode as usize), Ok((mode, 0)));
        }
        assert_eq!(sanitize_mpol_flags(MPOL_MAX as usize), Err(LxError::EINVAL));
        assert_eq!(sanitize_mpol_flags(u32::MAX as usize), Err(LxError::EINVAL));
        assert_eq!(
            sanitize_mpol_flags((1 << 32) | MPOL_BIND as usize),
            Ok((MPOL_BIND, 0))
        );
        let both = (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES | MPOL_BIND) as usize;
        assert_eq!(sanitize_mpol_flags(both), Err(LxError::EINVAL));
        let static_bind = (MPOL_F_STATIC_NODES | MPOL_BIND) as usize;
        assert_eq!(
            sanitize_mpol_flags(static_bind),
            Ok((MPOL_BIND, MPOL_F_STATIC_NODES))
        );
        for (mode, ok) in [
            (MPOL_BIND, true),
            (MPOL_PREFERRED_MANY, true),
            (MPOL_INTERLEAVE, false),
            (MPOL_PREFERRED, false),
        ] {
            let word = (MPOL_F_NUMA_BALANCING | mode) as usize;
            let want = if ok {
                Ok((mode, MPOL_F_NUMA_BALANCING))
            } else {
                Err(LxError::EINVAL)
            };
            assert_eq!(sanitize_mpol_flags(word), want, "mode {}", mode);
        }
    }

    /// `maxnode - 1` bits are read, none without a mask, and no more than a
    /// page of them.
    #[test]
    fn a_mask_is_one_bit_shorter_than_maxnode() {
        assert_eq!(mask_bits(true, 64), Ok(None));
        assert_eq!(mask_bits(false, 1), Ok(None));
        assert_eq!(mask_bits(false, 2), Ok(Some(1)));
        assert_eq!(mask_bits(false, 65), Ok(Some(64)));
        assert_eq!(mask_bits(false, PAGE_SIZE * 8 + 1), Ok(Some(PAGE_SIZE * 8)));
        assert_eq!(mask_bits(false, PAGE_SIZE * 8 + 2), Err(LxError::EINVAL));
        // A mask with maxnode 0 wraps to the largest count, as in C.
        assert_eq!(mask_bits(false, 0), Err(LxError::EINVAL));
        assert_eq!(mask_bits(true, 0), Ok(None));
    }

    /// `MPOL_INTERLEAVE`: allocate across the mask's nodes in turn.
    const MPOL_INTERLEAVE: u32 = 3;

    const NODE0_ONLY: Nodes = Nodes {
        node0: true,
        other: false,
    };
    const OTHER_ONLY: Nodes = Nodes {
        node0: false,
        other: true,
    };

    /// Only the bits the caller gave count; a node this kernel lacks is
    /// still a node up to MAX_NUMNODES, and past it EINVAL.
    #[test]
    fn the_nodes_are_read_from_the_bits_given() {
        assert_eq!(nodes_in(&[1], 1), Ok(NODE0_ONLY));
        // Bit 1 is past the one bit read.
        assert_eq!(nodes_in(&[2], 1), Ok(Nodes::default()));
        assert_eq!(nodes_in(&[2], 2), Ok(OTHER_ONLY));
        assert_eq!(nodes_in(&[0, 1], 128), Ok(OTHER_ONLY), "node 64");
        assert_eq!(
            nodes_in(&[3], 64),
            Ok(Nodes {
                node0: true,
                other: true
            })
        );
        let mut words = [0u64; 17];
        words[15] = 1 << 63;
        assert_eq!(nodes_in(&words, 17 * 64), Ok(OTHER_ONLY), "node 1023");
        words[16] = 1;
        assert_eq!(nodes_in(&words, 17 * 64), Err(LxError::EINVAL), "node 1024");
        assert_eq!(nodes_in(&words, 16 * 64), Ok(OTHER_ONLY), "not read");
        // Zeros past MAX_NUMNODES are fine.
        let mut zeros = [0u64; 32];
        zeros[0] = 1;
        assert_eq!(nodes_in(&zeros, 32 * 64), Ok(NODE0_ONLY));
    }

    /// `mpol_new` and `mpol_set_nodemask` with node 0 the only one: the
    /// default and local policies take no nodes, the others need node 0 or a
    /// relative mask.
    #[test]
    fn a_policy_needs_node_0_unless_it_takes_no_nodes() {
        let none = Nodes::default();
        let both = Nodes {
            node0: true,
            other: true,
        };
        let r = MPOL_F_RELATIVE_NODES;
        let s = MPOL_F_STATIC_NODES;
        let cases = [
            (MPOL_DEFAULT_MODE, 0, none, true),
            (MPOL_DEFAULT_MODE, 0, NODE0_ONLY, false),
            (MPOL_PREFERRED, 0, none, true),
            (MPOL_PREFERRED, s, none, false),
            (MPOL_PREFERRED, r, none, false),
            (MPOL_PREFERRED, 0, NODE0_ONLY, true),
            (MPOL_PREFERRED, 0, OTHER_ONLY, false),
            (MPOL_PREFERRED, r, OTHER_ONLY, true),
            (MPOL_LOCAL, 0, none, true),
            (MPOL_LOCAL, 0, NODE0_ONLY, false),
            (MPOL_LOCAL, s, none, false),
            (MPOL_BIND, 0, none, false),
            (MPOL_BIND, r, none, false),
            (MPOL_BIND, 0, NODE0_ONLY, true),
            (MPOL_BIND, 0, both, true),
            (MPOL_BIND, 0, OTHER_ONLY, false),
            (MPOL_BIND, s, OTHER_ONLY, false),
            (MPOL_BIND, r, OTHER_ONLY, true),
            (MPOL_INTERLEAVE, 0, none, false),
            (MPOL_INTERLEAVE, 0, NODE0_ONLY, true),
            (MPOL_PREFERRED_MANY, 0, OTHER_ONLY, false),
            (MPOL_PREFERRED_MANY, 0, both, true),
        ];
        for (mode, flags, nodes, ok) in cases {
            let want = if ok { Ok(()) } else { Err(LxError::EINVAL) };
            assert_eq!(
                check_policy(mode, flags, nodes),
                want,
                "mode {} flags {:#x} {:?}",
                mode,
                flags,
                nodes
            );
        }
    }

    /// `do_mbind` before the policy: its flags, CAP_SYS_NICE for
    /// MPOL_MF_MOVE_ALL, a page-aligned start, and a length rounded up to
    /// pages, wrapping as in C.
    #[test]
    fn mbind_takes_a_page_aligned_range() {
        let yes = || true;
        let no = || false;
        assert_eq!(
            mbind_range(0x1000, 1, MPOL_MF_VALID, yes),
            Ok(Some(PAGE_SIZE))
        );
        assert_eq!(mbind_range(0x1000, 1, 1 << 3, yes), Err(LxError::EINVAL));
        assert_eq!(
            mbind_range(0x1000, 1, MPOL_MF_MOVE_ALL, no),
            Err(LxError::EPERM)
        );
        assert_eq!(
            mbind_range(0x1000, 1, MPOL_MF_MOVE, no),
            Ok(Some(PAGE_SIZE))
        );
        // The capability is asked before the alignment.
        assert_eq!(
            mbind_range(0x1001, 1, MPOL_MF_MOVE_ALL, no),
            Err(LxError::EPERM)
        );
        assert_eq!(mbind_range(0x1001, 1, 0, yes), Err(LxError::EINVAL));
        assert_eq!(mbind_range(0x1000, 0, 0, yes), Ok(None));
        assert_eq!(
            mbind_range(0x1000, PAGE_SIZE + 1, 0, yes),
            Ok(Some(2 * PAGE_SIZE))
        );
        // PAGE_ALIGN wraps the largest length to nothing.
        assert_eq!(mbind_range(0x1000, usize::MAX, 0, yes), Ok(None));
        let top = usize::MAX - PAGE_SIZE + 1;
        assert_eq!(mbind_range(top, PAGE_SIZE, 0, yes), Err(LxError::EINVAL));
        assert_eq!(
            mbind_range(top - PAGE_SIZE, PAGE_SIZE, 0, yes),
            Ok(Some(PAGE_SIZE))
        );
    }

    /// The calls as a program makes them: a policy bound to node 0 is taken,
    /// one bound to a node this kernel lacks is not, and `mbind` wants every
    /// page of its range mapped.
    #[test]
    fn the_syscalls_take_a_policy_linux_would() {
        use crate::on_a_thread::on_a_thread;
        use alloc::boxed::Box;
        use linux_object::fs::FileDesc;
        use linux_object::process::LinuxProcess;
        use rcore_fs_ramfs::RamFS;
        use zircon_object::task::{Process, ROOT_JOB};

        const RW: usize = 0x3;
        const ANON_PRIVATE: usize = 0x22;
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            45_402,
            "numa-set",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let got = on_a_thread(&proc, |sys| {
            Box::pin(async move {
                let node0 = [1u64];
                let node1 = [2u64];
                let at = |mask: &[u64]| mask.as_ptr() as usize;
                let set = (
                    sys.sys_set_mempolicy(MPOL_BIND as usize, at(&node0).into(), 2),
                    sys.sys_set_mempolicy(MPOL_BIND as usize, at(&node1).into(), 3),
                    sys.sys_set_mempolicy(MPOL_DEFAULT as usize, 0.into(), 0),
                );
                let none = FileDesc::from(-1i32 as usize);
                let start = sys
                    .sys_mmap(0, 3 * PAGE_SIZE, RW, ANON_PRIVATE, none, 0)
                    .await
                    .unwrap();
                let bind = |start: usize, len: usize, mask: &[u64]| {
                    sys.sys_mbind(start, len, MPOL_BIND as usize, at(mask).into(), 2, 0)
                };
                let whole = bind(start, 3 * PAGE_SIZE, &node0);
                let elsewhere = bind(start, 3 * PAGE_SIZE, &node1);
                sys.sys_munmap(start + PAGE_SIZE, PAGE_SIZE).unwrap();
                let mbind = (
                    whole,
                    elsewhere,
                    bind(start, 3 * PAGE_SIZE, &node0),
                    bind(start, PAGE_SIZE, &node0),
                    bind(start + 2 * PAGE_SIZE, 1, &node0),
                    bind(start + PAGE_SIZE, PAGE_SIZE, &node0),
                );
                (set, mbind)
            })
        });
        let (set, mbind) = got;
        assert_eq!(set, (Ok(0), Err(LxError::EINVAL), Ok(0)));
        assert_eq!(
            mbind,
            (
                Ok(0),
                Err(LxError::EINVAL),
                Err(LxError::EFAULT),
                Ok(0),
                Ok(0),
                Err(LxError::EFAULT)
            ),
            "whole, on node 1, over the hole, before it, after it, the hole"
        );
    }
}
