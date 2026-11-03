//! The multiplexer, and the three places it touches the caller's memory.
//!
//! Everything goes in through `compat32_syscall(117, ..)` by number, because
//! that is what an i386 program does and because an arm deleted from
//! [`crate::compat32`] has to fail here rather than leaving a 32-bit program
//! with no System V IPC at all.
//!
//! Fixed pids live in 48_46x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's number for the whole family.
const IPC: u32 = 117;

/// `IPC_CREAT`, so each test makes its own object rather than finding
/// another test's.
const IPC_CREAT: usize = 0o1000;
/// Enough permission for the owner to do anything with it.
const OWNER_RW: usize = 0o600;
/// `IPC_NOWAIT`, on every `msgrcv` here.
///
/// Not tidiness: without it a `msgrcv` whose type matches nothing parks for
/// ever, so a mutation that breaks the type --- the sign of the kludge's
/// `msgtyp`, or the width of the `mtype` --- hangs the suite instead of
/// failing its test. A mutation killed by a hang is not killed by a test.
const IPC_NOWAIT: usize = 0o4000;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

const USER_AT: usize = 0x2300_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
/// Where a structure under test goes, poisoned so its width is readable.
const BUF: usize = USER_AT;
/// A second address, clear of `BUF`, for the out-parameters.
const OUT: usize = USER_AT + 0x800;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "ipc32", lp).unwrap();
    proc.vmar()
        .map_at(
            USER_AT,
            VmObject::new_paged(USER_PAGES),
            0,
            USER_PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .expect("pages of the process's own, at an address a u32 can hold");
    proc
}

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: kernel_hal::user::UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

#[test]
fn the_three_gets_and_shmdt_reach_their_handlers_by_sub_call_number() {
    // The shuffle itself. These four carry no structure at all, so if the
    // sub-call numbers or the argument order were wrong they would fail
    // outright rather than quietly -- which is what makes them the right
    // thing to pin the table with.
    let proc = a_process(48_460, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let sem = sys
                .compat32_syscall(IPC, [2, 0x4846_0001, 1, IPC_CREAT | OWNER_RW, 0, 0])
                .await;
            let msg = sys
                .compat32_syscall(IPC, [13, 0x4846_0002, IPC_CREAT | OWNER_RW, 0, 0, 0])
                .await;
            let shm = sys
                .compat32_syscall(
                    IPC,
                    [23, 0x4846_0003, PAGE_SIZE, IPC_CREAT | OWNER_RW, 0, 0],
                )
                .await;
            // And `shmdt` of an address nothing is attached at, which is the
            // one answer that says the argument reached the handler: it is
            // `ptr` for this sub-call and not `first`.
            let dt = sys.compat32_syscall(IPC, [22, 0, 0, 0, USER_AT, 0]).await;
            (sem, msg, shm, dt)
        })
    });
    assert!(got.0 >= 0, "semget through 117: {}", got.0);
    assert!(got.1 >= 0, "msgget through 117: {}", got.1);
    assert!(got.2 >= 0, "shmget through 117: {}", got.2);
    assert_eq!(
        got.3, EINVAL,
        "shmdt took its address from the wrong argument"
    );
    proc.exit(0);
}

#[test]
fn shmat_returns_zero_and_stores_the_address_in_four_bytes() {
    // The multiplexer's own WRITE, and the one that cannot be seen from the
    // return value: `shmat` through 117 answers **0**, not the address, and
    // puts the address at `third`. In FOUR bytes -- eight would drop a zero
    // high half into whatever the caller keeps next door.
    let proc = a_process(48_461, Abi::Linux32);
    let (id, rc, out) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let id = sys
                .compat32_syscall(
                    IPC,
                    [23, 0x4846_1001, PAGE_SIZE, IPC_CREAT | OWNER_RW, 0, 0],
                )
                .await;
            assert!(id >= 0, "shmget through 117: {}", id);
            put(OUT, &[0xAAu8; 16]);
            // `third` is the out-pointer, `ptr` the requested address (0 =
            // anywhere) and `second` the flags.
            let rc = sys
                .compat32_syscall(IPC, [21, id as usize, 0, OUT, 0, 0])
                .await;
            (id, rc, get(OUT, 16))
        })
    });
    assert_eq!(
        rc, 0,
        "shmat through 117 must answer 0 and not the address (id {})",
        id
    );
    let addr = word(&out, 0);
    assert_ne!(
        addr,
        0xaaaa_aaaa,
        "the address was never stored: {:x?}",
        &out[..8]
    );
    assert_ne!(addr, 0, "the address stored is a null pointer");
    assert_eq!(
        word(&out, 4),
        0xaaaa_aaaa,
        "shmat through 117 stored EIGHT bytes: the zero high half landed in \
         the caller's next variable along. {:x?}",
        &out[..8]
    );
    proc.exit(0);
}

#[test]
fn the_msgbuf_type_is_four_bytes_and_the_payload_starts_after_it() {
    // The pair that is the defect, from one process: the same bytes sent
    // through 117 and read back through 117 must round-trip, and the NATIVE
    // handler reading the same buffer must disagree about where the payload
    // begins.
    //
    // The type is 1 and the payload "ABCD". At the narrow layout those are
    // bytes 0..4 and 4..8. Read eight bytes as one `long`, the type becomes
    // 0x44434241_00000001 -- a huge positive number, so NOT an error, which
    // is exactly why this is worth a test: the call would succeed and queue
    // a message under a type nobody will ask for, with its payload taken
    // from four bytes further on.
    let proc = a_process(48_462, Abi::Linux32);
    let (sent, got, payload, native_type) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let id = sys
                .compat32_syscall(IPC, [13, 0x4846_2001, IPC_CREAT | OWNER_RW, 0, 0, 0])
                .await;
            assert!(id >= 0, "msgget through 117: {}", id);
            let id = id as usize;
            let mut buf = Vec::new();
            buf.extend_from_slice(&1u32.to_ne_bytes());
            buf.extend_from_slice(b"ABCD");
            put(BUF, &buf);
            // MSGSND: first=id, ptr=msgp, second=msgsz, third=msgflg.
            let sent = sys.compat32_syscall(IPC, [11, id, 4, 0, BUF, 0]).await;
            // MSGRCV version 1, so `ptr` IS the buffer and `fifth` the type.
            put(OUT, &[0xAAu8; 16]);
            // The version goes in the CALL argument, not in the syscall
            // number: `call >> 16`.
            let got = sys
                .compat32_syscall(IPC, [12 | (1 << 16), id, 4, IPC_NOWAIT, OUT, 1])
                .await;
            let back = get(OUT, 16);
            // And the native handler on the same buffer, to show the two
            // disagree. Eight bytes read as one type leaves the payload
            // wherever it is: the type it reports is the one the narrow
            // layout keeps the payload in.
            let native_type = {
                let p: kernel_hal::user::UserInPtr<i64> = BUF.into();
                p.read().expect("mapped")
            };
            (sent, got, back, native_type)
        })
    });
    assert_eq!(sent, 0, "msgsnd through 117 (msgsnd(2) answers 0)");
    assert_eq!(got, 4, "msgrcv through 117 did not receive four bytes");
    assert_eq!(
        word(&payload, 0),
        1,
        "the type did not come back in the first four bytes: {:x?}",
        &payload[..8]
    );
    assert_eq!(
        &payload[4..8],
        b"ABCD",
        "the payload did not come back at msgp + 4: {:x?}",
        &payload[..12]
    );
    assert_eq!(
        word(&payload, 8),
        0xaaaa_aaaa,
        "msgrcv wrote past the four bytes of type and four of payload"
    );
    // The number the native reading would have used, spelled out so the test
    // says WHY the width matters rather than only that it does.
    assert_eq!(
        native_type, 0x4443_4241_0000_0001,
        "the eight-byte reading of this buffer is not the number this test \
         is about"
    );
    assert!(
        native_type > 0,
        "the wrong width does not even fail: it queues the message under a \
         type nobody asks for"
    );
    proc.exit(0);
}

#[test]
fn msgrcv_version_zero_reads_the_kludge_and_its_signed_type() {
    // The old form glibc's i386 `msgrcv` has always used: `ptr` points at a
    // `struct ipc_kludge { void *msgp; long msgtyp; }` and not at the
    // buffer. Ignore the version and the kernel takes the ADDRESS OF THE
    // KLUDGE as the message buffer.
    //
    // The type asked for is **-5**, which is `msgrcv`'s "the lowest type not
    // above five" mode. Read unsigned that is four thousand million and
    // nothing ever matches, so this also pins the sign.
    let proc = a_process(48_463, Abi::Linux32);
    let (sent, got, payload) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let id = sys
                .compat32_syscall(IPC, [13, 0x4846_3001, IPC_CREAT | OWNER_RW, 0, 0, 0])
                .await;
            assert!(id >= 0, "msgget through 117: {}", id);
            let id = id as usize;
            let mut buf = Vec::new();
            buf.extend_from_slice(&3u32.to_ne_bytes());
            buf.extend_from_slice(b"wxyz");
            put(BUF, &buf);
            let sent = sys.compat32_syscall(IPC, [11, id, 4, 0, BUF, 0]).await;
            // The kludge: two four-byte words, the buffer and the type.
            let mut kludge = Vec::new();
            kludge.extend_from_slice(&(OUT as u32).to_ne_bytes());
            kludge.extend_from_slice(&(-5i32).to_ne_bytes());
            let kludge_at = BUF + 0x100;
            put(kludge_at, &kludge);
            put(OUT, &[0xAAu8; 16]);
            // Version 0: no high half in `call`.
            let got = sys
                .compat32_syscall(IPC, [12, id, 4, IPC_NOWAIT, kludge_at, 0])
                .await;
            (sent, got, get(OUT, 16))
        })
    });
    assert_eq!(sent, 0, "msgsnd(2) answers 0");
    assert_eq!(
        got, 4,
        "msgrcv version 0 did not read the kludge: either the buffer was \
         taken to be the kludge's own address, or the type of -5 was read \
         unsigned and matched nothing"
    );
    assert_eq!(word(&payload, 0), 3, "the type that came back");
    assert_eq!(&payload[4..8], b"wxyz");
    proc.exit(0);
}

#[test]
fn semctl_takes_its_argument_from_the_word_the_pointer_holds() {
    // `SEMCTL` is the sub-call where `ptr` does NOT point at the structure:
    // it points at a word holding the `union semun`. So `SETVAL` of 7 means
    // a word holding 7, read at four bytes.
    //
    // `GETVAL` afterwards is what says the 7 arrived: it is a scalar in and
    // a scalar out, with no structure anywhere, so nothing else can be the
    // reason it answers 7.
    const SETVAL: usize = 16;
    const GETVAL: usize = 12;
    let proc = a_process(48_464, Abi::Linux32);
    let (set, val, no_ptr) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let id = sys
                .compat32_syscall(IPC, [2, 0x4846_4001, 1, IPC_CREAT | OWNER_RW, 0, 0])
                .await;
            assert!(id >= 0, "semget through 117: {}", id);
            let id = id as usize;
            put(BUF, &7u32.to_ne_bytes());
            let set = sys.compat32_syscall(IPC, [3, id, 0, SETVAL, BUF, 0]).await;
            let val = sys.compat32_syscall(IPC, [3, id, 0, GETVAL, BUF, 0]).await;
            // And the check Linux makes before reading anything: a null
            // `ptr` is EINVAL, not EFAULT.
            let no_ptr = sys.compat32_syscall(IPC, [3, id, 0, SETVAL, 0, 0]).await;
            (set, val, no_ptr)
        })
    });
    assert_eq!(set, 0, "semctl SETVAL through 117");
    assert_eq!(
        val, 7,
        "the value did not arrive: semctl read the union from `ptr` itself \
         instead of from the word it points at"
    );
    assert_eq!(no_ptr, EINVAL, "a null ptr must be EINVAL and not EFAULT");
    proc.exit(0);
}

#[test]
fn the_shapes_that_are_not_written_yet_say_so_rather_than_guessing() {
    // Three refusals, each for its own reason, pinned so that "not
    // implemented" stays visible instead of becoming a silent wrong layout
    // or a wrong wait.
    let proc = a_process(48_465, Abi::Linux32);
    let (stat, set, rmid, timedop, shmat_v1, nonsense) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let id = sys
                .compat32_syscall(IPC, [2, 0x4846_5001, 1, IPC_CREAT | OWNER_RW, 0, 0])
                .await;
            assert!(id >= 0);
            let id = id as usize;
            // IPC_STAT and IPC_SET carry a `semid64_ds`, which is 64 bytes
            // to this caller and 104 here. Writing the native one would
            // overwrite forty bytes of the caller's memory, so: ENOSYS.
            // `| IPC_64` is what an i386 glibc actually sends, on EVERY
            // command, which is the whole reason `ctl_cmd` has to take it
            // off again.
            let stat = sys
                .compat32_syscall(IPC, [3, id, 0, 2 | 0x100, BUF, 0])
                .await;
            let set = sys
                .compat32_syscall(IPC, [3, id, 0, 1 | 0x100, BUF, 0])
                .await;
            // IPC_RMID carries nothing, so it goes through -- and it is the
            // only command in the family with no other reason to fail, which
            // is how the `IPC_64` bit left on showed up at all.
            let rmid = sys.compat32_syscall(IPC, [3, id, 0, 0x100, BUF, 0]).await;
            // No `semtimedop` at either width yet. Said by name rather than
            // silently becoming an untimed `semop`, which would park for
            // ever where the caller asked for a deadline.
            let timedop = sys.compat32_syscall(IPC, [4, 1, 0, 0, BUF, BUF]).await;
            // `shmat` version 1 is an interface that never shipped.
            let shmat_v1 = sys
                .compat32_syscall(IPC, [21 | (1 << 16), 1, 0, OUT, 0, 0])
                .await;
            let nonsense = sys.compat32_syscall(IPC, [99, 0, 0, 0, 0, 0]).await;
            (stat, set, rmid, timedop, shmat_v1, nonsense)
        })
    });
    assert_eq!(stat, ENOSYS, "IPC_STAT must not write the native layout");
    assert_eq!(set, ENOSYS, "IPC_SET must not read the native layout");
    assert_eq!(rmid, 0, "IPC_RMID carries no structure and must go through");
    assert_eq!(timedop, ENOSYS, "semtimedop");
    assert_eq!(shmat_v1, EINVAL, "shmat version 1");
    assert_eq!(nonsense, ENOSYS, "a sub-call that does not exist");
    proc.exit(0);
}

#[test]
fn the_number_is_not_in_the_translation_table() {
    // There is no native `ipc` to translate to, and a row would send 117 to
    // whatever number 117 happens to be here.
    assert_eq!(crate::compat32::native_number(IPC), None);
    assert_eq!(crate::compat32::name(IPC), Some("ipc"));
}
