//! The `/proc/gpudbg` sections and the cmdline switches nothing exercised:
//! `format_prime`, `format_gr_hang_probe`, `format_last_exec`,
//! `format_client_mem` and `format_exec_profile` only ran on a real GPU
//! with a terminal reading the file, and `nvidia.console_gsp`,
//! `nvidia.hwflip` and `nvidia.noosinitmapping` only from `zCore`'s
//! cmdline. Every static here is process-wide, so each test takes
//! `crate::test_globals::LOCK`, starts from a known state and puts the
//! switches back.

use super::*;

/// The lines of a section after its `---` header, with the `[gpudbg]  `
/// prefix stripped: what a terminal user reads.
fn body(section: &str) -> alloc::vec::Vec<&str> {
    section
        .lines()
        .filter(|l| !l.contains("---"))
        .map(|l| l.strip_prefix("[gpudbg]  ").unwrap_or(l))
        .collect()
}

fn clear_last_exec_state() {
    *LAST_CLIENT_EXEC.lock() = None;
    LAST_PRIME.lock().clear();
    *LAST_GR_HANG_PROBE.lock() = None;
    *LAST_CLIENT_IOCTL_ERR.lock() = None;
}

fn clear_client_mem_counters() {
    for c in [
        &GEM_TOTAL,
        &GEM_REQ_VRAM,
        &GEM_REQ_GART,
        &GEM_CPU_MAPPABLE,
        &GEM_TILED,
        &GEM_VRAM_BACKED,
        &GEM_VRAM_ONLY,
        &GEM_GART_ONLY,
        &GEM_BOTH,
        &VMBIND_MAP,
        &VMBIND_UNMAP,
        &VMBIND_SPARSE,
        &VMBIND_NON_GENERIC,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    for c in [&GEM_VRAM_ONLY_BYTES, &GEM_GART_ONLY_BYTES, &GEM_BOTH_BYTES] {
        c.store(0, Ordering::Relaxed);
    }
    for w in KINDS_SEEN.iter() {
        w.store(0, Ordering::Relaxed);
    }
}

fn clear_profile() {
    for st in IOCTL_PROFILE.iter() {
        st.count.store(0, Ordering::Relaxed);
        st.total_us.store(0, Ordering::Relaxed);
        st.max_us.store(0, Ordering::Relaxed);
    }
}

/// Each switch answers what its setter was last told, and none of the
/// three reads another's flag. `noosinitmapping` lives in `nvidia-rm-sys`
/// and this is only its door, so it is put back exactly as found.
#[test]
fn the_switches_answer_what_the_cmdline_last_said() {
    let _s = crate::test_globals::LOCK.lock();
    let (gsp, flip, osinit) = (
        console_gsp_enabled(),
        hwflip_enabled(),
        osinit_mapping_enabled(),
    );
    set_console_gsp_enabled(true);
    set_hwflip_enabled(false);
    set_osinit_mapping_enabled(false);
    assert!(console_gsp_enabled());
    assert!(!hwflip_enabled());
    assert!(!osinit_mapping_enabled());
    set_console_gsp_enabled(false);
    set_hwflip_enabled(true);
    set_osinit_mapping_enabled(true);
    assert!(!console_gsp_enabled());
    assert!(hwflip_enabled());
    assert!(osinit_mapping_enabled());
    set_console_gsp_enabled(gsp);
    set_hwflip_enabled(flip);
    set_osinit_mapping_enabled(osinit);
}

/// One line per primed context, a later prime of the same context
/// replaces its line, the order is first-primed first, and an empty table
/// says so instead of printing nothing.
#[test]
fn the_prime_section_keeps_one_line_per_context_and_the_latest_prime_wins() {
    let _s = crate::test_globals::LOCK.lock();
    clear_last_exec_state();
    assert_eq!(
        body(&format_prime()),
        ["(none this boot -- no client context has been primed yet)"]
    );
    record_prime(1, alloc::string::String::from("ctx1 prime OK"));
    record_prime(2, alloc::string::String::from("ctx2 prime TIMEOUT"));
    record_prime(1, alloc::string::String::from("ctx1 prime OK again"));
    assert_eq!(
        body(&format_prime()),
        ["ctx1 prime OK again", "ctx2 prime TIMEOUT"]
    );
    assert_eq!(
        prime_line_for(2).as_deref(),
        Some("ctx2 prime TIMEOUT"),
        "the probe asks for a context's line by number"
    );
    assert_eq!(prime_line_for(3), None);
    clear_last_exec_state();
}

/// The first fence-wait timeout's snapshot is kept, line by line, and a
/// later one does not replace it: the ring stays wedged and every later
/// submit repeats the same picture.
#[test]
fn the_gr_hang_probe_keeps_the_first_snapshot_only() {
    let _s = crate::test_globals::LOCK.lock();
    clear_last_exec_state();
    assert_eq!(
        body(&format_gr_hang_probe()),
        ["(none this boot -- no fence-wait timeout has occurred)"]
    );
    record_gr_hang_probe(alloc::string::String::from(
        "MMU_FAULT valid=1\nGP_GET=3 GP_PUT=4",
    ));
    record_gr_hang_probe(alloc::string::String::from("a later timeout"));
    assert_eq!(
        body(&format_gr_hang_probe()),
        ["MMU_FAULT valid=1", "GP_GET=3 GP_PUT=4"]
    );
    clear_last_exec_state();
}

/// The section a terminal `cat`s after a client died: the last EXEC, then
/// the persistent prime verdict, then the hang probe, then the last ioctl
/// error, each saying "none" until something is recorded, and the error
/// line naming pid, request, number, ioctl and errno.
#[test]
fn the_last_exec_section_tells_the_exec_the_prime_the_probe_and_the_error_in_that_order() {
    let _s = crate::test_globals::LOCK.lock();
    clear_last_exec_state();
    let empty = format_last_exec();
    let at = |needle: &str| {
        empty
            .find(needle)
            .unwrap_or_else(|| panic!("missing {:?}", needle))
    };
    let exec = at("no GL/Vulkan client reached a real draw submit yet");
    let prime = at("no client context has been primed yet");
    let probe = at("no fence-wait timeout has occurred");
    let err = at("no client nouveau ioctl has returned an error");
    assert!(exec < prime && prime < probe && probe < err, "{}", empty);

    record_client_exec(alloc::string::String::from("EXEC ctx=1 OK fence landed"));
    record_ioctl_err(77, 0xC018_0000 | NR_EXEC, ENOSYS);
    let full = format_last_exec();
    assert!(
        full.contains("[gpudbg]  EXEC ctx=1 OK fence landed"),
        "{}",
        full
    );
    let expected = alloc::format!(
        "[gpudbg]  pid=77 ioctl {:#010x} (nr={:#04x} {}) -> errno={}",
        0xC018_0000u32 | NR_EXEC,
        NR_EXEC,
        nouveau_ioctl_name(NR_EXEC),
        ENOSYS
    );
    assert!(full.contains(&expected), "{}\nwanted {}", full, expected);
    assert!(!full.contains("no GL/Vulkan client reached"), "{}", full);
    assert!(
        !full.contains("no client nouveau ioctl has returned"),
        "{}",
        full
    );
    clear_last_exec_state();
}

/// The memory summary is one line of "none" until a `GEM_NEW`, and then
/// every count adds up: requests by domain (which overlap), the three
/// disjoint buckets with their bytes, the VM_BIND tallies, and the PTE
/// kinds every bind and every tiled object brought in.
#[test]
fn the_client_mem_section_is_empty_until_a_gem_new_and_then_adds_up() {
    let _s = crate::test_globals::LOCK.lock();
    clear_client_mem_counters();
    assert_eq!(
        body(&format_client_mem()),
        ["(none this boot -- no GL/Vulkan client has allocated GEM memory yet)"]
    );
    const MIB: u64 = 1024 * 1024;
    record_gem_new(NOUVEAU_GEM_DOMAIN_VRAM, 2 * MIB, false, 0, true);
    record_gem_new(NOUVEAU_GEM_DOMAIN_GART, MIB, true, 0, false);
    record_gem_new(
        NOUVEAU_GEM_DOMAIN_VRAM | NOUVEAU_GEM_DOMAIN_GART,
        4 * MIB,
        true,
        0x0700,
        false,
    );
    record_vm_bind(true, false, 0x00, false);
    record_vm_bind(true, true, 0x05, true);
    record_vm_bind(false, false, 0x00, false);
    let s = format_client_mem();
    let lines = body(&s);
    assert_eq!(
        lines[0], "GEM_NEW: total=3 req_vram=2 req_gart=2 cpu_mappable=2 tiled=1 vram_backed=1",
        "{}",
        s
    );
    assert_eq!(
        lines[1], "GEM_NEW domains: vram_only=1 (2 MiB) gart_only=1 (1 MiB) both=1 (4 MiB)",
        "{}",
        s
    );
    assert!(lines[2].starts_with("GEM host PA handed out"), "{}", s);
    assert!(
        lines[3].starts_with("VM_BIND: map=2 unmap=1 sparse=1 non_generic_kind=1 <-"),
        "{}",
        s
    );
    assert_eq!(
        lines[4], "PTE kinds seen: 0x00 0x05 0x07",
        "every bind notes its kind, pitch included; the tiled object its tile kind: {}",
        s
    );
    clear_client_mem_counters();
}

/// The profile table lists only the ioctls that ran, each with its count,
/// total, average and maximum, and the header line says which EXEC path
/// is in force.
#[test]
fn the_exec_profile_lists_only_the_ioctls_that_ran_with_their_average_and_max() {
    let _s = crate::test_globals::LOCK.lock();
    clear_profile();
    let fast = exec_fast_enabled();
    set_exec_fast_enabled(true);
    profile_ioctl(NR_EXEC, 10);
    profile_ioctl(NR_EXEC, 30);
    profile_ioctl(NR_GEM_NEW, 5);
    let s = format_exec_profile();
    assert!(s.contains("EXEC direct-submit: ENABLED (path"), "{}", s);
    let rows: alloc::vec::Vec<alloc::vec::Vec<&str>> = s
        .lines()
        .skip_while(|l| !l.contains("ioctl            count"))
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        rows,
        [
            alloc::vec!["[gpudbg]", "EXEC", "2", "40", "20", "30"],
            alloc::vec!["[gpudbg]", "GEM_NEW", "1", "5", "5", "5"],
        ],
        "{}",
        s
    );
    set_exec_fast_enabled(false);
    assert!(
        format_exec_profile().contains("EXEC direct-submit: DISABLED (nvidia.exec_rm)"),
        "the legacy path says so"
    );
    set_exec_fast_enabled(fast);
    clear_profile();
}
