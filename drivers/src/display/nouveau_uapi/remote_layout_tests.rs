use super::*;
use crate::scheme::drm_remote::ioctl::{nested, Array};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

/// The ioctl number of `nr` with a struct of `size` bytes, read and written.
fn iowr(nr: u32, size: usize) -> u32 {
    0xc000_0000 | ((size as u32) << 16) | (0x64 << 8) | nr
}

fn at(count_at: usize, ptr_at: usize, elem: usize) -> (usize, usize, usize) {
    (count_at, ptr_at, elem)
}

fn of(a: &[Array]) -> Vec<(usize, usize, usize)> {
    a.iter().map(|a| (a.count_at, a.ptr_at, a.elem)).collect()
}

#[test]
fn vm_bind_carries_its_ops_waits_and_signals() {
    type B = DrmNouveauVmBind;
    let cmd = iowr(DRM_COMMAND_BASE + DRM_NOUVEAU_VM_BIND, size_of::<B>());
    assert_eq!(
        of(nested(cmd)),
        vec![
            at(
                offset_of!(B, op_count),
                offset_of!(B, op_ptr),
                size_of::<DrmNouveauVmBindOp>()
            ),
            at(
                offset_of!(B, wait_count),
                offset_of!(B, wait_ptr),
                size_of::<DrmNouveauSync>()
            ),
            at(
                offset_of!(B, sig_count),
                offset_of!(B, sig_ptr),
                size_of::<DrmNouveauSync>()
            ),
        ]
    );
}

#[test]
fn exec_carries_its_pushes_waits_and_signals() {
    type E = DrmNouveauExec;
    let cmd = iowr(DRM_COMMAND_BASE + DRM_NOUVEAU_EXEC, size_of::<E>());
    assert_eq!(
        of(nested(cmd)),
        vec![
            at(
                offset_of!(E, push_count),
                offset_of!(E, push_ptr),
                size_of::<DrmNouveauExecPush>()
            ),
            at(
                offset_of!(E, wait_count),
                offset_of!(E, wait_ptr),
                size_of::<DrmNouveauSync>()
            ),
            at(
                offset_of!(E, sig_count),
                offset_of!(E, sig_ptr),
                size_of::<DrmNouveauSync>()
            ),
        ]
    );
}

#[test]
fn pushbuf_carries_its_buffers_and_pushes() {
    type P = DrmNouveauGemPushbuf;
    let cmd = iowr(DRM_COMMAND_BASE + DRM_NOUVEAU_GEM_PUSHBUF, size_of::<P>());
    assert_eq!(
        of(nested(cmd)),
        vec![
            at(
                offset_of!(P, nr_buffers),
                offset_of!(P, buffers),
                size_of::<DrmNouveauGemPushbufBo>()
            ),
            at(
                offset_of!(P, nr_push),
                offset_of!(P, push),
                size_of::<DrmNouveauGemPushbufPush>()
            ),
        ]
    );
}

#[test]
fn every_other_nouveau_ioctl_carries_its_struct_alone() {
    for nr in [
        DRM_NOUVEAU_GETPARAM,
        DRM_NOUVEAU_CHANNEL_ALLOC,
        DRM_NOUVEAU_NVIF,
        DRM_NOUVEAU_VM_INIT,
        DRM_NOUVEAU_GEM_NEW,
        DRM_NOUVEAU_GEM_CPU_PREP,
        DRM_NOUVEAU_GEM_INFO,
    ] {
        assert!(
            nested(iowr(DRM_COMMAND_BASE + nr, 64)).is_empty(),
            "nr {:#x}",
            nr
        );
    }
}
