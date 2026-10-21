//! What a program asks the kernel about the machine it is on: the two names
//! it can set, the whole of `struct utsname` it reads them back from, its
//! execution domain, which CPU it is on, and the numbers `uptime` and `top`
//! print.
//!
//! Six syscalls, and until now **not one test named any of them**.
//! `utsname_tests` next door in `linux-object` covers the pure half --
//! cutting a name to its 65-byte field, and the 390 bytes of the struct --
//! and `uname_machine_tests` and `sysinfo_layout_tests` in this file's
//! parent cover a `const fn` and a set of field offsets. What none of them
//! can ask is the one question userspace cares about: **does the name a
//! program SET come back out of the field a program READS**. The two halves
//! are a global in `linux-object` and a 390-byte write in `linux-syscall`,
//! and nothing joined them up.
//!
//! The same shape for the other three: `personality` answers the PREVIOUS
//! persona and its query must not become a set; `getcpu` has two optional
//! out-parameters; `sysinfo` fills a struct whose layout is pinned and whose
//! WRITE was not.
//!
//! The two names are machine-wide, so every test here that writes one holds
//! [`crate::on_a_thread::the_machine_names`] while it does, and puts back
//! what it found. Each also sets the name it is about rather than trusting
//! the one it inherited, so a test that panicked earlier cannot make the
//! next one lie.
//!
//! One measured surprise: `getcpu`'s two slots the other way round IS
//! pinned, and why is worth knowing. A hosted kernel's `cpu_id()` is not a
//! constant -- it is a slot claimed out of a bitmap by the host thread that
//! asks for it, given back when that thread exits -- so in a test binary
//! with this many live threads the harness thread's slot is almost never 0,
//! while the one NUMA node always is, and the two numbers differ. The
//! mutation lot killed the swap for exactly that reason. It is luck rather
//! than law: a run whose harness thread happened to hold slot 0 would let it
//! through, and nothing here can choose the slot.
//!
//! Fixed pids live in 47_21x.

extern crate std;

use super::*;
use crate::on_a_thread::{on_a_thread, the_machine_names};
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, CAP_SYS_ADMIN};
use linux_object::uname::{self, HOST_NAME_MAX, OS_RELEASE, OS_TYPE, UTSNAME_LEN, UTS_FIELD_LEN};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// An ordinary user, who holds no capability at all.
const A_USER: u32 = 1000;

/// `ADDR_NO_RANDOMIZE`, the one persona bit a program is likely to ask for
/// (`setarch -R`). Its value matters only in that it is not zero and not the
/// query.
const A_PERSONA: usize = 0x0004_0000;

/// What a slot holds before a call, so "left alone" cannot pass for
/// "written".
const A_SEED: u32 = 0xdead_beef;

/// The six fields of `struct utsname`, in order.
const SYSNAME: usize = 0;
const NODENAME: usize = 1;
const RELEASE: usize = 2;
const MACHINE: usize = 4;
const DOMAINNAME: usize = 5;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// The `Syscall` the harness lends, as a `&mut` one: `sethostname`,
/// `setdomainname` and `sysinfo` take one, and every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

fn sethostname(sys: &Syscall<'_>, name: &[u8], len: usize) -> SysResult {
    as_mut(sys).sys_sethostname((name.as_ptr() as usize).into(), len)
}

fn setdomainname(sys: &Syscall<'_>, name: &[u8], len: usize) -> SysResult {
    as_mut(sys).sys_setdomainname((name.as_ptr() as usize).into(), len)
}

/// The whole 390-byte `struct utsname`, as the syscall writes it, out of a
/// buffer seeded so that a byte it does not write shows up as one.
fn utsname_of(sys: &Syscall<'_>) -> [u8; UTSNAME_LEN] {
    let mut buf = [0xffu8; UTSNAME_LEN];
    assert_eq!(
        sys.sys_uname((buf.as_mut_ptr() as usize).into()),
        Ok(0),
        "uname failed"
    );
    buf
}

/// One field of that struct, raw: all 65 bytes, NUL padding included.
fn raw_field(buf: &[u8; UTSNAME_LEN], at: usize) -> &[u8] {
    &buf[at * UTS_FIELD_LEN..(at + 1) * UTS_FIELD_LEN]
}

/// One field as userspace reads it: the bytes before its first NUL.
fn field(buf: &[u8; UTSNAME_LEN], at: usize) -> &[u8] {
    let whole = raw_field(buf, at);
    let end = whole.iter().position(|&b| b == 0).unwrap_or(whole.len());
    &whole[..end]
}

#[test]
fn the_host_name_sethostname_stores_is_the_one_uname_reports_in_its_own_field() {
    // The join nothing tested: `sethostname` writes a global in
    // `linux-object` and `uname` builds 390 bytes in `linux-syscall`, and
    // the only thing that says the second reads the first is a program
    // calling both. `hostname` and `hostname -F /etc/hostname` are that
    // program, and Alpine's boot scripts run it before anything else.
    let _names = the_machine_names();
    let was_host = uname::hostname();
    let was_domain = uname::domainname();
    let proc = a_process(47_210, "uts-host");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            const NAME: &[u8] = b"a-named-host";
            assert_eq!(
                sethostname(sys, NAME, NAME.len()),
                Ok(0),
                "sethostname failed"
            );
            assert_eq!(
                uname::hostname().as_bytes(),
                NAME,
                "sethostname did not store the name it was given"
            );

            let buf = utsname_of(sys);
            assert_eq!(
                field(&buf, NODENAME),
                NAME,
                "the name is not in the field uname reports it in"
            );
            // The name landing in its own field and in no other: the three
            // around it are what this build says they are, whatever the
            // host name is.
            assert_eq!(
                field(&buf, SYSNAME),
                OS_TYPE.as_bytes(),
                "sysname moved with the host name"
            );
            assert_eq!(
                field(&buf, RELEASE),
                OS_RELEASE.as_bytes(),
                "release moved with the host name"
            );
            assert_eq!(
                field(&buf, DOMAINNAME),
                uname::domainname().as_bytes(),
                "the domain name moved with the host name"
            );
        })
    });
    uname::set_hostname(&was_host);
    uname::set_domainname(&was_domain);
}

#[test]
fn the_domain_name_setdomainname_stores_lands_in_the_last_field_and_not_the_second() {
    // The mirror, and the pair is what tells the two syscalls apart: they
    // take the same two arguments and both answer zero, so only which name
    // moved says which one ran.
    let _names = the_machine_names();
    let was_host = uname::hostname();
    let was_domain = uname::domainname();
    let proc = a_process(47_211, "uts-domain");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            const HOST: &[u8] = b"a-host-of-its-own";
            const DOMAIN: &[u8] = b"a-domain-of-its-own";
            assert_eq!(
                sethostname(sys, HOST, HOST.len()),
                Ok(0),
                "sethostname failed"
            );
            assert_eq!(
                setdomainname(sys, DOMAIN, DOMAIN.len()),
                Ok(0),
                "setdomainname failed"
            );

            let buf = utsname_of(sys);
            assert_eq!(
                field(&buf, DOMAINNAME),
                DOMAIN,
                "the domain name is not in the last field"
            );
            assert_eq!(
                field(&buf, NODENAME),
                HOST,
                "setdomainname moved the host name as well, or instead"
            );
        })
    });
    uname::set_hostname(&was_host);
    uname::set_domainname(&was_domain);
}

#[test]
fn a_name_as_long_as_the_field_allows_is_accepted_and_the_next_field_is_untouched() {
    // `HOST_NAME_MAX` bytes is the longest name there is, and the field is
    // one byte wider for the NUL -- so the boundary is where a name would
    // start writing the field after it. `uname` used to write each name
    // with `write_cstring` at its field's offset, which did exactly that.
    let _names = the_machine_names();
    let was_host = uname::hostname();
    let proc = a_process(47_212, "uts-long");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let longest = alloc::vec![b'n'; HOST_NAME_MAX];
            assert_eq!(
                sethostname(sys, &longest, HOST_NAME_MAX),
                Ok(0),
                "a name of exactly HOST_NAME_MAX bytes was refused"
            );
            let buf = utsname_of(sys);
            assert_eq!(
                field(&buf, NODENAME),
                &longest[..],
                "the longest name did not arrive whole"
            );
            assert_eq!(
                raw_field(&buf, NODENAME)[HOST_NAME_MAX],
                0,
                "the longest name left its field without a terminator"
            );
            assert_eq!(
                field(&buf, RELEASE),
                OS_RELEASE.as_bytes(),
                "the longest name ran into the field after it"
            );

            // One byte more is not a name.
            let too_long = alloc::vec![b'm'; HOST_NAME_MAX + 1];
            assert_eq!(
                sethostname(sys, &too_long, HOST_NAME_MAX + 1),
                Err(LxError::EINVAL),
                "a name past HOST_NAME_MAX was accepted"
            );
            assert_eq!(
                setdomainname(sys, &too_long, HOST_NAME_MAX + 1),
                Err(LxError::EINVAL),
                "a domain name past HOST_NAME_MAX was accepted"
            );
            assert_eq!(
                field(&utsname_of(sys), NODENAME),
                &longest[..],
                "the refused name was stored anyway"
            );
        })
    });
    uname::set_hostname(&was_host);
}

#[test]
fn the_capability_is_asked_before_the_length_so_a_stranger_gets_eperm_not_einval() {
    // Both calls check `CAP_SYS_ADMIN` ahead of the length, as `kernel/sys.c`
    // does, and the order is observable from one call: a name that is too
    // long, asked by somebody who may not set names, has to come back
    // `EPERM`. With the two checks the other way round an unprivileged
    // program would be told its name was too long -- and `hostname` reports
    // what it is told, so the message would send the user to shorten a name
    // that was never the problem.
    let _names = the_machine_names();
    let was_host = uname::hostname();
    let was_domain = uname::domainname();
    let proc = a_process(47_213, "uts-stranger");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();
            assert!(
                !lp.capable(CAP_SYS_ADMIN),
                "the caller still holds the capability this test is about"
            );

            let too_long = alloc::vec![b'x'; HOST_NAME_MAX + 1];
            assert_eq!(
                sethostname(sys, &too_long, HOST_NAME_MAX + 1),
                Err(LxError::EPERM),
                "sethostname judged the length before the caller"
            );
            assert_eq!(
                setdomainname(sys, &too_long, HOST_NAME_MAX + 1),
                Err(LxError::EPERM),
                "setdomainname judged the length before the caller"
            );

            // And a name that would have been fine is refused too.
            const SHORT: &[u8] = b"not-mine-to-set";
            assert_eq!(
                sethostname(sys, SHORT, SHORT.len()),
                Err(LxError::EPERM),
                "an unprivileged caller set the host name"
            );
            assert_eq!(
                setdomainname(sys, SHORT, SHORT.len()),
                Err(LxError::EPERM),
                "an unprivileged caller set the domain name"
            );
            let buf = utsname_of(sys);
            assert_eq!(
                field(&buf, NODENAME),
                uname::hostname().as_bytes(),
                "a refused call left a name of its own behind"
            );
            assert_ne!(
                field(&buf, NODENAME),
                SHORT,
                "the refused name was stored anyway"
            );
        })
    });
    uname::set_hostname(&was_host);
    uname::set_domainname(&was_domain);
}

#[test]
fn the_name_is_read_as_a_length_and_not_as_a_c_string() {
    // `kernel/sys.c` copies `len` bytes and terminates them itself, so a
    // name with a NUL in the middle of it is stored whole -- and a kernel
    // that read a C string instead would store the first word of it and
    // report success, having quietly renamed the machine to something else.
    let _names = the_machine_names();
    let was_host = uname::hostname();
    let proc = a_process(47_214, "uts-length");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            const NAME: &[u8] = b"abc\0def";
            assert_eq!(
                sethostname(sys, NAME, NAME.len()),
                Ok(0),
                "sethostname failed"
            );
            assert_eq!(
                uname::hostname().as_bytes(),
                NAME,
                "the bytes past the NUL were dropped, so the name was read as a C string"
            );
            assert_eq!(
                &raw_field(&utsname_of(sys), NODENAME)[..NAME.len()],
                NAME,
                "uname did not copy the whole name into its field"
            );

            // The length is what is read, not the buffer: fewer bytes than
            // the caller has is a shorter name, not a truncated read of the
            // whole thing.
            assert_eq!(
                sethostname(sys, NAME, 3),
                Ok(0),
                "sethostname with a short length failed"
            );
            assert_eq!(
                uname::hostname().as_bytes(),
                b"abc",
                "the length the caller gave was not the length that was read"
            );
        })
    });
    uname::set_hostname(&was_host);
}

#[test]
fn uname_writes_every_byte_of_the_struct_and_the_machine_of_this_build() {
    // 390 bytes, not six strings: a caller that reads `machine` out of a
    // struct whose tail was never written gets whatever was on its stack.
    // The buffer here is seeded with `0xff`, which is neither a name nor a
    // terminator.
    let proc = a_process(47_215, "uts-whole");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let buf = utsname_of(sys);
            assert_eq!(
                field(&buf, MACHINE),
                uname_machine(
                    cfg!(target_arch = "x86_64"),
                    cfg!(target_arch = "aarch64"),
                    cfg!(target_arch = "riscv64"),
                )
                .as_bytes(),
                "uname does not report the machine this build is for"
            );
            for at in 0..6 {
                assert!(
                    raw_field(&buf, at).contains(&0),
                    "field {} has no terminator inside its own 65 bytes",
                    at
                );
                assert!(
                    !raw_field(&buf, at).contains(&0xff),
                    "field {} has a byte uname never wrote",
                    at
                );
            }
            assert_eq!(
                buf[UTSNAME_LEN - 1],
                0,
                "the last byte of the struct was not written"
            );
        })
    });
}

#[test]
fn personality_answers_the_previous_persona_and_its_query_leaves_it_alone() {
    // `0xffffffff` is the query, and everything else is a set that answers
    // the persona as it WAS. A set that answered the new value would read
    // correctly from the call site and break every caller that does what
    // the manual says -- ask with the query, OR in a bit, set, and keep the
    // answer to put back.
    let proc = a_process(47_216, "uts-persona");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let was = lp.personality() as usize;

            assert_eq!(
                sys.sys_personality(A_PERSONA),
                Ok(was),
                "setting a persona did not answer the one before it"
            );
            assert_eq!(
                lp.personality() as usize,
                A_PERSONA,
                "the persona that was set is not the one stored"
            );

            // The query, twice: it answers the stored persona and is not a
            // set of `0xffffffff`.
            assert_eq!(
                sys.sys_personality(0xffff_ffff),
                Ok(A_PERSONA),
                "the query did not answer the persona in force"
            );
            assert_eq!(
                sys.sys_personality(0xffff_ffff),
                Ok(A_PERSONA),
                "the query changed the persona it was asked about"
            );

            assert_eq!(
                sys.sys_personality(was),
                Ok(A_PERSONA),
                "putting the persona back did not answer the one before it"
            );
            assert_eq!(
                lp.personality() as usize,
                was,
                "the persona did not go back"
            );
        })
    });
}

#[test]
fn getcpu_writes_each_slot_it_is_given_and_skips_a_null_one() {
    // musl's `sched_getcpu` is this syscall and nothing else, and both of
    // its out-parameters are optional: a caller that wants only the CPU
    // passes NULL for the node, and a kernel that wrote through it anyway
    // would fault a program that is doing exactly what the manual says.
    let proc = a_process(47_217, "uts-getcpu");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut cpu = A_SEED;
            let mut node = A_SEED;
            assert_eq!(
                sys.sys_getcpu(
                    (&mut cpu as *mut u32 as usize).into(),
                    (&mut node as *mut u32 as usize).into(),
                    0
                ),
                Ok(0),
                "getcpu with both slots failed"
            );
            assert_eq!(
                cpu,
                kernel_hal::cpu::cpu_id() as u32,
                "getcpu did not write the CPU this thread is on"
            );
            assert_eq!(node, 0, "getcpu did not write the one NUMA node there is");

            // Each one alone, and neither of them.
            let mut only = A_SEED;
            assert_eq!(
                sys.sys_getcpu(0.into(), (&mut only as *mut u32 as usize).into(), 0),
                Ok(0),
                "getcpu with a null CPU slot failed"
            );
            assert_ne!(only, A_SEED, "getcpu skipped the node slot it was given");
            let mut alone = A_SEED;
            assert_eq!(
                sys.sys_getcpu((&mut alone as *mut u32 as usize).into(), 0.into(), 0),
                Ok(0),
                "getcpu with a null node slot failed"
            );
            assert_ne!(alone, A_SEED, "getcpu skipped the CPU slot it was given");
            assert_eq!(
                sys.sys_getcpu(0.into(), 0.into(), 0),
                Ok(0),
                "getcpu with nowhere to write failed"
            );
        })
    });
}

#[test]
fn sysinfo_fills_the_struct_the_caller_gave_it_and_nowhere_is_efault() {
    // `sysinfo_layout_tests` pins where each field sits; what it cannot ask
    // is whether the write happens at all. `uptime`'s headline was exactly
    // that: the call answered zero and left the struct as it found it, so
    // `uptime` and `top` reported "up 0 min" forever.
    let proc = a_process(47_218, "uts-sysinfo");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut info = SysInfo {
                uptime: u64::MAX,
                loads: [u64::MAX; 3],
                totalram: u64::MAX,
                freeram: u64::MAX,
                sharedram: u64::MAX,
                bufferram: u64::MAX,
                totalswap: u64::MAX,
                freeswap: u64::MAX,
                procs: u16::MAX,
                totalhigh: u64::MAX,
                freehigh: u64::MAX,
                mem_unit: u32::MAX,
                ..SysInfo::default()
            };
            assert_eq!(
                as_mut(sys).sys_sysinfo((&mut info as *mut SysInfo as usize).into()),
                Ok(0),
                "sysinfo failed"
            );

            assert_eq!(info.mem_unit, 1, "the memory unit was not written");
            assert!(info.totalram > 0, "there is no memory at all");
            assert!(
                info.freeram <= info.totalram,
                "more memory free than there is"
            );
            assert!(info.procs >= 1, "nothing is running, not even this test");
            // The uptime is the monotonic clock's own seconds, the source
            // `/proc/uptime` reads too. Asked OF the clock here rather than
            // just compared against the seed: a kernel that answered a
            // constant would pass a test that only looked for "not what I
            // put there".
            let now = timer_now().as_secs();
            assert!(
                info.uptime <= now && now - info.uptime <= 1,
                "the uptime is not the clock's: {} against {}",
                info.uptime,
                now
            );
            // The fields this kernel has no number for are written too, as
            // zeros: left alone they would come back as whatever the caller
            // had there, which is what the seed above stands for.
            assert_eq!(
                (
                    info.sharedram,
                    info.bufferram,
                    info.totalswap,
                    info.freeswap
                ),
                (0, 0, 0, 0),
                "a field with no number of its own was left as the caller had it"
            );
            assert_eq!(
                (info.totalhigh, info.freehigh),
                (0, 0),
                "the high-memory fields were left as the caller had them"
            );

            assert_eq!(
                as_mut(sys).sys_sysinfo(0.into()),
                Err(LxError::EFAULT),
                "sysinfo wrote to nowhere and said it had"
            );
        })
    });
}
