//! The Eclipse extension calls of the Zircon gateway, driven the way a
//! process drives them: a Linux process of its own, handles in its table,
//! and pointers into memory the test owns (under `libos` a user pointer is
//! a host address). What each test asserts is what the call promises in
//! its doc comment, above all the parts a driver or an attacker leans on:
//! who may make it, what a bad pointer costs, and what is left behind when
//! it fails.

extern crate std;

use super::*;
use kernel_hal::drivers::user::{self as udev, Transport, Wiring};
use linux_object::fs::{FileDesc, HandleFd, OpenFlags};
use linux_object::process::CAP_SYS_ADMIN;
use rcore_fs_ramfs::RamFS;
use zircon_object::dev::BusTransactionInitiator;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::{Handle, Rights};
use zircon_object::vm::{VmObject, PAGE_SIZE};

/// A pid no other test is using: `Process::create_linux` takes its pid
/// from the caller, and the tests share one process table.
pub(super) fn a_pid() -> KoID {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x5000_0000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A Linux process with the capabilities the first one has (all of them).
pub(super) fn a_process() -> Arc<Process> {
    let job = zircon_object::task::ROOT_JOB.clone();
    Process::create_linux(&job, RamFS::new(), 0, None, a_pid())
        .expect("a Linux process on an empty ramfs")
}

/// A Linux process without `CAP_SYS_ADMIN` in its effective set: an
/// ordinary program.
pub(super) fn an_unprivileged_process() -> Arc<Process> {
    let proc = a_process();
    proc.linux()
        .change_capabilities(|caps| {
            caps.effective &= !(1u64 << CAP_SYS_ADMIN);
            Ok(())
        })
        .expect("dropping a capability always works");
    assert!(!proc.linux().capable(CAP_SYS_ADMIN));
    proc
}

pub(super) fn call(proc: &Arc<Process>, num: u32, args: [usize; 6]) -> isize {
    eclipse_gateway_call(proc, num, &args)
}

pub(super) fn zx(e: ZxError) -> isize {
    e as isize
}

/// One end of a fresh channel in `proc`'s table with `rights`, and the
/// other end, kept by the test.
pub(super) fn a_channel(proc: &Arc<Process>, rights: Rights) -> (u32, Arc<Channel>) {
    let (ours, theirs) = Channel::create();
    (proc.add_handle(Handle::new(ours, rights)), theirs)
}

pub(super) fn ptr<T>(x: &mut T) -> usize {
    x as *mut T as usize
}

// ---- who may make which call ------------------------------------------

#[test]
fn only_the_device_disk_and_network_calls_are_the_administrators() {
    for num in [
        ECLIPSE_DEVICE_CLAIM,
        ECLIPSE_BLOCK_SERVE,
        ECLIPSE_NETSTACK_SERVE,
        ECLIPSE_NET_BORROW,
    ] {
        assert!(
            gateway_call_is_admins(num),
            "{:#x} is the administrator's",
            num
        );
    }
    // A channel turned into an fd and back, a redirect of the caller's own
    // syscalls, a server reading its own client: every process's.
    for num in [
        ECLIPSE_HANDLE_TO_FD,
        ECLIPSE_FD_TO_HANDLE,
        ECLIPSE_REDIRECT_SYSCALLS,
        ECLIPSE_REDIRECT_MEMORY,
        ECLIPSE_CHANNEL_REPLY_WAIT,
        ECLIPSE_FIFO_REPLY_WAIT,
        ECLIPSE_NET_LENT_TURN,
    ] {
        assert!(!gateway_call_is_admins(num), "{:#x} is everyone's", num);
    }
}

#[test]
fn a_process_without_cap_sys_admin_is_refused_before_anything_is_looked_at() {
    let proc = an_unprivileged_process();
    // A channel in its table with every right a serve call asks for: had
    // the call got as far as taking it, the handle would be gone.
    let (value, _peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut claim = [0xaau64; CLAIM_WORDS];
    let claim_at = ptr(&mut claim);
    for (num, args) in [
        (
            ECLIPSE_DEVICE_CLAIM,
            [ECLIPSE_DEVICE_VIRTIO_BLK, 0, claim_at, 0, 0, 0],
        ),
        (
            ECLIPSE_BLOCK_SERVE,
            [value as usize, value as usize, 64, 0, 0, 0],
        ),
        (ECLIPSE_NETSTACK_SERVE, [value as usize, 1500, 0, 24, 0, 0]),
        (ECLIPSE_NET_BORROW, [0, claim_at, 1, 1, claim_at, 0]),
    ] {
        assert_eq!(
            call(&proc, num, args),
            zx(ZxError::ACCESS_DENIED),
            "{:#x} from an unprivileged process",
            num
        );
    }
    assert!(
        proc.get_object::<Channel>(value).is_ok(),
        "a refused call took the caller's handle"
    );
    assert_eq!(
        claim, [0xaa; CLAIM_WORDS],
        "a refused claim wrote to the caller"
    );
}

#[test]
fn a_number_past_the_extensions_is_not_supported() {
    let proc = a_process();
    assert_eq!(
        call(&proc, ECLIPSE_GATEWAY_EXT + 0x3fff, [0; 6]),
        zx(ZxError::NOT_SUPPORTED)
    );
}

// ---- eclipse_handle_to_fd ---------------------------------------------

#[test]
fn handle_to_fd_moves_the_handle_into_a_new_fd() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let fd = call(&proc, ECLIPSE_HANDLE_TO_FD, [value as usize, 0, 0, 0, 0, 0]);
    assert!(fd >= 0, "handle_to_fd answered {}", fd);
    assert_eq!(
        proc.get_object::<Channel>(value).err(),
        Some(ZxError::BAD_HANDLE),
        "the handle is the fd's now, not the table's"
    );
    let file = proc
        .linux()
        .get_file_like(FileDesc::from(fd as usize))
        .expect("the new fd is open");
    let holder = file.downcast_arc::<HandleFd>().ok().expect("a HandleFd");
    let back = holder.take().expect("the fd holds the handle");
    let channel = back
        .object
        .clone()
        .downcast_arc::<Channel>()
        .ok()
        .expect("the channel");
    assert_eq!(channel.peer().ok().map(|p| p.id()), Some(peer.id()));
}

#[test]
fn handle_to_fd_refuses_flags_other_than_cloexec_and_nonblock_and_keeps_the_handle() {
    let proc = a_process();
    let (value, _peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let bad = OpenFlags::APPEND.bits();
    assert_eq!(
        call(
            &proc,
            ECLIPSE_HANDLE_TO_FD,
            [value as usize, bad, 0, 0, 0, 0]
        ),
        zx(ZxError::INVALID_ARGS)
    );
    assert!(proc.get_object::<Channel>(value).is_ok());
    // And the two it takes are what the fd gets.
    let flags = (OpenFlags::CLOEXEC | OpenFlags::NON_BLOCK).bits();
    let fd = call(
        &proc,
        ECLIPSE_HANDLE_TO_FD,
        [value as usize, flags, 0, 0, 0, 0],
    );
    assert!(fd >= 0);
    assert!(proc
        .linux()
        .fd_cloexec(FileDesc::from(fd as usize))
        .expect("the fd is open"));
}

#[test]
fn handle_to_fd_without_the_transfer_right_leaves_the_handle_where_it_is() {
    let proc = a_process();
    let (value, _peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL - Rights::TRANSFER);
    assert_eq!(
        call(&proc, ECLIPSE_HANDLE_TO_FD, [value as usize, 0, 0, 0, 0, 0]),
        zx(ZxError::ACCESS_DENIED)
    );
    assert!(
        proc.get_object::<Channel>(value).is_ok(),
        "a handle that could not be moved was consumed"
    );
    // A value that names nothing.
    assert_eq!(
        call(&proc, ECLIPSE_HANDLE_TO_FD, [0xdead, 0, 0, 0, 0, 0]),
        zx(ZxError::BAD_HANDLE)
    );
}

// ---- eclipse_fd_to_handle ---------------------------------------------

/// A channel end with `rights` behind a new fd of `proc`.
fn a_handle_fd(proc: &Arc<Process>, rights: Rights) -> (usize, Arc<Channel>) {
    let (value, peer) = a_channel(proc, rights);
    let fd = call(proc, ECLIPSE_HANDLE_TO_FD, [value as usize, 0, 0, 0, 0, 0]);
    assert!(fd >= 0);
    (fd as usize, peer)
}

#[test]
fn fd_to_handle_with_take_empties_the_fd() {
    let proc = a_process();
    let (fd, peer) = a_handle_fd(&proc, Rights::DEFAULT_CHANNEL);
    let mut out = 0u32;
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [fd, ECLIPSE_FD_TAKE, ptr(&mut out), 0, 0, 0]
        ),
        0
    );
    let channel = proc.get_object::<Channel>(out).expect("the handle is back");
    assert_eq!(channel.peer().ok().map(|p| p.id()), Some(peer.id()));
    // The fd has nothing left to give.
    let mut again = 0u32;
    let second = call(
        &proc,
        ECLIPSE_FD_TO_HANDLE,
        [fd, ECLIPSE_FD_TAKE, ptr(&mut again), 0, 0, 0],
    );
    assert!(second < 0, "an emptied fd gave a second handle");
}

#[test]
fn fd_to_handle_without_take_duplicates_and_needs_the_duplicate_right() {
    let proc = a_process();
    // A channel's default rights leave DUPLICATE out, as Zircon's do.
    let (fd, _peer) = a_handle_fd(&proc, Rights::DEFAULT_CHANNEL);
    let mut out = 0u32;
    assert_eq!(
        call(&proc, ECLIPSE_FD_TO_HANDLE, [fd, 0, ptr(&mut out), 0, 0, 0]),
        zx(ZxError::ACCESS_DENIED)
    );
    let (fd, _peer) = a_handle_fd(&proc, Rights::DEFAULT_CHANNEL | Rights::DUPLICATE);
    assert_eq!(
        call(&proc, ECLIPSE_FD_TO_HANDLE, [fd, 0, ptr(&mut out), 0, 0, 0]),
        0
    );
    assert!(proc.get_object::<Channel>(out).is_ok());
    // A copy: the fd still has one to give.
    let mut taken = 0u32;
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [fd, ECLIPSE_FD_TAKE, ptr(&mut taken), 0, 0, 0]
        ),
        0
    );
    assert_ne!(taken, out);
}

#[test]
fn fd_to_handle_with_a_bad_out_pointer_does_not_cost_the_fd_its_handle() {
    let proc = a_process();
    let (fd, _peer) = a_handle_fd(&proc, Rights::DEFAULT_CHANNEL);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [fd, ECLIPSE_FD_TAKE, 0, 0, 0, 0]
        ),
        zx(ZxError::INVALID_ARGS)
    );
    let mut out = 0u32;
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [fd, ECLIPSE_FD_TAKE, ptr(&mut out), 0, 0, 0]
        ),
        0,
        "the handle went with the failed call"
    );
}

#[test]
fn fd_to_handle_refuses_unknown_flags_and_fds_that_hold_no_handle() {
    let proc = a_process();
    let (fd, _peer) = a_handle_fd(&proc, Rights::DEFAULT_CHANNEL);
    let mut out = 0u32;
    assert_eq!(
        call(&proc, ECLIPSE_FD_TO_HANDLE, [fd, 2, ptr(&mut out), 0, 0, 0]),
        zx(ZxError::INVALID_ARGS)
    );
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [999, 0, ptr(&mut out), 0, 0, 0]
        ),
        zx(ZxError::BAD_HANDLE)
    );
    let (read, _write) = linux_object::fs::Pipe::create_pair();
    let read =
        linux_object::fs::File::new(Arc::new(read), OpenFlags::RDONLY, String::from("pipe_r:[]"));
    let pipe = proc.linux().add_file(read).expect("a pipe fd");
    assert_eq!(
        call(
            &proc,
            ECLIPSE_FD_TO_HANDLE,
            [usize::from(pipe), 0, ptr(&mut out), 0, 0, 0]
        ),
        zx(ZxError::WRONG_TYPE)
    );
}

// ---- eclipse_redirect_syscalls ----------------------------------------

/// A Linux syscall number that may never be redirected (`exit` on this
/// architecture), and one that may.
fn kept_and_redirectable() -> (u32, u32) {
    let kept = (0..512)
        .find(|&n| linux_syscall::never_redirected(n))
        .expect("exit is never redirected");
    let free = (0..512)
        .find(|&n| !linux_syscall::never_redirected(n))
        .expect("some syscall is");
    (kept, free)
}

fn mask_of(nums: &[u32]) -> [u64; 8] {
    let mut mask = [0u64; 8];
    for &n in nums {
        mask[n as usize / 64] |= 1 << (n % 64);
    }
    mask
}

#[test]
fn a_redirect_takes_the_channel_and_happens_once() {
    let proc = a_process();
    let (_, free) = kept_and_redirectable();
    let mut mask = mask_of(&[free]);
    let (value, _server) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_REDIRECT_SYSCALLS,
            [value as usize, ptr(&mut mask), 0, 0, 0, 0]
        ),
        0
    );
    assert!(
        proc.get_object::<Channel>(value).is_err(),
        "the process can still reach its end of the redirect"
    );
    let redirect = proc.linux().syscall_redirect().get().cloned().expect("set");
    assert!(redirect.covers(free as usize));
    // A second one is refused, and its channel stays with the caller.
    let (other, _server) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_REDIRECT_SYSCALLS,
            [other as usize, ptr(&mut mask), 0, 0, 0, 0]
        ),
        zx(ZxError::ALREADY_BOUND)
    );
    assert!(proc.get_object::<Channel>(other).is_ok());
}

#[test]
fn exit_and_sigreturn_cannot_be_redirected() {
    let proc = a_process();
    let (kept, free) = kept_and_redirectable();
    let mut mask = mask_of(&[free, kept]);
    let (value, _server) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_REDIRECT_SYSCALLS,
            [value as usize, ptr(&mut mask), 0, 0, 0, 0]
        ),
        zx(ZxError::INVALID_ARGS)
    );
    assert!(proc.linux().syscall_redirect().get().is_none());
    assert!(proc.get_object::<Channel>(value).is_ok());
}

#[test]
fn a_redirect_needs_read_write_and_transfer_and_a_readable_mask() {
    let proc = a_process();
    let (_, free) = kept_and_redirectable();
    let mut mask = mask_of(&[free]);
    for missing in [Rights::READ, Rights::WRITE, Rights::TRANSFER] {
        let (value, _server) = a_channel(&proc, Rights::DEFAULT_CHANNEL - missing);
        assert_eq!(
            call(
                &proc,
                ECLIPSE_REDIRECT_SYSCALLS,
                [value as usize, ptr(&mut mask), 0, 0, 0, 0]
            ),
            zx(ZxError::ACCESS_DENIED),
            "a channel without {:?}",
            missing
        );
        assert!(proc.get_object::<Channel>(value).is_ok());
    }
    let (value, _server) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    assert_eq!(
        call(
            &proc,
            ECLIPSE_REDIRECT_SYSCALLS,
            [value as usize, 0, 0, 0, 0, 0]
        ),
        zx(ZxError::INVALID_ARGS)
    );
    assert!(proc.linux().syscall_redirect().get().is_none());
}

// ---- eclipse_redirect_memory ------------------------------------------

#[test]
fn a_server_reaches_only_the_memory_of_the_clients_it_serves() {
    let server = a_process();
    let (server_end, _) = a_channel(&server, Rights::DEFAULT_CHANNEL);
    let stranger = a_process();
    let mut buf = [0u8; 16];
    let mut other = 0u8;
    // A process that never redirected anything to this server.
    assert_eq!(
        call(
            &server,
            ECLIPSE_REDIRECT_MEMORY,
            [
                server_end as usize,
                stranger.id() as usize,
                ptr(&mut other),
                ptr(&mut buf),
                16,
                0
            ]
        ),
        zx(ZxError::ACCESS_DENIED)
    );
    // No such process.
    assert_eq!(
        call(
            &server,
            ECLIPSE_REDIRECT_MEMORY,
            [
                server_end as usize,
                0x7fff_ffff,
                ptr(&mut other),
                ptr(&mut buf),
                16,
                0
            ]
        ),
        zx(ZxError::NOT_FOUND)
    );
    // Flags it does not know, before anything else.
    assert_eq!(
        call(
            &server,
            ECLIPSE_REDIRECT_MEMORY,
            [server_end as usize, stranger.id() as usize, 0, 0, 0, 2]
        ),
        zx(ZxError::INVALID_ARGS)
    );
    assert_eq!(buf, [0; 16], "a refused copy wrote to the server");
}

#[test]
fn the_client_of_a_redirect_is_the_one_whose_channel_peer_the_server_holds() {
    let client = a_process();
    let server = a_process();
    let (_, free) = kept_and_redirectable();
    let mut mask = mask_of(&[free]);
    let (client_end, server_end) = Channel::create();
    let value = client.add_handle(Handle::new(client_end, Rights::DEFAULT_CHANNEL));
    assert_eq!(
        call(
            &client,
            ECLIPSE_REDIRECT_SYSCALLS,
            [value as usize, ptr(&mut mask), 0, 0, 0, 0]
        ),
        0
    );
    let served = server.add_handle(Handle::new(server_end.clone(), Rights::DEFAULT_CHANNEL));
    // The server's handle needs READ and WRITE.
    let weak = server.add_handle(Handle::new(server_end, Rights::READ));
    let mut buf = [0u8; 8];
    assert_eq!(
        call(
            &server,
            ECLIPSE_REDIRECT_MEMORY,
            [
                weak as usize,
                client.id() as usize,
                0x1000,
                ptr(&mut buf),
                8,
                0
            ]
        ),
        zx(ZxError::ACCESS_DENIED)
    );
    // With them, the client is found; what is copied is up to its address
    // space, which has nothing mapped at 0x1000: no bytes, and no error.
    assert_eq!(
        call(
            &server,
            ECLIPSE_REDIRECT_MEMORY,
            [
                served as usize,
                client.id() as usize,
                0x1000,
                ptr(&mut buf),
                8,
                0
            ]
        ),
        0
    );
}

// ---- eclipse_channel_reply_wait, up to the wait -------------------------

#[test]
fn reply_wait_sends_nothing_when_actual_is_a_bad_pointer() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut reply = *b"hello";
    let args = [value as usize, ptr(&mut reply), 5, 0, 0, 0];
    assert_eq!(reply_then(&proc, &args).err(), Some(ZxError::INVALID_ARGS));
    assert_eq!(
        peer.read().err(),
        Some(ZxError::SHOULD_WAIT),
        "a reply went out ahead of a call that could not finish"
    );
}

#[test]
fn reply_wait_refuses_a_reply_bigger_than_a_channel_message() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut actual = 7u32;
    let args = [
        value as usize,
        0x1000,
        CHANNEL_MAX_MSG_BYTES + 1,
        0,
        0,
        ptr(&mut actual),
    ];
    assert_eq!(reply_then(&proc, &args).err(), Some(ZxError::OUT_OF_RANGE));
    assert_eq!(peer.read().err(), Some(ZxError::SHOULD_WAIT));
}

#[test]
fn reply_wait_writes_the_reply_and_needs_read_and_write() {
    let proc = a_process();
    let mut reply = *b"pong";
    let mut actual = 7u32;
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let args = [value as usize, ptr(&mut reply), 4, 0, 0, ptr(&mut actual)];
    assert!(reply_then(&proc, &args).is_ok());
    assert_eq!(peer.read().expect("the reply").data, b"pong");
    assert_eq!(actual, 0, "actual is cleared before anything is sent");
    // No reply at all with a length of 0.
    assert!(reply_then(&proc, &[value as usize, 0, 0, 0, 0, ptr(&mut actual)]).is_ok());
    assert_eq!(peer.read().err(), Some(ZxError::SHOULD_WAIT));
    let (weak, _) = a_channel(&proc, Rights::DEFAULT_CHANNEL - Rights::WRITE);
    let args = [weak as usize, ptr(&mut reply), 4, 0, 0, ptr(&mut actual)];
    assert_eq!(reply_then(&proc, &args).err(), Some(ZxError::ACCESS_DENIED));
}

#[async_std::test]
async fn reply_wait_reads_the_next_message_into_the_buffer() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut buf = [0u8; 8];
    let mut actual = 0u32;
    let args = [value as usize, 0, 0, ptr(&mut buf), 8, ptr(&mut actual)];
    let channel = reply_then(&proc, &args).expect("nothing to send");
    peer.write(MessagePacket {
        data: b"ping".to_vec(),
        handles: Vec::new(),
    })
    .unwrap();
    assert_eq!(wait_then_read(channel, args).await, Ok(()));
    assert_eq!(actual, 4);
    assert_eq!(&buf[..4], b"ping");
}

#[async_std::test]
async fn reply_wait_leaves_a_message_too_big_for_the_buffer_queued() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut buf = [0u8; 2];
    let mut actual = 0u32;
    let args = [value as usize, 0, 0, ptr(&mut buf), 2, ptr(&mut actual)];
    let channel = reply_then(&proc, &args).unwrap();
    peer.write(MessagePacket {
        data: b"ping".to_vec(),
        handles: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        wait_then_read(channel.clone(), args).await,
        Err(ZxError::BUFFER_TOO_SMALL)
    );
    assert_eq!(actual, 4, "actual says how big a buffer it takes");
    assert_eq!(buf, [0; 2]);
    // Still there for a buffer that fits.
    let mut big = [0u8; 8];
    let args = [value as usize, 0, 0, ptr(&mut big), 8, ptr(&mut actual)];
    assert_eq!(wait_then_read(channel, args).await, Ok(()));
    assert_eq!(&big[..4], b"ping");
}

#[async_std::test]
async fn reply_wait_answers_peer_closed_once_the_queue_is_empty() {
    let proc = a_process();
    let (value, peer) = a_channel(&proc, Rights::DEFAULT_CHANNEL);
    let mut buf = [0u8; 8];
    let mut actual = 0u32;
    let args = [value as usize, 0, 0, ptr(&mut buf), 8, ptr(&mut actual)];
    let channel = reply_then(&proc, &args).unwrap();
    peer.write(MessagePacket {
        data: b"last".to_vec(),
        handles: Vec::new(),
    })
    .unwrap();
    drop(peer);
    assert_eq!(wait_then_read(channel.clone(), args).await, Ok(()));
    assert_eq!(&buf[..4], b"last");
    assert_eq!(
        wait_then_read(channel, args).await,
        Err(ZxError::PEER_CLOSED)
    );
}

// ---- eclipse_device_claim -----------------------------------------------

/// A register window the "device" answers through: the kernel resets a
/// virtio-mmio one by writing 0 to `Status` (offset 0x70) and reading it
/// back.
fn a_window() -> usize {
    let regs: &'static mut [u32; 64] = alloc::boxed::Box::leak(alloc::boxed::Box::new([0; 64]));
    regs[0x70 / 4] = 0xf; // DRIVER_OK and friends: a device that runs
    regs.as_mut_ptr() as usize
}

fn status_of(window: usize) -> u32 {
    // SAFETY: `a_window` leaked it; nothing else writes it but the reset.
    unsafe { ((window + 0x70) as *const u32).read_volatile() }
}

/// Reserve a virtio block device at `paddr` the way a bus does, and answer
/// its index among the reserved ones. The bus counts devices across every
/// test, so the index is looked up by the address.
fn a_reserved_disk(paddr: usize, size: usize, transport: Transport, vaddr: Option<usize>) -> usize {
    assert!(udev::reserve_virtio_blk(
        transport,
        paddr,
        size,
        Wiring { irq: None, vaddr }
    ));
    (0..4096)
        .find(|&i| udev::find(udev::VIRTIO_BLK, i).is_some_and(|d| d.paddr == paddr))
        .expect("the disk just reserved")
}

/// A physical address no other test reserves.
fn a_paddr() -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0x9000_0000);
    NEXT.fetch_add(0x10_0000, Ordering::Relaxed)
}

fn claim(proc: &Arc<Process>, kind: usize, index: usize, out: usize) -> isize {
    call(proc, ECLIPSE_DEVICE_CLAIM, [kind, index, out, 0, 0, 0])
}

#[test]
fn a_claim_hands_over_the_registers_a_bti_and_where_the_window_is() {
    let proc = a_process();
    // A window that does not start on a page, as QEMU's virtio-mmio ones
    // do not on aarch64.
    let paddr = a_paddr() + 0x200;
    let index = a_reserved_disk(paddr, 0x200, Transport::VirtioMmio, Some(a_window()));
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&proc, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
    let (regs, bti) = (words[0] as u32, (words[0] >> 32) as u32);
    let vmo = proc
        .get_object::<VmObject>(regs)
        .expect("word 0 low is a VMO");
    assert_eq!(vmo.len(), PAGE_SIZE, "the one page holding the window");
    assert!(proc.get_object::<BusTransactionInitiator>(bti).is_ok());
    assert_eq!(words[1] as u32 as u64, ECLIPSE_TRANSPORT_VIRTIO_MMIO);
    assert_eq!(words[1] >> 32, u32::MAX as u64, "no line known");
    assert_eq!(words[2], 0x200, "where the window starts in the VMO");
    assert_eq!(words[3], 0x200);
    assert_eq!(
        &words[4..7],
        &[0, 0, 0],
        "virtio-mmio needs nothing besides"
    );
    assert_eq!(words[7], 0, "no interrupt to give");
    assert_eq!(device_claim::held_by(&proc, udev::VIRTIO_BLK), Some(index));
}

#[test]
fn a_virtio_pci_claim_says_where_the_four_structures_are() {
    let proc = a_process();
    let paddr = a_paddr();
    let transport = Transport::VirtioPci {
        common: 0x0,
        notify: 0x3000,
        isr: 0x1000,
        device: 0x2000,
        notify_multiplier: 4,
    };
    let index = a_reserved_disk(paddr, 0x4000, transport, None);
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&proc, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
    assert_eq!(words[1] as u32 as u64, ECLIPSE_TRANSPORT_VIRTIO_PCI);
    assert_eq!(words[4], 0x3000 << 32, "common low, notify high");
    assert_eq!(words[5], 0x1000 | 0x2000 << 32, "isr low, device high");
    assert_eq!(words[6], 4);
    let vmo = proc.get_object::<VmObject>(words[0] as u32).unwrap();
    assert_eq!(vmo.len(), 0x4000);
}

#[test]
fn a_device_has_one_driver_at_a_time() {
    let first = a_process();
    let second = a_process();
    let index = a_reserved_disk(a_paddr(), 0x200, Transport::VirtioMmio, Some(a_window()));
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&first, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
    let mut theirs = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&second, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut theirs)),
        zx(ZxError::ALREADY_BOUND)
    );
    assert_eq!(device_claim::held_by(&second, udev::VIRTIO_BLK), None);
}

#[test]
fn a_bad_claim_pointer_does_not_cost_the_device() {
    let proc = a_process();
    let index = a_reserved_disk(a_paddr(), 0x200, Transport::VirtioMmio, Some(a_window()));
    assert_eq!(
        claim(&proc, ECLIPSE_DEVICE_VIRTIO_BLK, index, 0),
        zx(ZxError::INVALID_ARGS)
    );
    // A kernel address, as bare metal sees one.
    kernel_hal::user::set_strict_user_half(true);
    let kernel = claim(
        &proc,
        ECLIPSE_DEVICE_VIRTIO_BLK,
        index,
        0xffff_8000_0000_1000,
    );
    kernel_hal::user::set_strict_user_half(false);
    assert_eq!(kernel, zx(ZxError::INVALID_ARGS));
    let device = udev::find(udev::VIRTIO_BLK, index).unwrap();
    assert!(
        !device.is_claimed(),
        "a claim that wrote nothing took the device"
    );
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&proc, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
}

#[test]
fn a_claim_of_an_unknown_kind_or_a_device_not_reserved_is_refused() {
    let proc = a_process();
    let mut words = [0xaau64; CLAIM_WORDS];
    for kind in [0, 12, usize::MAX] {
        assert_eq!(
            claim(&proc, kind, 0, ptr(&mut words)),
            zx(ZxError::INVALID_ARGS)
        );
    }
    assert_eq!(
        words, [0xaa; CLAIM_WORDS],
        "an unknown kind is refused first"
    );
    assert_eq!(
        claim(&proc, ECLIPSE_DEVICE_VIRTIO_BLK, 100_000, ptr(&mut words)),
        zx(ZxError::NOT_FOUND)
    );
    // A PCI identity is 32 bits.
    let args = [
        device_claim::ECLIPSE_DEVICE_PCI,
        0,
        ptr(&mut words),
        1 << 40,
        0,
        0,
    ];
    assert_eq!(
        call(&proc, ECLIPSE_DEVICE_CLAIM, args),
        zx(ZxError::INVALID_ARGS)
    );
    let args = [
        device_claim::ECLIPSE_DEVICE_PCI,
        0,
        ptr(&mut words),
        0x1234_5678,
        0,
        0,
    ];
    assert_eq!(
        call(&proc, ECLIPSE_DEVICE_CLAIM, args),
        zx(ZxError::NOT_FOUND)
    );
}

/// Wait, a while at most, for `done`.
pub(super) fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..2000 {
        if done() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("{}: still not so after two seconds", what);
}

#[test]
fn when_its_driver_dies_the_device_is_reset_and_free_to_be_claimed_again() {
    let window = a_window();
    let index = a_reserved_disk(a_paddr(), 0x200, Transport::VirtioMmio, Some(window));
    let driver = a_process();
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&driver, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
    assert_eq!(
        status_of(window),
        0xf,
        "the claim itself leaves the device alone"
    );
    driver.exit(0);
    let device = udev::find(udev::VIRTIO_BLK, index).unwrap();
    eventually("the device is released", || !device.is_claimed());
    assert_eq!(status_of(window), 0, "released without being reset");
    let next = a_process();
    assert_eq!(
        claim(&next, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
}

#[test]
fn a_device_the_kernel_cannot_reset_stays_out_of_use_after_its_driver() {
    // Not mapped in the kernel: there is no way to stop it, and it may
    // still be doing DMA into what the dead driver pinned.
    let index = a_reserved_disk(a_paddr(), 0x200, Transport::VirtioMmio, None);
    let driver = a_process();
    let mut words = [0u64; CLAIM_WORDS];
    assert_eq!(
        claim(&driver, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        0
    );
    driver.exit(0);
    eventually("the dead driver no longer holds it", || {
        device_claim::held_by(&driver, udev::VIRTIO_BLK).is_none()
    });
    let device = udev::find(udev::VIRTIO_BLK, index).unwrap();
    assert!(
        device.is_claimed(),
        "a device that could not be reset was given out again"
    );
    let next = a_process();
    assert_eq!(
        claim(&next, ECLIPSE_DEVICE_VIRTIO_BLK, index, ptr(&mut words)),
        zx(ZxError::ALREADY_BOUND)
    );
}

// ---- the allowlist, by name -------------------------------------------

/// The generated table of Zircon numbers, `NAME = n,` per line.
fn zircon_numbers() -> Vec<(&'static str, u32)> {
    include_str!("../../../zircon-syscall/src/consts.rs")
        .lines()
        .filter_map(|line| {
            let (name, n) = line.trim().trim_end_matches(',').split_once(" = ")?;
            Some((name, n.parse().ok()?))
        })
        .collect()
}

#[test]
fn the_gateway_lets_through_exactly_the_syscalls_its_comment_names() {
    // The comments in `zircon_gateway_allows` name these; the numbers there
    // are bare, and a renumbered table would let something else through.
    let mut allowed: Vec<&str> = alloc::vec![
        "CHANNEL_CALL_ETC_NORETRY",
        "CHANNEL_CALL_NORETRY",
        "CHANNEL_CREATE",
        "CHANNEL_READ",
        "CHANNEL_READ_ETC",
        "CHANNEL_WRITE",
        "CHANNEL_WRITE_ETC",
        "EVENT_CREATE",
        "EVENTPAIR_CREATE",
        "FIFO_CREATE",
        "FIFO_READ",
        "FIFO_WRITE",
        "HANDLE_CLOSE",
        "HANDLE_CLOSE_MANY",
        "HANDLE_DUPLICATE",
        "HANDLE_REPLACE",
        "OBJECT_GET_INFO",
        "OBJECT_SIGNAL",
        "OBJECT_SIGNAL_PEER",
        "OBJECT_WAIT_ASYNC",
        "OBJECT_WAIT_MANY",
        "OBJECT_WAIT_ONE",
        "PORT_CANCEL",
        "PORT_CANCEL_KEY",
        "PORT_CREATE",
        "PORT_QUEUE",
        "PORT_WAIT",
        "SOCKET_CREATE",
        "SOCKET_READ",
        "SOCKET_SET_DISPOSITION",
        "SOCKET_WRITE",
        "SOCKET_SHUTDOWN",
        "TIMER_CANCEL",
        "TIMER_CREATE",
        "TIMER_SET",
        "VMO_CREATE",
        "VMO_CREATE_CHILD",
        "VMO_GET_SIZE",
        "VMO_READ",
        "VMO_SET_SIZE",
        "VMO_WRITE",
        "BTI_PIN",
        "BTI_RELEASE_QUARANTINE",
        "PMT_UNPIN",
        "VMO_CREATE_CONTIGUOUS",
        "INTERRUPT_ACK",
        "INTERRUPT_BIND",
        "INTERRUPT_DESTROY",
        "INTERRUPT_WAIT",
    ];
    allowed.sort_unstable();
    let table = zircon_numbers();
    assert!(
        table.len() > 150,
        "the table did not parse: {} rows",
        table.len()
    );
    let mut passed: Vec<&str> = table
        .iter()
        .filter(|(_, n)| zircon_gateway_allows(*n))
        .map(|(name, _)| *name)
        .collect();
    passed.sort_unstable();
    assert_eq!(passed, allowed);
    // Nor anything past the table but below the extensions.
    let known: Vec<u32> = table.iter().map(|(_, n)| *n).collect();
    for n in 0..ECLIPSE_GATEWAY_EXT {
        if !known.contains(&n) {
            assert!(
                !zircon_gateway_allows(n),
                "{} names no syscall and passes",
                n
            );
        }
    }
}
