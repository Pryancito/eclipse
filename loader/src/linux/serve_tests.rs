//! The gateway calls by which a process serves something to the kernel --
//! a device node, a filesystem, the mounts of a type the kernel has no
//! code for, a disk, the network -- and the turns those servers take.
//! What lies behind each (`linux_object::fs::served`, `devfs::served`) has
//! tests of its own; these are about the call: who may make it, what it
//! does with the handles it is given, how it reads the caller's memory and
//! which Zircon status each refusal comes back as.

use super::gateway_tests::{a_process, an_unprivileged_process, call, eventually, ptr, zx};
use super::*;
use zircon_object::ipc::Fifo;
use zircon_object::object::{Handle, Rights};
use zircon_object::vm::{VmObject, PAGE_SIZE};

/// A FIFO of `elem`-byte messages: one end in `proc`'s table, the other
/// kept by the test.
fn a_fifo(proc: &Arc<Process>, elem: usize, rights: Rights) -> (u32, Arc<Fifo>) {
    let (ours, theirs) = Fifo::create(16, elem);
    (proc.add_handle(Handle::new(ours, rights)), theirs)
}

/// A window of `bytes` in `proc`'s table.
fn a_vmo(proc: &Arc<Process>, bytes: usize) -> u32 {
    let vmo = VmObject::new_paged(bytes.div_ceil(PAGE_SIZE));
    proc.add_handle(Handle::new(vmo, Rights::DEFAULT_VMO))
}

fn gone(proc: &Arc<Process>, handles: &[u32]) -> bool {
    handles
        .iter()
        .all(|&h| proc.get_dyn_object_with_rights(h, Rights::empty()).is_err())
}

const DEV_SLOT: usize = linux_object::fs::devfs::served::wire::SLOT_LEN;
const FS_SLOT: usize = linux_object::fs::served::wire::SLOT_LEN;

// ---- eclipse_dev_serve ------------------------------------------------

fn dev_serve(proc: &Arc<Process>, name: &mut [u8], rdev: usize, mode: usize) -> (isize, [u32; 2]) {
    let (fifo, _peer) = a_fifo(proc, 32, Rights::DEFAULT_FIFO);
    let window = a_vmo(proc, DEV_SLOT);
    let args = [
        fifo as usize,
        window as usize,
        ptr(&mut name[0]),
        name.len(),
        rdev,
        mode,
    ];
    (
        call(proc, dev_serve::ECLIPSE_DEV_SERVE, args),
        [fifo, window],
    )
}

#[test]
fn dev_serve_is_for_whoever_may_mount_and_takes_the_handles_anyway() {
    let proc = an_unprivileged_process();
    let (answer, handles) = dev_serve(&proc, &mut *b"input/event9".to_vec(), 13 << 32 | 73, 0o660);
    assert_eq!(answer, zx(ZxError::ACCESS_DENIED));
    assert!(
        gone(&proc, &handles),
        "both handles go to the kernel, whatever the answer"
    );
}

#[test]
fn dev_serve_refuses_names_numbers_and_modes_a_node_cannot_have() {
    let proc = a_process();
    let ok = 13usize << 32 | 73;
    for (name, rdev, mode, why) in [
        (
            &b"../../etc/passwd"[..],
            ok,
            0o660,
            "a name that climbs out of /dev",
        ),
        (b"/input/event9", ok, 0o660, "an absolute name"),
        (
            b"input//event9",
            ok,
            0o660,
            "an empty name between two slashes",
        ),
        (b"a/b/c/d/e", ok, 0o660, "five names deep"),
        (b"input/event9", 4096 << 32, 0o660, "a major past 4095"),
        (b"input/event9", 13 << 32 | 256, 0o660, "a minor past 255"),
        (b"input/event9", ok, 0o4755, "setuid"),
        (b"input/event9", ok, 1 << 40, "a mode past 32 bits"),
        (b"\xff\xfe", ok, 0o660, "not UTF-8"),
    ] {
        let (answer, handles) = dev_serve(&proc, &mut name.to_vec(), rdev, mode);
        assert_eq!(answer, zx(ZxError::INVALID_ARGS), "{}", why);
        assert!(gone(&proc, &handles), "{}: the handles stayed", why);
    }
}

#[test]
fn dev_serve_reads_the_name_with_its_length_checked_first() {
    let proc = a_process();
    for (at, len) in [(0x1000, 0), (0x1000, 257), (0, 8)] {
        let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO);
        let window = a_vmo(&proc, DEV_SLOT);
        let args = [fifo as usize, window as usize, at, len, 0, 0o600];
        assert_eq!(
            call(&proc, dev_serve::ECLIPSE_DEV_SERVE, args),
            zx(ZxError::INVALID_ARGS),
            "a name of {} bytes at {:#x}",
            len,
            at
        );
    }
}

#[test]
fn dev_serve_wants_a_fifo_of_its_messages_and_a_window_that_holds_a_slot() {
    let proc = a_process();
    let mut name = b"served-test".to_vec();
    // A FIFO of 16-byte elements.
    let (fifo, _peer) = a_fifo(&proc, 16, Rights::DEFAULT_FIFO);
    let window = a_vmo(&proc, DEV_SLOT);
    let args = [
        fifo as usize,
        window as usize,
        ptr(&mut name[0]),
        name.len(),
        0,
        0o600,
    ];
    assert_eq!(
        call(&proc, dev_serve::ECLIPSE_DEV_SERVE, args),
        zx(ZxError::INVALID_ARGS)
    );
    // A window short of one slot.
    let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO);
    let window = a_vmo(&proc, DEV_SLOT - PAGE_SIZE);
    let args = [
        fifo as usize,
        window as usize,
        ptr(&mut name[0]),
        name.len(),
        0,
        0o600,
    ];
    assert_eq!(
        call(&proc, dev_serve::ECLIPSE_DEV_SERVE, args),
        zx(ZxError::INVALID_ARGS)
    );
    // A FIFO handle that cannot write.
    let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO - Rights::WRITE);
    let window = a_vmo(&proc, DEV_SLOT);
    let args = [
        fifo as usize,
        window as usize,
        ptr(&mut name[0]),
        name.len(),
        0,
        0o600,
    ];
    assert_eq!(
        call(&proc, dev_serve::ECLIPSE_DEV_SERVE, args),
        zx(ZxError::ACCESS_DENIED)
    );
    assert!(gone(&proc, &[fifo, window]));
}

#[test]
fn dev_serve_puts_a_good_node_in_dev_and_keeps_both_handles() {
    // Every Linux process made here builds a root filesystem, and with it
    // the one `/dev` (`linux_object::fs::create_root_fs`), so there is a
    // `/dev` once this one is made. Which `/dev` is not stable while other
    // tests make processes, so this asks no more of it than taking the node.
    let proc = a_process();
    let name = alloc::format!("served-test/node{}", proc.id());
    let (answer, handles) = dev_serve(&proc, &mut name.into_bytes(), 13 << 32 | 99, 0o660);
    assert_eq!(answer, 0);
    assert!(
        gone(&proc, &handles),
        "the kernel keeps the FIFO and the window"
    );
}

// ---- eclipse_fs_serve -------------------------------------------------

fn fs_serve(proc: &Arc<Process>, names: &[u8], flags: usize, kept: bool) -> (isize, [u32; 3]) {
    let (fifo, _peer) = a_fifo(proc, 32, Rights::DEFAULT_FIFO);
    let window = a_vmo(proc, FS_SLOT);
    let kept = if kept { a_vmo(proc, PAGE_SIZE) } else { 0 };
    let mut names = names.to_vec();
    let args = [
        fifo as usize,
        window as usize,
        ptr(&mut names[0]),
        names.len(),
        flags,
        kept as usize,
    ];
    (
        call(proc, fs_serve::ECLIPSE_FS_SERVE, args),
        [fifo, window, kept],
    )
}

#[test]
fn fs_serve_is_for_whoever_may_mount_and_takes_all_three_handles_anyway() {
    let proc = an_unprivileged_process();
    let (answer, handles) = fs_serve(&proc, b"/mnt/x\0src\0vfat", 0, true);
    assert_eq!(answer, zx(ZxError::ACCESS_DENIED));
    assert!(
        gone(&proc, &handles),
        "the handles stayed with a refused server"
    );
}

#[test]
fn fs_serve_wants_three_names_and_a_path_that_is_absolute() {
    let proc = a_process();
    for (names, why) in [
        (&b"/mnt/x\0src"[..], "two names"),
        (b"/mnt/x\0src\0vfat\0more", "four names"),
        (b"mnt/x\0src\0vfat", "a relative place"),
        (b"/mnt/x\0\0vfat", "an empty source"),
        (b"/mnt/x\0src\0v fat", "a type with a space"),
        (b"/mnt/x\0s\xffc\0vfat", "not UTF-8"),
    ] {
        let (answer, handles) = fs_serve(&proc, names, 0, true);
        assert_eq!(answer, zx(ZxError::INVALID_ARGS), "{}", why);
        assert!(gone(&proc, &handles), "{}: the handles stayed", why);
    }
    // Flags it does not know.
    let (answer, _) = fs_serve(&proc, b"/mnt/x\0src\0vfat", 1 << 20, false);
    assert_eq!(answer, zx(ZxError::INVALID_ARGS));
}

#[test]
fn fs_serve_taking_over_a_mount_that_is_not_there_is_not_found() {
    let proc = a_process();
    // Flag 4: take a mount over and make none.
    let (answer, _) = fs_serve(&proc, b"/served-test/nowhere\0src\0vfat", 4, false);
    assert_eq!(answer, zx(ZxError::NOT_FOUND));
}

#[test]
fn fs_serve_reads_the_names_with_their_length_checked_first() {
    let proc = a_process();
    for (at, len) in [(0x1000, 0), (0x1000, 4096 + 256 + 1), (0, 16)] {
        let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO);
        let window = a_vmo(&proc, FS_SLOT);
        let args = [fifo as usize, window as usize, at, len, 0, 0];
        assert_eq!(
            call(&proc, fs_serve::ECLIPSE_FS_SERVE, args),
            zx(ZxError::INVALID_ARGS),
            "names of {} bytes at {:#x}",
            len,
            at
        );
    }
}

// ---- eclipse_mount_help -------------------------------------------------

#[test]
fn there_is_one_mount_helper_at_a_time_and_the_next_comes_when_it_goes() {
    let unprivileged = an_unprivileged_process();
    let (fifo, _peer) = a_fifo(&unprivileged, 32, Rights::DEFAULT_FIFO);
    let window = a_vmo(&unprivileged, FS_SLOT);
    let args = [fifo as usize, window as usize, 0, 0, 0, 0];
    assert_eq!(
        call(&unprivileged, mount_help::ECLIPSE_MOUNT_HELP, args),
        zx(ZxError::ACCESS_DENIED)
    );
    assert!(gone(&unprivileged, &[fifo, window]));

    let first = a_process();
    let (fifo, first_peer) = a_fifo(&first, 32, Rights::DEFAULT_FIFO);
    let window = a_vmo(&first, FS_SLOT);
    let args = [fifo as usize, window as usize, 0, 0, 0, 0];
    assert_eq!(call(&first, mount_help::ECLIPSE_MOUNT_HELP, args), 0);
    assert!(linux_object::fs::served::helper::present());

    let second = a_process();
    let register = |proc: &Arc<Process>| {
        let (fifo, peer) = a_fifo(proc, 32, Rights::DEFAULT_FIFO);
        let window = a_vmo(proc, FS_SLOT);
        let args = [fifo as usize, window as usize, 0, 0, 0, 0];
        (call(proc, mount_help::ECLIPSE_MOUNT_HELP, args), peer)
    };
    let (answer, _) = register(&second);
    assert_eq!(answer, zx(ZxError::ALREADY_BOUND));
    // The first helper's end closes: it is gone, and the next is let in.
    drop(first_peer);
    eventually("the helper that went is no longer there", || {
        !linux_object::fs::served::helper::present()
    });
    let (answer, _kept) = register(&second);
    assert_eq!(answer, 0);
}

// ---- eclipse_device_port ------------------------------------------------

#[test]
fn device_port_is_only_for_the_process_that_holds_the_device() {
    use device_claim::{ECLIPSE_DEVICE_I8042, ECLIPSE_DEVICE_I8042_AUX};
    let proc = a_process();
    for kind in [ECLIPSE_DEVICE_I8042, ECLIPSE_DEVICE_I8042_AUX] {
        // A read of the keyboard's data port, and a write of a command.
        for args in [[kind, 0, 0, 1, 0, 0], [kind, 0, 4, 1, 0xfe, 1]] {
            assert_eq!(
                call(&proc, device_port::ECLIPSE_DEVICE_PORT, args),
                zx(ZxError::ACCESS_DENIED),
                "a port of a device this process never claimed"
            );
        }
    }
}

#[test]
fn device_port_refuses_kinds_without_ports_and_widths_that_are_not_one() {
    use device_claim::ECLIPSE_DEVICE_I8042;
    let proc = a_process();
    for (args, why) in [
        ([ECLIPSE_DEVICE_VIRTIO_BLK, 0, 0, 1, 0, 0], "a disk's ports"),
        ([ECLIPSE_DEVICE_I8042, 0, 0, 3, 0, 0], "three bytes"),
        ([ECLIPSE_DEVICE_I8042, 0, 0, 8, 0, 0], "eight bytes"),
        (
            [ECLIPSE_DEVICE_I8042, 0, 0, 1, 0, 2],
            "neither a read nor a write",
        ),
    ] {
        assert_eq!(
            call(&proc, device_port::ECLIPSE_DEVICE_PORT, args),
            zx(ZxError::INVALID_ARGS),
            "{}",
            why
        );
    }
}

// ---- eclipse_block_serve, eclipse_net_borrow ------------------------------

#[test]
fn block_serve_wants_a_fifo_and_a_window_it_can_read_and_write() {
    let proc = a_process();
    let window = a_vmo(&proc, 16 * PAGE_SIZE);
    let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO - Rights::READ);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_BLOCK_SERVE,
            [fifo as usize, window as usize, 64, 0, 0, 0]
        ),
        zx(ZxError::ACCESS_DENIED)
    );
    // The window's handle where the FIFO's should be.
    assert_eq!(
        call(
            &proc,
            ECLIPSE_BLOCK_SERVE,
            [window as usize, window as usize, 64, 0, 0, 0]
        ),
        zx(ZxError::WRONG_TYPE)
    );
}

#[test]
fn a_boot_that_lends_no_card_refuses_every_borrower() {
    // The host boot lends none; the administrator is refused all the same,
    // and before the out pointer is looked at.
    assert!(!linux_object::net::user_stack::boot::lends_some());
    let proc = a_process();
    let mut out = [0xaau64; 2];
    assert_eq!(
        call(
            &proc,
            ECLIPSE_NET_BORROW,
            [0, 0x1000, 1, 1, ptr(&mut out), 0]
        ),
        zx(ZxError::ACCESS_DENIED)
    );
    assert_eq!(out, [0xaa; 2]);
}

#[test]
fn a_lent_card_turn_wants_a_lent_card() {
    let proc = a_process();
    let (fifo, _peer) = a_fifo(&proc, 32, Rights::DEFAULT_FIFO);
    let mut actual = 0usize;
    let args = [fifo as usize, 0, 0, 0, 0, ptr(&mut actual)];
    assert_eq!(
        net_lend::give_then(&proc, &args).err(),
        Some(ZxError::WRONG_TYPE)
    );
    assert_eq!(
        net_lend::give_then(&proc, &[0xdead, 0, 0, 0, 0, ptr(&mut actual)]).err(),
        Some(ZxError::BAD_HANDLE)
    );
}

// ---- eclipse_fifo_reply_wait, from the call ---------------------------------

/// `eclipse_fifo_reply_wait(fifo, elem_size, buf, replies, room, *actual)`.
fn fifo_turn(proc: &Arc<Process>, args: [usize; 6]) -> ZxResult<fifo_turn::Turn> {
    fifo_turn::reply_then(proc, &args)
}

#[test]
fn a_fifo_turn_sends_nothing_when_actual_is_a_bad_pointer() {
    let proc = a_process();
    let (fifo, peer) = a_fifo(&proc, 8, Rights::DEFAULT_FIFO);
    let mut buf = [7u8; 16];
    let args = [fifo as usize, 8, ptr(&mut buf), 2, 2, 0];
    assert_eq!(fifo_turn(&proc, args).err(), Some(ZxError::INVALID_ARGS));
    let mut got = [0u8; 8];
    assert_eq!(
        peer.read(8, &mut got, 1).err(),
        Some(ZxError::SHOULD_WAIT),
        "replies went out ahead of a call that could not finish"
    );
}

#[test]
fn a_fifo_turn_checks_the_element_size_the_room_and_how_many_replies_fit() {
    let proc = a_process();
    let (fifo, _peer) = a_fifo(&proc, 8, Rights::DEFAULT_FIFO);
    let mut buf = [0u8; 8 * 32];
    let mut actual = 9usize;
    for (args, why) in [
        (
            [fifo as usize, 16, ptr(&mut buf), 0, 1, ptr(&mut actual)],
            "another element size",
        ),
        (
            [fifo as usize, 8, ptr(&mut buf), 0, 0, ptr(&mut actual)],
            "no room to read into",
        ),
        (
            [fifo as usize, 8, ptr(&mut buf), 17, 1, ptr(&mut actual)],
            "more replies than it holds",
        ),
    ] {
        assert_eq!(
            fifo_turn(&proc, args).err(),
            Some(ZxError::OUT_OF_RANGE),
            "{}",
            why
        );
    }
    let (weak, _peer) = a_fifo(&proc, 8, Rights::DEFAULT_FIFO - Rights::READ);
    let args = [weak as usize, 8, ptr(&mut buf), 0, 1, ptr(&mut actual)];
    assert_eq!(fifo_turn(&proc, args).err(), Some(ZxError::ACCESS_DENIED));
    // Replies from a buffer the caller cannot read.
    let args = [fifo as usize, 8, 0, 1, 1, ptr(&mut actual)];
    assert_eq!(fifo_turn(&proc, args).err(), Some(ZxError::INVALID_ARGS));
}

#[async_std::test]
async fn a_fifo_turn_sends_its_replies_then_reads_into_the_same_buffer() {
    let proc = a_process();
    let (fifo, peer) = a_fifo(&proc, 8, Rights::DEFAULT_FIFO);
    let mut buf = *b"reply-1!reply-2!";
    let mut actual = 9usize;
    let args = [fifo as usize, 8, ptr(&mut buf), 2, 2, ptr(&mut actual)];
    let turn = fifo_turn(&proc, args).expect("two replies fit");
    assert_eq!(actual, 0, "actual is cleared before anything is sent");
    let mut got = [0u8; 16];
    assert_eq!(peer.read(8, &mut got, 2), Ok(2));
    assert_eq!(&got, b"reply-1!reply-2!");
    peer.write(8, b"request!", 1).unwrap();
    assert_eq!(turn.wait_then_read().await, Ok(()));
    assert_eq!(actual, 1);
    assert_eq!(&buf[..8], b"request!");
}
