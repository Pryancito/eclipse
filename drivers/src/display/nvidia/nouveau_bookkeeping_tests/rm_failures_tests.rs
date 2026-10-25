//! The arms the fake RM reaches only when it fails as a CALL, or when the
//! chip is one the tables do not know: a `gem_alloc` or `gem_map_cpu` that
//! returns no status at all, a `vm_bind_map` that does, a `class_free` of
//! an object the RM has already lost, a `ctx0_reset` it refuses, the GR
//! probe that answers, the `Unknown` architecture through GRAPH_UNITS,
//! the device name and SCLASS, the
//! empty EXEC that rides the ring, and the two fault decoders. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own because
//! the parent is at its size ceiling (`xtask tamano`).

use super::*;
use crate::utils::host_hooks::take_dmesg;

/// What dmesg got since the last look.
fn lines() -> Vec<alloc::string::String> {
    take_dmesg().into_iter().map(|(_, l)| l).collect()
}

fn one_line_with(lines: &[alloc::string::String], needle: &str) -> alloc::string::String {
    let hits: Vec<_> = lines.iter().filter(|l| l.contains(needle)).collect();
    assert_eq!(hits.len(), 1, "{needle:?} in {lines:?}");
    hits[0].clone()
}

/// NV_PFAULT_FAULT_TYPE and NV_PFAULT_ACCESS_TYPE as Turing's
/// dev_fault.ref.txt lists them; a code outside the table is a `?`, not
/// a neighbour's name.
#[test]
fn the_fault_decoders_name_the_turing_codes_and_shrug_at_the_rest() {
    for (code, name) in [
        (0, "PDE"),
        (1, "PDE_SIZE"),
        (2, "PTE(unmapped)"),
        (3, "VA_LIMIT"),
        (4, "UNBOUND_INST"),
        (5, "PRIV"),
        (6, "RO"),
        (7, "WO"),
        (0xa, "BAD_APERTURE"),
    ] {
        assert_eq!(fault_reason_name(code), name, "reason {code:#x}");
    }
    for code in [8, 9, 0xb, 0x1f, u32::MAX] {
        assert_eq!(fault_reason_name(code), "?", "reason {code:#x}");
    }
    for (code, name) in [
        (0, "READ"),
        (1, "WRITE"),
        (2, "ATOMIC"),
        (3, "PREFETCH"),
        (8, "PHYS_READ"),
        (9, "PHYS_WRITE"),
        (0xa, "PHYS_ATOMIC"),
    ] {
        assert_eq!(fault_access_name(code), name, "access {code:#x}");
    }
    for code in [4, 7, 0xb, 0xf, u32::MAX] {
        assert_eq!(fault_access_name(code), "?", "access {code:#x}");
    }
}

/// The scheme hands out the mode it was built with, says its scanout is
/// write-combining and advertises every bulk 2D primitive: the console
/// picks the accelerated path off these three answers.
#[test]
fn the_display_scheme_reports_its_mode_and_advertises_the_bulk_primitives() {
    use crate::scheme::display::AccelCaps;
    use crate::scheme::DisplayScheme;
    let _g = LOCK.lock();
    let gpu = gpu();
    let info = DisplayScheme::info(&gpu);
    assert_eq!(
        (info.width, info.height, info.pitch, info.fb_size),
        (
            gpu.info.width,
            gpu.info.height,
            gpu.info.pitch,
            gpu.info.fb_size
        )
    );
    assert!(gpu.fb_write_combining());
    assert_eq!(
        gpu.accel_caps(),
        AccelCaps {
            fill: true,
            copy: true,
            blit: true,
        }
    );
}

/// An op code the uAPI does not define is EINVAL before the RM; a MAP the
/// RM call itself refuses (no status, the wrapper's `Err`) is EIO, named
/// in dmesg with its NV_STATUS, with nothing recorded for it -- and the
/// next bind of the same object goes through: a failed call leaves no
/// state behind.
#[test]
fn an_undefined_vm_bind_op_is_einval_and_a_map_the_rm_call_refuses_is_eio_and_leaves_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let before = FAKE_RM.lock().calls.len();
    let mut odd = map(h, PUSH_VA, 65536);
    odd.op = 0x7;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [odd]), Err(nv::EINVAL));
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "an op nobody defined costs no RM call"
    );
    FAKE_RM.lock().fail_bind = true;
    let _ = lines();
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(h, PUSH_VA, 65536)]),
        Err(nv::EIO)
    );
    FAKE_RM.lock().fail_bind = false;
    let calls = rm_calls_since(before);
    assert_eq!(
        calls.iter().filter(|c| **c == "vm_bind_map").count(),
        1,
        "asked once: {calls:?}"
    );
    assert!(
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .all(|m| m.va != PUSH_VA),
        "a map the RM never made is not in the table"
    );
    let said = one_line_with(&lines(), "VM_BIND MAP failed");
    assert!(
        said.contains(&alloc::format!(
            "handle={} VA={:#x} range={:#x}",
            h,
            PUSH_VA,
            65536
        )) && said.contains("NV_STATUS=0x56"),
        "{}",
        said
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(h, PUSH_VA, 65536)]),
        Ok(0),
        "the next try is not poisoned by the failed one"
    );
    assert!(gpu
        .nouveau_vm_mappings
        .lock()
        .iter()
        .any(|m| m.va == PUSH_VA));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// Ops are applied in order and the first one the RM refuses stops the
/// bind: the ops before it STAY applied (there is no unwind, as the
/// comment in the loop says), the ops after it never run.
#[test]
fn a_bind_the_rm_refuses_part_way_keeps_the_ops_before_it_and_runs_none_after() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let h2 = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA2: u64 = PUSH_VA + 0x10_0000;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
    FAKE_RM.lock().refuse_map = true;
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [
                unmap(PUSH_VA, 65536),
                map(h2, VA2, 65536),
                map(h, PUSH_VA, 65536)
            ]
        ),
        Err(nv::EIO)
    );
    FAKE_RM.lock().refuse_map = false;
    let maps = gpu.nouveau_vm_mappings.lock();
    assert!(
        maps.iter().all(|m| m.va != PUSH_VA),
        "op[0], the unmap, stays applied: nothing is put back"
    );
    assert!(maps.iter().all(|m| m.va != VA2), "op[1] was refused");
    drop(maps);
    assert_eq!(
        FAKE_RM.lock().maps.len(),
        0,
        "and op[2] never reached the RM"
    );
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// `gem_alloc` failing as a CALL (the wrapper's `Err`, no `alloc_status`
/// to read) is ENOMEM like the refusal inside an `Ok`: named in dmesg
/// with the aperture asked for and the NV_STATUS, nothing in the table,
/// nothing counted, and the id burnt on it is the next object's loss
/// only (the slice holds 32Mi of them).
#[test]
fn a_gem_alloc_that_fails_as_a_call_is_enomem_and_counts_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let live = LiveBytes::hold();
    FAKE_RM.lock().fail_gem_alloc_call = true;
    let _ = lines();
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A).map(|i| i.handle),
        Err(nv::ENOMEM)
    );
    assert_eq!(
        gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_VRAM, A).map(|i| i.handle),
        Err(nv::ENOMEM)
    );
    FAKE_RM.lock().fail_gem_alloc_call = false;
    assert_eq!(rm_calls_since(before), ["gem_alloc", "gem_alloc"]);
    assert_eq!(live.delta(), 0, "nothing counted");
    assert!(
        gpu.nouveau_gem.lock().iter().all(|o| o.owner_pid != A),
        "nothing in the table"
    );
    let said = lines();
    one_line_with(&said, "gem_alloc (sysmem/GART) failed, NV_STATUS=0x40");
    one_line_with(&said, "gem_alloc (vidmem/VRAM) failed, NV_STATUS=0x40");
    assert!(
        gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A).is_ok(),
        "the next allocation is not poisoned"
    );
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// `gem_map_cpu` failing as a CALL leaves the object made, in the table
/// and counted, with no CPU address: usable for `VM_BIND`/`EXEC`, not
/// for `mmap` -- the same shape as a lookup the RM refuses inside an
/// `Ok`, and said in dmesg with the handle and the NV_STATUS.
#[test]
fn a_gem_whose_cpu_lookup_fails_as_a_call_is_made_but_has_no_cpu_address() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    FAKE_RM.lock().fail_map_cpu_call = true;
    let _ = lines();
    let before = FAKE_RM.lock().calls.len();
    let info = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A).unwrap();
    FAKE_RM.lock().fail_map_cpu_call = false;
    assert_eq!(rm_calls_since(before), ["gem_alloc", "gem_map_cpu"]);
    {
        let gem = gpu.nouveau_gem.lock();
        let o = gem
            .iter()
            .find(|o| o.handle == info.handle)
            .expect("the object is made");
        assert_eq!(o.phys_addr, None, "no CPU address");
        assert_eq!((o.owner_pid, o.size), (A, 65536));
    }
    let said = one_line_with(&lines(), "gem_map_cpu failed, NV_STATUS=0x40");
    assert!(
        said.contains(&alloc::format!("GEM_NEW handle={}:", info.handle))
            && said.contains("not CPU-mmap-able"),
        "{}",
        said
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(info.handle, PUSH_VA, 65536)]),
        Ok(0),
        "still a GPU object"
    );
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A chip the id table does not know (and whose BOOT_0 says nothing)
/// reports a full generic die to GRAPH_UNITS, names itself `NV` to
/// `NV_DEVICE_V0_INFO`, and refuses to list engine classes -- NVK then
/// skips the GPU, the honest outcome for hardware the driver cannot
/// place, rather than encoding another chip's methods for it.
#[test]
fn an_unknown_architecture_reports_a_generic_die_a_bare_chip_tag_and_no_classes() {
    let _g = LOCK.lock();
    let mut gpu = gpu();
    gpu.architecture = NvidiaArchitecture::Unknown;
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(8 | (64 << 8)),
        "a generic die, never zero (NVK divides by it)"
    );
    assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0));
    let mut r = device_info(nv::NV_DEVICE_V0_INFO);
    assert_eq!(r.send(&gpu, A), Ok(0));
    let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
    assert_eq!(cstr(&info.chip), "NV");
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    assert_eq!(
        sclass(0, 0xff, 16, 16).send(&gpu, A),
        Err(nv::EINVAL),
        "no classes to guess"
    );
    gpu.nouveau_release_process(A);
}

/// With the RM attached, GRAPH_UNITS is the GR probe's answer when it
/// gives one (`num_gpc` in the low byte, `total_tpc` above), and the
/// architecture's full die when the probe is refused or empty -- down
/// to the last table entry.
#[test]
fn graph_units_are_the_rm_probes_answer_when_it_has_one_and_the_full_die_otherwise() {
    let _g = LOCK.lock();
    let mut gpu = gpu_rm();
    FAKE_RM.lock().gr_probe = Some((5, 30));
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(5 | (30 << 8)),
        "the floorswept truth"
    );
    FAKE_RM.lock().gr_probe = Some((0, 30));
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(6 | (36 << 8)),
        "a probe with no GPCs in it is no answer: TU102"
    );
    FAKE_RM.lock().gr_probe = None;
    gpu.architecture = NvidiaArchitecture::Blackwell;
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(12 | (96 << 8)),
        "GB202"
    );
    gpu.architecture = NvidiaArchitecture::Unknown;
    assert_eq!(
        getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
        Ok(8 | (64 << 8))
    );
}

/// An NVIF DEL of a class object whose RM handle is already gone (a
/// `class_free` the RM answers OBJECT_NOT_FOUND) still removes it here
/// and answers 0, as `nvif_object_dtor` cannot fail: the refusal is one
/// line in dmesg with the handle and the status, not an errno Mesa
/// would turn into a device loss on context destroy.
#[test]
fn an_nvif_del_whose_rm_object_is_already_gone_still_removes_it_here() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = channel_alloc(&gpu, A).unwrap().channel as u64;
    assert_eq!(subchan_new(ch, 0xc597, 0x1000).send(&gpu, A), Ok(0));
    let (h_object, ctx) = {
        let mut f = FAKE_RM.lock();
        let c = *f.classes.last().expect("the RM built the class object");
        f.classes.clear();
        (c.0, c.1)
    };
    let _ = lines();
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0x1000, HDR).send(&gpu, A),
        Ok(0)
    );
    assert_eq!(rm_calls_since(before), ["class_free"]);
    assert_eq!(
        nv::class_object_remove(0x1000, A),
        None,
        "gone here, whatever the RM said"
    );
    let said = one_line_with(&lines(), "NVIF DEL: class_free(");
    assert!(
        said.contains(&alloc::format!(
            "class_free({:#010x}) -> NV_STATUS=0x57",
            h_object
        )),
        "{}",
        said
    );
    assert_eq!(
        ctx_of(&gpu, A).map(|c| c.0),
        Some(ctx),
        "freed in its own context"
    );
    // The fake counts a free of an object it no longer holds as a bad
    // call; that is the call under test, so it is not a bad one here.
    assert_eq!(FAKE_RM.lock().bad, 1);
    FAKE_RM.lock().bad = 0;
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// A `ctx0_reset` the RM refuses at the compositor's exit is said in
/// dmesg with the status, and the exit goes on: the role is released
/// all the same (a respawn is a compositor again), and the RM's channel
/// stands, which is what the warning is for.
#[test]
fn a_ctx0_reset_the_rm_refuses_is_said_and_the_exit_still_frees_the_role() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_ladder();
    let _ch = client_with_pushbuf(&gpu, COMP);
    assert_eq!(ctx0_owner(&gpu), COMP);
    assert!(FAKE_RM.lock().chan_built, "step 17 built the channel");
    FAKE_RM.lock().fail_ctx0_reset = true;
    let _ = lines();
    gpu.nouveau_release_process(COMP);
    FAKE_RM.lock().fail_ctx0_reset = false;
    assert_eq!(ctx0_owner(&gpu), 0, "the role is free anyway");
    assert_eq!(ctx0_resets(), 0, "the RM kept its channel");
    assert!(FAKE_RM.lock().chan_built);
    let said = one_line_with(&lines(), "ctx0 reset:");
    assert!(
        said.contains(&alloc::format!("pid={} failed, NV_STATUS=0x40", COMP)),
        "{}",
        said
    );
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// An empty EXEC whose wait is a fence this ring has not landed yet rides
/// the ring like a submission would: a GPU ACQUIRE of that fence and a
/// fence of its own, the sigs attached to it, no CPU wait at all -- so
/// `vkQueueWaitIdle`'s probe behind a frame costs the CPU nothing and the
/// sigs land when the frame does, not before.
#[test]
fn an_empty_exec_waiting_on_this_rings_pending_fence_rides_the_ring_and_signals_behind_it() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let ctx = ctx_of(&gpu, A).unwrap().0;
    let frame = syncobj::create(false);
    let idle = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(frame)]),
        Ok(0)
    );
    let waited = nv::EXEC_WAIT_US.load(Ordering::Relaxed);
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(exec(&gpu, A, ch, &[], &[sync(frame)], &[sync(idle)]), Ok(0));
    assert_eq!(
        nv::EXEC_WAIT_US.load(Ordering::Relaxed),
        waited,
        "no CPU wait: the acquire went onto the ring"
    );
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "direct submit: the RM is not asked"
    );
    assert_eq!(syncobj::query(frame), Some(0), "the ring has not run");
    assert_eq!(syncobj::query(idle), Some(0), "so nothing is signaled yet");
    let fetched = run_gpu(ctx);
    assert_eq!(fetched.len(), 4, "push, fence; acquire, fence: {fetched:?}");
    assert_eq!(syncobj::query(frame), Some(1));
    assert_eq!(syncobj::query(idle), Some(1), "behind the frame");
    assert!(syncobj::destroy(frame));
    assert!(syncobj::destroy(idle));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// The RM's own submit builds the GPFIFO entry in C and cannot mark it
/// SYNC_WAIT, so a NO_PREFETCH push through that path goes in
/// prefetchable: said once per boot, the push still submitted.
#[test]
fn a_no_prefetch_push_through_the_rm_path_is_submitted_and_said_once() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let out = syncobj::create(false);
    let _ = lines();
    let before = FAKE_RM.lock().submits.len();
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push_no_prefetch(PUSH_VA, 16)],
            &[],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert_eq!(
        FAKE_RM.lock().submits.len(),
        before + 1,
        "submitted all the same"
    );
    one_line_with(
        &lines(),
        "NO_PREFETCH push submitted through the RM, which cannot mark the entry SYNC_WAIT",
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[push_no_prefetch(PUSH_VA, 16)],
            &[],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert!(
        lines()
            .iter()
            .all(|l| !l.contains("NO_PREFETCH push submitted")),
        "reported once"
    );
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}
