extern crate std;

use super::*;
use std::cell::{Cell, RefCell};
use std::sync::{Mutex, MutexGuard};
use std::vec::Vec;

/// `IN_CALL`, `CALLER_GPUS` and `USER_RANGE_OK` are process-globals and
/// `cargo test` runs in parallel: every test that reads or sets them holds
/// this, so "outside a call" really is outside every test's call.
pub(super) fn turnstile() -> MutexGuard<'static, ()> {
    static T: Mutex<()> = Mutex::new(());
    T.lock().unwrap_or_else(|e| e.into_inner())
}

std::thread_local! {
    /// What the hosted stand-in for the C answers this thread's next
    /// RM_MAP_MEMORY with, and the status of every mapping it was told to
    /// unmap.
    static MAP_ANSWER: Cell<Option<(NV_STATUS, RawMmapContext)>> = const { Cell::new(None) };
    static UNMAPPED: RefCell<Vec<NV_STATUS>> = const { RefCell::new(Vec::new()) };
    /// What the stand-in runs for this thread's next escape, inside the call.
    static BODY: RefCell<Option<std::boxed::Box<dyn FnMut(NvU32) -> NV_STATUS>>> =
        const { RefCell::new(None) };
}

/// The stand-in's escape, when a test set a body for it.
pub(super) fn stand_in_body(cmd: NvU32) -> Option<NV_STATUS> {
    // Taken first: the body may well make an escape of its own.
    let mut body = BODY.with(|b| b.borrow_mut().take())?;
    Some(body(cmd))
}

/// Have the stand-in run `body` for this thread's next escape.
pub(super) fn answer_with(body: impl FnMut(NvU32) -> NV_STATUS + 'static) {
    BODY.with(|b| *b.borrow_mut() = Some(std::boxed::Box::new(body)));
}

/// The stand-in's RM_MAP_MEMORY, when a test set an answer.
pub(super) fn stand_in_map(mmap: *mut RawMmapContext) -> Option<NV_STATUS> {
    let (status, record) = MAP_ANSWER.with(|a| a.take())?;
    if !mmap.is_null() {
        // SAFETY: `rm_map_memory` passes its own record.
        unsafe { *mmap = record };
    }
    Some(status)
}

pub(super) fn stand_in_unmapped(why: NV_STATUS) {
    UNMAPPED.with(|u| u.borrow_mut().push(why));
}

fn answer_map(status: NV_STATUS, record: RawMmapContext) {
    MAP_ANSWER.with(|a| a.set(Some((status, record))));
}

fn unmapped() -> Vec<NV_STATUS> {
    UNMAPPED.with(|u| u.take())
}

fn version_of(s: &[u8]) -> [u8; VERSION_STRING_LENGTH] {
    let mut v = [0u8; VERSION_STRING_LENGTH];
    v[..s.len()].copy_from_slice(s);
    v
}

const RM: &[u8] = b"580.178.04";

#[test]
fn the_same_version_passes_and_is_left_alone() {
    let mut v = version_of(RM);
    assert!(version_check(0, &mut v, RM));
    assert_eq!(v, version_of(RM));
}

/// libnvidia-ml from another release must be refused: a user-mode driver
/// talking to an RM of another version is exactly what `CHECK_VERSION_STR`
/// exists to stop, and nvidia.ko's comparison is byte for byte.
#[test]
fn another_version_is_refused() {
    let mut v = version_of(b"580.178.05");
    assert!(!version_check(0, &mut v, RM));
    let mut v = version_of(b"580.178.0");
    assert!(!version_check(0, &mut v, RM), "a prefix is not a match");
    let mut v = version_of(b"580.178.040");
    assert!(!version_check(0, &mut v, RM), "nor is a longer string");
}

/// The relaxed form compares up to the first `.`, and no further.
#[test]
fn relaxed_compares_the_major_only() {
    let mut v = version_of(b"580.95.05");
    assert!(version_check(VERSION_CMD_RELAXED, &mut v, RM));
    let mut v = version_of(b"575.178.04");
    assert!(!version_check(VERSION_CMD_RELAXED, &mut v, RM));
    let mut v = version_of(b"580.95.05");
    assert!(
        !version_check(0, &mut v, RM),
        "the strict form must not inherit the relaxed answer"
    );
}

#[test]
fn a_query_answers_with_the_rm_string() {
    let mut v = version_of(b"whatever");
    assert!(version_check(VERSION_CMD_QUERY, &mut v, RM));
    assert_eq!(v, version_of(RM));
}

/// A client string with no NUL in 64 bytes is terminated at the last one
/// before anything prints it, as `RmPerformVersionCheck` does.
#[test]
fn an_unterminated_client_string_is_terminated_on_mismatch() {
    let mut v = [b'9'; VERSION_STRING_LENGTH];
    assert!(!version_check(0, &mut v, RM));
    assert_eq!(v[VERSION_STRING_LENGTH - 1], 0);
    assert!(v[..VERSION_STRING_LENGTH - 1].iter().all(|&b| b == b'9'));
}

/// With no RM (an empty string) nothing but an empty client string matches;
/// a hosted kernel never says yes to a real driver.
#[test]
fn no_rm_matches_nothing_real() {
    let mut v = version_of(RM);
    assert!(!version_check(0, &mut v, b""));
}

#[test]
fn copy_version_always_leaves_room_for_the_nul() {
    let mut v = [0xffu8; VERSION_STRING_LENGTH];
    copy_version(&mut v, &[b'7'; 100]);
    assert_eq!(v[VERSION_STRING_LENGTH - 1], 0);
    assert!(v[..VERSION_STRING_LENGTH - 1].iter().all(|&b| b == b'7'));
    let mut v = [0xffu8; VERSION_STRING_LENGTH];
    copy_version(&mut v, RM);
    assert_eq!(&v[..RM.len()], RM);
    assert_eq!(v[RM.len()], 0);
}

#[test]
fn the_mask_names_instances_bit_by_bit() {
    assert!(instance_in_mask(0, 0b1));
    assert!(!instance_in_mask(1, 0b1));
    assert!(instance_in_mask(1, 0b10));
    assert!(instance_in_mask(31, 1 << 31));
    // Past the mask: never accessible, never a wrapped bit 0.
    assert!(!instance_in_mask(32, u32::MAX));
    assert!(!instance_in_mask(u32::MAX, u32::MAX));
}

fn accept_all(_addr: usize, _len: usize) -> bool {
    true
}

fn refuse_all(_addr: usize, _len: usize) -> bool {
    false
}

/// Outside a front-end call nothing in this port hands the RM a user
/// pointer, so the copies keep answering what they always did.
#[test]
fn user_copies_are_refused_outside_a_call() {
    let _t = turnstile();
    set_user_range_validator(accept_all);
    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    assert_eq!(
        copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
    assert_eq!(
        copy_to_user(dst.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
    assert_eq!(dst, [0; 4]);
}

/// A caller for a scope that is only about the window, not about who.
fn a_caller(gpu_mask: u32, admin: bool) -> Caller {
    Caller {
        os_info: 0x1000,
        ctl: true,
        admin,
        gpu_mask,
        pid: 0,
        name: [0; CALLER_NAME_LEN],
    }
}

#[test]
fn user_copies_ask_the_kernel_inside_a_call() {
    let _t = turnstile();
    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    {
        let _scope = CallScope::enter(&a_caller(0, false));
        set_user_range_validator(refuse_all);
        assert_eq!(
            copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4),
            NV_ERR_INVALID_ADDRESS
        );
        assert_eq!(
            copy_to_user(dst.as_mut_ptr(), src.as_ptr(), 4),
            NV_ERR_INVALID_ADDRESS
        );
        assert_eq!(dst, [0; 4], "a refused range copies nothing");

        set_user_range_validator(accept_all);
        assert_eq!(copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4), NV_OK);
        assert_eq!(dst, src);
        let mut back = [0u8; 4];
        assert_eq!(copy_to_user(back.as_mut_ptr(), dst.as_ptr(), 4), NV_OK);
        assert_eq!(back, src);
    }
    // And the scope's end closes the window again.
    let mut after = [0u8; 4];
    assert_eq!(
        copy_from_user(after.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
}

#[test]
fn the_call_scope_sets_and_clears_the_caller() {
    let _t = turnstile();
    assert!(!IN_CALL.load(Ordering::SeqCst));
    {
        let _scope = CallScope::enter(&a_caller(0b101, true));
        assert!(IN_CALL.load(Ordering::SeqCst));
        assert_eq!(CALLER_GPUS.load(Ordering::SeqCst), 0b101);
        assert!(is_administrator());
    }
    assert!(!IN_CALL.load(Ordering::SeqCst));
    assert_eq!(CALLER_GPUS.load(Ordering::SeqCst), 0);
    assert!(
        !is_administrator(),
        "nobody is an administrator outside a call"
    );
    {
        let _scope = CallScope::enter(&a_caller(0, false));
        assert!(!is_administrator());
    }
}

/// The pid is the whole of the RM's per-process accounting: `client.c`
/// files every client it makes under `osGetCurrentProcess()`, and
/// `NV2080_CTRL_CMD_GPU_GET_PIDS` -- `nvidia-smi`'s "Processes" table --
/// answers with nothing else. It must be the calling process's for the
/// length of the call and 0 outside, which is what a close is.
#[test]
fn the_call_scope_names_the_calling_process_and_only_inside_the_call() {
    let _t = turnstile();
    let mut caller = a_caller(0, false);
    caller.pid = 1234;
    caller.name = Caller::name_of(b"ollama");
    assert_eq!(crate::os_interface::os_get_current_process(), 0);
    {
        let _scope = CallScope::enter(&caller);
        assert_eq!(crate::os_interface::os_get_current_process(), 1234);
        assert_eq!(&current_name()[..6], b"ollama");
        let mut buf = [0xffu8; 32];
        crate::os_boundary::osGetCurrentProcessName(buf.as_mut_ptr() as *mut c_char, 32);
        assert_eq!(&buf[..7], b"ollama\0");
    }
    assert_eq!(crate::os_interface::os_get_current_process(), 0);
    assert_eq!(current_name(), [0; CALLER_NAME_LEN]);
    // Outside a call the RM is asking about this kernel, and gets its name.
    let mut buf = [0xffu8; 32];
    crate::os_boundary::osGetCurrentProcessName(buf.as_mut_ptr() as *mut c_char, 32);
    assert_eq!(&buf[..15], b"eclipse-kernel\0");
    // A close names no process either.
    {
        let _scope = CallScope::enter_kernel();
        assert_eq!(crate::os_interface::os_get_current_process(), 0);
        let mut buf = [0xffu8; 32];
        crate::os_boundary::osGetCurrentProcessName(buf.as_mut_ptr() as *mut c_char, 32);
        assert_eq!(&buf[..15], b"eclipse-kernel\0");
    }
}

/// A name longer than `current->comm` is cut, and a cut name still ends in
/// the NUL the RM's `%s` needs -- `kernel_rc.c` prints `RmClient::name` out
/// of a buffer nothing zeroed.
#[test]
fn a_caller_s_name_is_cut_to_comm_s_length_and_always_terminated() {
    let name = Caller::name_of(b"un-nombre-larguisimo-de-proceso");
    assert_eq!(name.len(), CALLER_NAME_LEN);
    assert_eq!(&name[..CALLER_NAME_LEN - 1], b"un-nombre-largu");
    assert_eq!(name[CALLER_NAME_LEN - 1], 0, "siempre queda el terminador");
    assert_eq!(Caller::name_of(b""), [0; CALLER_NAME_LEN]);
    // Exactly the room there is, terminator included.
    assert_eq!(&Caller::name_of(b"quincecaracter")[..14], b"quincecaracter");
}

/// The RM's buffer is never overrun and never left unterminated, whatever
/// room it has, and a one-byte buffer gets just the NUL.
#[test]
fn a_callers_name_never_overruns_the_rms_buffer() {
    let _t = turnstile();
    let mut caller = a_caller(0, false);
    caller.name = Caller::name_of(b"ollama");
    let _scope = CallScope::enter(&caller);
    for room in 1..10usize {
        let mut buf = [0xffu8; 16];
        crate::os_interface::write_caller_name(buf.as_mut_ptr() as *mut c_char, room as NvU32);
        // The name, cut to the room there is, and then the terminator.
        let n = b"ollama".len().min(room - 1);
        assert_eq!(&buf[..n], &b"ollama"[..n], "copiado con room={room}");
        assert_eq!(buf[n], 0, "terminado con room={room}");
        assert!(
            buf[n + 1..].iter().all(|&b| b == 0xff),
            "nada escrito mas alla del terminador con room={room}"
        );
    }
    // A buffer of nothing is left alone, as `write_process_name` does.
    let mut buf = [0xffu8; 4];
    crate::os_interface::write_caller_name(buf.as_mut_ptr() as *mut c_char, 0);
    assert_eq!(buf, [0xff; 4]);
    crate::os_interface::write_caller_name(core::ptr::null_mut(), 4);
}

/// `AttributedTo` is the DRM half of the same accounting: a GR context is
/// built by the kernel, with no front-end call in flight, so nothing else
/// would name the process it is being built for. It names it for the length
/// of the build and leaves nothing behind, so the next kernel-side RM work
/// is nobody's again.
#[test]
fn the_drm_guard_names_a_process_for_a_kernel_side_call_and_leaves_nothing() {
    let _t = turnstile();
    assert_eq!(crate::os_interface::os_get_current_process(), 0);
    {
        let _who = AttributedTo::enter(4242);
        assert_eq!(crate::os_interface::os_get_current_process(), 4242);
        // It does NOT open the user-copy window: there is no user memory in
        // a call the kernel makes on its own.
        assert!(!IN_CALL.load(Ordering::SeqCst));
        let mut dst = [0u8; 4];
        let src = [1u8, 2, 3, 4];
        set_user_range_validator(accept_all);
        assert_eq!(
            copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4),
            NV_ERR_NOT_SUPPORTED,
            "no user copies outside a front-end call, guard or no guard"
        );
    }
    assert_eq!(crate::os_interface::os_get_current_process(), 0);
}

/// Nested, it RESTORES rather than clears. A guard taken inside a front-end
/// call that then cleared the pid would leave the rest of that call -- and
/// every client it still makes -- filed under nobody, which is the bug #2092
/// fixed. Two guards nest the same way.
#[test]
fn the_drm_guard_gives_the_pid_back_to_whoever_had_it() {
    let _t = turnstile();
    let mut caller = a_caller(0, false);
    caller.pid = 1234;
    let _scope = CallScope::enter(&caller);
    assert_eq!(crate::os_interface::os_get_current_process(), 1234);
    {
        let _who = AttributedTo::enter(7);
        assert_eq!(crate::os_interface::os_get_current_process(), 7);
        {
            let _inner = AttributedTo::enter(8);
            assert_eq!(crate::os_interface::os_get_current_process(), 8);
        }
        assert_eq!(crate::os_interface::os_get_current_process(), 7);
    }
    assert_eq!(
        crate::os_interface::os_get_current_process(),
        1234,
        "the escape still names its own process"
    );
    // A pid of 0 is allowed and means nobody, as outside a call.
    {
        let _who = AttributedTo::enter(0);
        assert_eq!(crate::os_interface::os_get_current_process(), 0);
    }
    assert_eq!(crate::os_interface::os_get_current_process(), 1234);
}

#[test]
fn no_gpu_is_accessible_outside_a_call() {
    let _t = turnstile();
    let mut fake = 0u8;
    assert!(!gpu_accessible(&mut fake as *mut u8 as *mut c_void));
    let _scope = CallScope::enter(&a_caller(u32::MAX, true));
    assert!(
        !gpu_accessible(core::ptr::null_mut()),
        "a null OBJGPU is never accessible"
    );
}

/// Hosted builds have no RM: every escape is refused and no open can be
/// confused for another (a zero token is refused before the RM sees it).
#[test]
fn hosted_escapes_are_refused() {
    let _t = turnstile();
    let mut payload = [0u8; 32];
    let caller = Caller {
        os_info: 0x1000,
        ctl: true,
        admin: false,
        gpu_mask: 1,
        pid: 0,
        name: [0; CALLER_NAME_LEN],
    };
    assert_eq!(rm_escape(0x2A, &mut payload, &caller), NV_ERR_NOT_SUPPORTED);
    let nobody = Caller {
        os_info: 0,
        ..caller
    };
    assert_eq!(
        rm_escape(0x2A, &mut payload, &nobody),
        NV_ERR_INVALID_ARGUMENT
    );
    assert!(rm_version().is_empty());
    assert_eq!(gpu_id(0), u32::MAX);
    assert_eq!(gpu_uuid(0), None);
}

/// `RmFreeUnusedClients`: after the open's clients, the close frees every OS
/// event registered on that open, and none registered on another.
#[test]
fn a_close_frees_the_os_events_registered_on_it() {
    const CLIENT: u32 = 0xc1d0_1001;
    let _t = turnstile();
    assert_eq!(events::alloc_os_event(CLIENT, 0x00e2_0001, 7), NV_OK);
    assert_eq!(events::alloc_os_event(CLIENT, 0x00e2_0002, 8), NV_OK);
    release(0x00e2_0001);
    assert_eq!(
        events::free_os_event(CLIENT, 7),
        events::NV_ERR_INVALID_EVENT
    );
    assert_eq!(events::free_os_event(CLIENT, 8), NV_OK);
}

#[test]
fn a_hosted_build_agrees_with_its_own_layout() {
    assert_eq!(layout_mismatch(), None);
}

/// The record is laid out once in C and once here; the C side's `sizeof` is
/// checked against `LAYOUT` at boot, and this checks ours against both.
#[test]
fn the_mapping_record_has_the_c_layout() {
    assert_eq!(core::mem::size_of::<RawMmapContext>(), 40);
    assert_eq!(
        LAYOUT[LAYOUT.len() - 1],
        (
            "EclipseRmMmapContext",
            core::mem::size_of::<RawMmapContext>()
        )
    );
    let raw = RawMmapContext::default();
    let base = &raw as *const _ as usize;
    assert_eq!(&raw.start as *const _ as usize - base, 24);
    assert_eq!(&raw.size as *const _ as usize - base, 32);
}

fn system(start: u64, size: u64) -> RawMmapContext {
    RawMmapContext {
        valid: 1,
        ctl: 1,
        gpu_instance: u32::MAX,
        memory_type: 0,
        caching: caching::CACHED,
        prot: 3,
        start,
        size,
    }
}

fn aperture(memory_type: u32, caching: u32) -> RawMmapContext {
    RawMmapContext {
        valid: 1,
        ctl: 0,
        gpu_instance: 1,
        memory_type,
        caching,
        prot: 3,
        start: 0xd000_0000,
        size: 0x10000,
    }
}

#[test]
fn a_whole_record_is_read_as_it_is() {
    let m = MmapContext::from_raw(&system(0x1234_5000, 0x3000)).unwrap();
    assert_eq!(m.memory, MmapMemory::System);
    assert!(m.on_ctl());
    assert!(m.writable);
    assert_eq!((m.start, m.size), (0x1234_5000, 0x3000));
    let m = MmapContext::from_raw(&aperture(1, caching::CACHED)).unwrap();
    assert_eq!(m.memory, MmapMemory::Registers);
    assert!(!m.on_ctl());
    assert_eq!(m.gpu_instance, 1);
    let m = MmapContext::from_raw(&aperture(2, caching::DEFAULT)).unwrap();
    assert_eq!(m.memory, MmapMemory::Framebuffer);
}

/// Nothing to record, or a record that does not hold up, maps nothing.
#[test]
fn a_record_that_does_not_hold_up_is_not_a_mapping() {
    assert_eq!(MmapContext::from_raw(&RawMmapContext::default()), None);
    let mut r = system(0x1000, 0x1000);
    r.valid = 0;
    assert_eq!(MmapContext::from_raw(&r), None, "not valid");
    let mut r = system(0x1000, 0x1000);
    r.memory_type = 3;
    assert_eq!(MmapContext::from_raw(&r), None, "an unknown kind");
    assert_eq!(
        MmapContext::from_raw(&aperture(3, caching::DEFAULT)),
        None,
        "an unknown kind, off the control node"
    );
    let mut r = system(0x1000, 0x1000);
    r.ctl = 0;
    assert_eq!(
        MmapContext::from_raw(&r),
        None,
        "system memory off the control node"
    );
    let mut r = aperture(1, caching::CACHED);
    r.ctl = 1;
    assert_eq!(
        MmapContext::from_raw(&r),
        None,
        "an aperture on the control node"
    );
    assert_eq!(
        MmapContext::from_raw(&system(0x1001, 0x1000)),
        None,
        "unaligned start"
    );
    assert_eq!(
        MmapContext::from_raw(&system(0x1000, 0x1800)),
        None,
        "a part page"
    );
    assert_eq!(MmapContext::from_raw(&system(0x1000, 0)), None, "empty");
    assert_eq!(
        MmapContext::from_raw(&system(u64::MAX - 0xfff, 0x2000)),
        None,
        "past the end of the address space"
    );
    assert!(MmapContext::from_raw(&system(u64::MAX - 0x1fff, 0x1000)).is_some());
}

#[test]
fn only_the_writeable_bit_makes_a_mapping_writable() {
    for (prot, writable) in [(0, false), (1, false), (2, true), (3, true)] {
        let mut r = system(0x1000, 0x1000);
        r.prot = prot;
        assert_eq!(
            MmapContext::from_raw(&r).unwrap().writable,
            writable,
            "prot {prot}"
        );
    }
}

/// `nv_encode_caching`, and the one place Eclipse departs from it: system
/// memory takes the type the kernel's own mapping of it has.
#[test]
fn every_mapping_gets_the_caching_its_memory_allows() {
    let all = [
        caching::CACHED,
        caching::UNCACHED,
        caching::WRITECOMBINED,
        caching::WRITEBACK,
        caching::DEFAULT,
        caching::UNCACHED_WEAK,
    ];
    for c in all {
        let regs = MmapContext::from_raw(&aperture(1, c)).unwrap();
        assert_eq!(
            regs.cpu_caching(),
            CpuCaching::UncachedDevice,
            "registers, {c}"
        );
        let fb = MmapContext::from_raw(&aperture(2, c)).unwrap();
        let want = match c {
            caching::DEFAULT | caching::WRITECOMBINED => CpuCaching::WriteCombined,
            _ => CpuCaching::UncachedDevice,
        };
        assert_eq!(fb.cpu_caching(), want, "framebuffer, {c}");
        let mut r = system(0x1000, 0x1000);
        r.caching = c;
        let sys = MmapContext::from_raw(&r).unwrap();
        let want = if c == caching::UNCACHED {
            CpuCaching::Uncached
        } else {
            CpuCaching::WriteBack
        };
        assert_eq!(sys.cpu_caching(), want, "system, {c}");
    }
}

/// Hosted builds have no RM to map anything with.
#[test]
fn hosted_maps_are_refused() {
    let _t = turnstile();
    let mut payload = [0u8; 56];
    let caller = Caller {
        os_info: 0x1000,
        ctl: true,
        admin: false,
        gpu_mask: 1,
        pid: 0,
        name: [0; CALLER_NAME_LEN],
    };
    assert_eq!(
        rm_map_memory(&mut payload, &caller),
        (NV_ERR_NOT_SUPPORTED, None)
    );
    let nobody = Caller {
        os_info: 0,
        ..caller
    };
    assert_eq!(
        rm_map_memory(&mut payload, &nobody),
        (NV_ERR_INVALID_ARGUMENT, None)
    );
    map_unrecorded(&mut payload, &caller, NV_ERR_STATE_IN_USE);
    assert_eq!(payload, [0u8; 56], "nothing reaches a payload with no RM");
    assert_eq!(unmapped(), [NV_ERR_STATE_IN_USE]);
    map_unrecorded(&mut payload, &nobody, NV_ERR_STATE_IN_USE);
    assert_eq!(unmapped(), [], "a zero token reaches no RM");
}

/// What the C answers RM_MAP_MEMORY with is handed on only when its record
/// holds up. One that does not is unmapped in the same call, with
/// INVALID_STATE for the client, so no mapping is left that nothing
/// records; no record at all is nothing to unmap.
#[test]
fn a_record_that_does_not_hold_up_is_unmapped_at_once() {
    let _t = turnstile();
    let mut payload = [0u8; 56];
    let caller = Caller {
        os_info: 0x1000,
        ctl: true,
        admin: false,
        gpu_mask: 1,
        pid: 0,
        name: [0; CALLER_NAME_LEN],
    };
    let good = system(0x1000, 0x2000);
    answer_map(NV_OK, good);
    assert_eq!(
        rm_map_memory(&mut payload, &caller),
        (NV_OK, MmapContext::from_raw(&good))
    );
    assert_eq!(unmapped(), []);

    let bad = RawMmapContext {
        start: 0x1001,
        ..good
    };
    answer_map(NV_OK, bad);
    assert_eq!(rm_map_memory(&mut payload, &caller), (NV_OK, None));
    assert_eq!(unmapped(), [NV_ERR_INVALID_STATE]);

    answer_map(NV_OK, RawMmapContext::default());
    assert_eq!(rm_map_memory(&mut payload, &caller), (NV_OK, None));
    assert_eq!(unmapped(), []);
}
