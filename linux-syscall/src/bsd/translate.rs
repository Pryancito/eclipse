//! Say a FreeBSD program's flag words in Linux, or say that they cannot be
//! said.
//!
//! A FreeBSD binary passes FreeBSD flag bits; the `sys_*` methods this layer
//! reuses parse Linux flag bits. Handing the raw word through would silently
//! mean something else — FreeBSD `O_CREAT` is 0x0200, which on Linux is
//! `O_EXCL`, and FreeBSD `O_CLOEXEC` is 0x0010_0000, which on Linux is
//! nothing at all.
//!
//! What this module used to be was a whitelist: the bits it knew were
//! rewritten and the rest were dropped without a word. That is the same shape
//! as `bitflags::from_bits_truncate`, and it has the same consequence — the
//! kernel does a different thing from the one it was asked for and reports
//! success. A program that opens a file `O_EXLOCK` believed it held a lock; a
//! program that mapped `MAP_FIXED|MAP_EXCL` believed nothing could already be
//! at that address, which is the same "this address or nothing" that
//! `MAP_FIXED_NOREPLACE` exists for on the Linux side.
//!
//! So every bit of every word now lands in one of three piles:
//!
//! * **Translated** — Linux has a peer *and this kernel honours it*.
//! * **Ignorable** — a program cannot observe its absence. Named one at a
//!   time below with the reason, never as a catch-all.
//! * **Refused** — everything else. A flag FreeBSD defines and this kernel
//!   cannot carry out answers `EOPNOTSUPP`; a bit FreeBSD does not define at
//!   all answers `EINVAL`, which is what a FreeBSD kernel answers for it.
//!
//! The emphasis in the first pile is load-bearing. Translating a bit onto a
//! Linux bit that `OpenFlags` or `MmapFlags` does not name is the same silent
//! drop one layer down, and two of the rows here were exactly that: FreeBSD
//! `O_PATH` onto a Linux `O_PATH` that `OpenFlags` throws away, and FreeBSD
//! `MAP_STACK` onto a Linux `MAP_STACK` that `MmapFlags` throws away. The
//! tests at the bottom parse every bit this module can produce back through
//! those two types and demand that it survives.

use super::consts::{fcntl, lin_fcntl, lin_mman, lin_oflags, lin_wait, mman, oflags, wait};
use linux_object::error::{LxError, LxResult};

/// The only page size this kernel maps with, as a shift.
const PAGE_SHIFT: i32 = 12;

/// Rewrite the bits `map` names, drop the bits `ignored` names, and refuse
/// whatever is left.
///
/// `refused` is the flags this system understands and cannot carry out, which
/// is a different answer from a bit that is not a flag anywhere.
fn sift(word: i32, map: &[(i32, i32)], ignored: i32, refused: i32) -> LxResult<i32> {
    let mut out = 0;
    let mut known = ignored | refused;
    for &(from, to) in map {
        known |= from;
        if word & from != 0 {
            out |= to;
        }
    }
    if word & refused != 0 {
        // Understood, and not doable here. Answering `Ok` would be answering
        // that it was done.
        return Err(LxError::EOPNOTSUPP);
    }
    if word & !known != 0 {
        // Not a flag on either system.
        return Err(LxError::EINVAL);
    }
    Ok(out)
}

/// FreeBSD `open(2)` bits that have a Linux peer this kernel honours.
const OPEN_MAP: &[(i32, i32)] = &[
    (oflags::O_NONBLOCK, lin_oflags::O_NONBLOCK),
    (oflags::O_APPEND, lin_oflags::O_APPEND),
    (oflags::O_ASYNC, lin_oflags::O_ASYNC),
    // FreeBSD spells one flag `O_FSYNC` and `O_SYNC` alike, and it is the
    // strong one: data *and* metadata. Linux's `O_SYNC` is two bits, and only
    // the lower of them is `O_DSYNC`.
    (oflags::O_FSYNC, lin_oflags::O_SYNC),
    (oflags::O_NOFOLLOW, lin_oflags::O_NOFOLLOW),
    (oflags::O_CREAT, lin_oflags::O_CREAT),
    (oflags::O_TRUNC, lin_oflags::O_TRUNC),
    (oflags::O_EXCL, lin_oflags::O_EXCL),
    (oflags::O_NOCTTY, lin_oflags::O_NOCTTY),
    (oflags::O_DIRECT, lin_oflags::O_DIRECT),
    (oflags::O_DIRECTORY, lin_oflags::O_DIRECTORY),
    (oflags::O_CLOEXEC, lin_oflags::O_CLOEXEC),
    (oflags::O_DSYNC, lin_oflags::O_DSYNC),
];

/// `O_TTY_INIT` asks that a terminal be reset to the system default `termios`
/// as it is opened. On anything that is not a terminal FreeBSD documents it
/// as a no-op, and the programs that set it (`getty`, `login`) follow the
/// open with a `tcsetattr` of their own, so its absence is not visible.
const OPEN_IGNORED: i32 = oflags::O_TTY_INIT;

/// `open(2)` flags that promise something this kernel cannot deliver.
const OPEN_REFUSED: i32 =
    // A lock taken by the open itself. Linux has no open flag for it, and a
    // program that asked for one and did not get it goes on to write over
    // another writer without ever finding out.
    oflags::O_SHLOCK
    | oflags::O_EXLOCK
    // "Open for execute only": the descriptor may be handed to `fexecve` and
    // to nothing else. Dropping it asks for a readable descriptor to a file
    // the caller may only have execute permission on.
    | oflags::O_EXEC
    // "Open only after the contents have been verified" (`veriexec`). A
    // security promise, so silence is the wrong answer.
    | oflags::O_VERIFY
    // A descriptor that names a file without opening it: no reads, no writes.
    // `OpenFlags` in `linux-object` does not name Linux's `O_PATH`, so
    // translating this bit would drop it one layer down and hand back a
    // descriptor the program can read from.
    | oflags::O_PATH
    // Refuse to resolve a name outside the starting directory. It is a
    // sandbox; dropping it takes the sandbox away and says nothing.
    | oflags::O_RESOLVE_BENEATH
    // An empty path means the directory descriptor itself. Linux spells that
    // `AT_EMPTY_PATH` on the `*at` calls and has no `open` flag for it, so
    // `openat(fd, "", O_EMPTY_PATH)` would become `openat(fd, "")` — ENOENT
    // where the program expected a second descriptor onto `fd`.
    | oflags::O_EMPTY_PATH
    // NFSv4 named attributes. There is no such namespace here.
    | oflags::O_NAMEDATTR
    // Close on `fork`, not on `exec`. Nothing in this kernel closes anything
    // at `fork`.
    | oflags::O_CLOFORK;

/// Translate FreeBSD `open(2)`/`openat(2)` flags to the Linux flag word that
/// [`crate::Syscall::sys_openat`] understands.
///
/// The access mode lives in the low two bits and is identical on both systems
/// (`O_RDONLY`=0, `O_WRONLY`=1, `O_RDWR`=2), so it is copied straight across;
/// every other bit is sorted into one of the three piles.
pub fn open_flags_to_linux(bsd: i32) -> LxResult<i32> {
    let mode = bsd & oflags::O_ACCMODE;
    // 3 is not an access mode on either system: FreeBSD spells "execute only"
    // with `O_EXEC`, and Linux rejects `O_WRONLY|O_RDWR` outright.
    if mode == oflags::O_ACCMODE {
        return Err(LxError::EINVAL);
    }
    let rest = sift(
        bsd & !oflags::O_ACCMODE,
        OPEN_MAP,
        OPEN_IGNORED,
        OPEN_REFUSED,
    )?;
    Ok(rest | mode)
}

/// FreeBSD `*at` bits that have a Linux peer.
///
/// Linux spells `AT_EACCESS` and `AT_REMOVEDIR` with the same bit (0x200) and
/// FreeBSD does not, so two rows here really do land on one value. No call
/// takes both, which is why Linux can afford it.
const AT_MAP: &[(i32, i32)] = &[
    (oflags::AT_EACCESS, lin_oflags::AT_EACCESS),
    (oflags::AT_SYMLINK_NOFOLLOW, lin_oflags::AT_SYMLINK_NOFOLLOW),
    (oflags::AT_SYMLINK_FOLLOW, lin_oflags::AT_SYMLINK_FOLLOW),
    (oflags::AT_REMOVEDIR, lin_oflags::AT_REMOVEDIR),
    (oflags::AT_EMPTY_PATH, lin_oflags::AT_EMPTY_PATH),
];

/// `AT_RESOLVE_BENEATH` is `O_RESOLVE_BENEATH` on the `*at` calls, and is
/// refused for the same reason: it is a sandbox.
const AT_REFUSED: i32 = oflags::AT_RESOLVE_BENEATH;

/// Translate FreeBSD `*at` flags (the `AT_*` word) to Linux.
///
/// Which `AT_*` bits a given call accepts is the Linux side's business and it
/// checks them; this only decides what each bit is called over there.
pub fn at_flags_to_linux(bsd: i32) -> LxResult<i32> {
    sift(bsd, AT_MAP, 0, AT_REFUSED)
}

/// FreeBSD `mmap(2)` bits that have a Linux peer this kernel honours.
const MMAP_MAP: &[(i32, i32)] = &[
    (mman::MAP_SHARED, lin_mman::MAP_SHARED),
    (mman::MAP_PRIVATE, lin_mman::MAP_PRIVATE),
    (mman::MAP_FIXED, lin_mman::MAP_FIXED),
    (mman::MAP_ANON, lin_mman::MAP_ANONYMOUS),
    // FreeBSD's `MAP_STACK` *implies* `MAP_ANON|MAP_PRIVATE` — `vm_mmap.c`
    // adds both, and the manual says the offset must be zero. Translating it
    // to Linux's `MAP_STACK`, which is a no-op there and a bit `MmapFlags`
    // does not even name, left a request carrying neither a backing nor a
    // sharing bit, with a file descriptor of -1. That is exactly the call
    // FreeBSD's libthr makes for every thread stack it allocates.
    (
        mman::MAP_STACK,
        lin_mman::MAP_PRIVATE | lin_mman::MAP_ANONYMOUS,
    ),
];

/// `mmap(2)` flags whose absence a program cannot see.
const MMAP_IGNORED: i32 =
    // "This region may contain semaphores." A hint left from a time when it
    // decided which pages could be wired; FreeBSD does nothing with it on any
    // architecture it still supports.
    mman::MAP_HASSEMAPHORE
    // "Page to the file but do not sync it." It changes when dirty pages
    // reach the backing store, not what any program reads back.
    | mman::MAP_NOSYNC
    // "Leave these pages out of a core dump." Nothing here writes core dumps.
    | mman::MAP_NOCORE
    // "Fault the mapping in for reading now." A hint about when the work is
    // done, and Linux's nearest thing (`MAP_POPULATE`) is a hint too.
    | mman::MAP_PREFAULT_READ;

/// `mmap(2)` flags that promise something this kernel cannot deliver.
const MMAP_REFUSED: i32 =
    // Reserve the range and fault on every access: a guard. Linux says it
    // with `PROT_NONE`, which is not in this word, so it cannot be answered
    // from here — and a guard page quietly turned into ordinary memory is a
    // stack that runs into the next one instead of faulting.
    mman::MAP_GUARD
    // "In the low 2 GiB of the address space." The programs that ask are the
    // ones that cannot address more than that. `MmapFlags` does not name it,
    // so the promise cannot be passed on.
    | mman::MAP_32BIT;

/// Translate FreeBSD `mmap(2)` flags to Linux. `PROT_*` are identical on both
/// systems and never pass through here — only the `MAP_*` word is remapped.
pub fn mmap_flags_to_linux(bsd: i32) -> LxResult<i32> {
    // `MAP_ALIGNED(n)` is a number in the top eight bits, not a flag: the
    // mapping must be aligned to 2^n.
    let align = (bsd & mman::MAP_ALIGNMENT_MASK) >> mman::MAP_ALIGNMENT_SHIFT;
    match align {
        // Nothing asked for.
        0 => {}
        // `MAP_ALIGNED_SUPER` is `MAP_ALIGNED(1)`, and the manual calls it a
        // request for a superpage "if it is possible", which is a hint.
        1 => {}
        // Every mapping this kernel hands out starts on a page boundary, so
        // anything up to a page is already true of it.
        n if n <= PAGE_SHIFT => {}
        // Larger is a request, and not one this layer can pass on.
        _ => return Err(LxError::EOPNOTSUPP),
    }

    // `MAP_EXCL` only means anything beside `MAP_FIXED`; on its own it is an
    // error on FreeBSD too. Together the two are "this address or `EEXIST`",
    // which is what `MAP_FIXED_NOREPLACE` was added to Linux for.
    let excl = if bsd & mman::MAP_EXCL != 0 {
        if bsd & mman::MAP_FIXED == 0 {
            return Err(LxError::EINVAL);
        }
        lin_mman::MAP_FIXED_NOREPLACE
    } else {
        0
    };

    let word = bsd & !(mman::MAP_ALIGNMENT_MASK | mman::MAP_EXCL);
    let out = sift(word, MMAP_MAP, MMAP_IGNORED, MMAP_REFUSED)?;

    // Exactly one of shared and private, on both systems (`vm_mmap.c`
    // switches on the pair and its `default` is `EINVAL`). `MAP_STACK` counts
    // as private, because it brings private with it.
    let backing = out & (lin_mman::MAP_SHARED | lin_mman::MAP_PRIVATE);
    if backing != lin_mman::MAP_SHARED && backing != lin_mman::MAP_PRIVATE {
        return Err(LxError::EINVAL);
    }

    Ok(out | excl)
}

/// The status flags `F_SETFL` may change and `F_GETFL` reports, in both
/// spellings. `kern_fcntl` masks its argument to these and ignores the rest,
/// so an `F_SETFL` word is never refused for a bit outside them.
const STATUS_MAP: &[(i32, i32)] = &[
    (oflags::O_NONBLOCK, lin_oflags::O_NONBLOCK),
    (oflags::O_APPEND, lin_oflags::O_APPEND),
    (oflags::O_ASYNC, lin_oflags::O_ASYNC),
    (oflags::O_FSYNC, lin_oflags::O_SYNC),
    (oflags::O_DIRECT, lin_oflags::O_DIRECT),
    (oflags::O_DSYNC, lin_oflags::O_DSYNC),
];

/// The `F_SETFL` argument of a FreeBSD program, in Linux's spelling.
///
/// It used to go through untouched, and the two systems put the flags this
/// command exists for on different bits: FreeBSD's `O_NONBLOCK` is `0x4`,
/// which Linux does not use, so `fcntl(fd, F_SETFL, O_NONBLOCK)` from a
/// FreeBSD binary left the descriptor blocking and answered 0.
pub fn setfl_flags_to_linux(bsd: i32) -> i32 {
    STATUS_MAP
        .iter()
        .filter(|(from, _)| bsd & from != 0)
        .fold(0, |out, (_, to)| out | to)
}

/// The word `F_GETFL` hands a FreeBSD program: the access mode, and the
/// status flags in FreeBSD's spelling.
///
/// A Linux answer read with FreeBSD's headers was a lie bit for bit: Linux
/// `O_APPEND` (`0x400`) is FreeBSD's `O_TRUNC`, and Linux `O_NONBLOCK`
/// (`0x800`) is FreeBSD's `O_EXCL`, so a program asking whether its socket
/// was non-blocking was told about flags that do not exist on an open file.
/// A Linux bit with no peer here is dropped: FreeBSD's headers cannot name it.
pub fn open_flags_from_linux(lin: i32) -> i32 {
    let mut out = lin & oflags::O_ACCMODE;
    for &(bsd, l) in STATUS_MAP {
        // Linux `O_SYNC` is two bits, one of them `O_DSYNC`; FreeBSD reports
        // the strong one as `O_FSYNC` alone.
        if l == lin_oflags::O_DSYNC && lin & lin_oflags::O_SYNC == lin_oflags::O_SYNC {
            continue;
        }
        if lin & l == l {
            out |= bsd;
        }
    }
    out
}

/// What a FreeBSD `fcntl(2)` becomes here.
#[derive(Debug, PartialEq, Eq)]
pub enum Fcntl {
    /// The command in Linux's numbering, with its argument in Linux's
    /// spelling.
    Linux {
        /// A `lin_fcntl` command.
        cmd: usize,
        /// Its argument.
        arg: usize,
    },
    /// `F_DUP2FD` and `F_DUP2FD_CLOEXEC`: `dup2` and `dup3` by another name.
    Dup2 {
        /// The descriptor to land on.
        target: usize,
        /// Whether the new descriptor is close-on-exec.
        cloexec: bool,
    },
}

/// Translate a FreeBSD `fcntl(2)` command and argument.
///
/// The commands used to go through with their FreeBSD numbers: the five
/// lowest happen to agree with Linux, and past them `F_DUPFD_CLOEXEC` (17)
/// landed on Linux's `F_SETLEASE`, `F_DUP2FD` (10) on `F_SETSIG`, and the
/// record locks (11-13) on `F_GETSIG`/`F_SETSIG`/nothing, all of which this
/// kernel refuses with `EINVAL`. `F_SETFL` went through with FreeBSD's flag
/// bits (see [`setfl_flags_to_linux`]).
///
/// The record locks are still `EINVAL`: FreeBSD's `struct flock` lays its
/// fields out differently from Linux's (`l_start` first, `l_type` fifth), so
/// passing the pointer through would read a lock request that was never
/// made.
pub fn fcntl_to_linux(cmd: usize, arg: usize) -> LxResult<Fcntl> {
    let linux = |cmd, arg| Ok(Fcntl::Linux { cmd, arg });
    match cmd {
        fcntl::F_DUPFD => linux(lin_fcntl::F_DUPFD, arg),
        fcntl::F_GETFD => linux(lin_fcntl::F_GETFD, arg),
        // `FD_CLOEXEC` is 1 on both.
        fcntl::F_SETFD => linux(lin_fcntl::F_SETFD, arg),
        fcntl::F_GETFL => linux(lin_fcntl::F_GETFL, arg),
        fcntl::F_SETFL => linux(
            lin_fcntl::F_SETFL,
            setfl_flags_to_linux(arg as i32) as u32 as usize,
        ),
        fcntl::F_GETOWN => linux(lin_fcntl::F_GETOWN, arg),
        fcntl::F_SETOWN => linux(lin_fcntl::F_SETOWN, arg),
        fcntl::F_DUPFD_CLOEXEC => linux(lin_fcntl::F_DUPFD_CLOEXEC, arg),
        // The seal bits (`F_SEAL_SEAL`, `_SHRINK`, `_GROW`, `_WRITE`) are the
        // same four on both.
        fcntl::F_ADD_SEALS => linux(lin_fcntl::F_ADD_SEALS, arg),
        fcntl::F_GET_SEALS => linux(lin_fcntl::F_GET_SEALS, arg),
        fcntl::F_DUP2FD => Ok(Fcntl::Dup2 {
            target: arg,
            cloexec: false,
        }),
        fcntl::F_DUP2FD_CLOEXEC => Ok(Fcntl::Dup2 {
            target: arg,
            cloexec: true,
        }),
        _ => Err(LxError::EINVAL),
    }
}

/// FreeBSD `wait4(2)` option bits and their Linux peers.
const WAIT_MAP: &[(i32, i32)] = &[
    (wait::WNOHANG, lin_wait::WNOHANG),
    (wait::WUNTRACED, lin_wait::WUNTRACED),
    (wait::WCONTINUED, lin_wait::WCONTINUED),
    (wait::WNOWAIT, lin_wait::WNOWAIT),
    (wait::WLINUXCLONE, lin_wait::WCLONE),
];

/// `wait4` reports exits and traps whether or not it is asked to, on both
/// systems; Linux spells that by refusing `WEXITED` on `wait4` (it belongs
/// to `waitid`), FreeBSD by accepting it as the default it already is.
const WAIT_IGNORED: i32 = wait::WEXITED | wait::WTRAPPED;

/// Translate the options word of a FreeBSD `wait4(2)`.
///
/// It used to go through untouched, and past the two lowest bits the two
/// systems disagree: FreeBSD's `WCONTINUED` (4) is Linux's `WEXITED`, which
/// `wait4` refuses, so a FreeBSD shell asking to hear about resumed jobs
/// got `EINVAL`; and FreeBSD's `WNOWAIT` (8) is Linux's `WCONTINUED`, so a
/// program asking to look without reaping reaped.
pub fn wait_options_to_linux(bsd: i32) -> LxResult<i32> {
    sift(bsd, WAIT_MAP, WAIT_IGNORED, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::MmapFlags;
    use linux_object::fs::OpenFlags;

    // ---- access mode and the bits with a peer ---------------------------

    #[test]
    fn access_mode_is_preserved() {
        assert_eq!(open_flags_to_linux(oflags::O_RDONLY).unwrap() & 3, 0);
        assert_eq!(open_flags_to_linux(oflags::O_WRONLY).unwrap() & 3, 1);
        assert_eq!(open_flags_to_linux(oflags::O_RDWR).unwrap() & 3, 2);
    }

    #[test]
    fn a_mode_of_three_is_not_a_mode() {
        assert_eq!(open_flags_to_linux(oflags::O_ACCMODE), Err(LxError::EINVAL));
    }

    #[test]
    fn create_flags_land_on_the_right_linux_bits() {
        // FreeBSD O_CREAT|O_TRUNC|O_WRONLY -> Linux O_CREAT|O_TRUNC|O_WRONLY.
        let bsd = oflags::O_WRONLY | oflags::O_CREAT | oflags::O_TRUNC;
        let lin = open_flags_to_linux(bsd).unwrap();
        assert_eq!(lin & 3, 1);
        assert_ne!(lin & lin_oflags::O_CREAT, 0);
        assert_ne!(lin & lin_oflags::O_TRUNC, 0);
        // And it must NOT accidentally set the raw FreeBSD bit positions.
        assert_eq!(lin & lin_oflags::O_NONBLOCK, 0);
    }

    #[test]
    fn cloexec_is_remapped_not_passed_through() {
        // FreeBSD O_CLOEXEC is 0x0010_0000; Linux is 0x0008_0000. A pass-through
        // would leave CLOEXEC unset on the Linux side.
        let lin = open_flags_to_linux(oflags::O_CLOEXEC).unwrap();
        assert_eq!(lin, lin_oflags::O_CLOEXEC);
    }

    /// FreeBSD has one flag for "data and metadata", and it is the one a
    /// database or a mail spool opens with. Linux's is two bits.
    #[test]
    fn the_strong_sync_stays_the_strong_one() {
        let lin = open_flags_to_linux(oflags::O_FSYNC).unwrap();
        assert_eq!(lin & lin_oflags::O_SYNC, lin_oflags::O_SYNC);
        // O_SYNC contains O_DSYNC, so the weak one must not be all there is.
        assert_ne!(lin, lin_oflags::O_DSYNC);
        // And the weak one on its own stays weak.
        let weak = open_flags_to_linux(oflags::O_DSYNC).unwrap();
        assert_eq!(weak, lin_oflags::O_DSYNC);
    }

    /// `O_ASYNC` asks for SIGIO on the descriptor. It was being dropped, so
    /// the signal simply never arrived.
    #[test]
    fn async_and_the_rest_of_the_low_word_are_translated() {
        assert_eq!(
            open_flags_to_linux(oflags::O_ASYNC).unwrap(),
            lin_oflags::O_ASYNC
        );
        assert_eq!(
            open_flags_to_linux(oflags::O_NOCTTY).unwrap(),
            lin_oflags::O_NOCTTY
        );
        assert_eq!(
            open_flags_to_linux(oflags::O_DIRECT).unwrap(),
            lin_oflags::O_DIRECT
        );
    }

    // ---- the three piles -------------------------------------------------

    /// A promise this kernel cannot keep is an error, not a silence.
    #[test]
    fn a_lock_taken_by_the_open_is_refused() {
        assert_eq!(
            open_flags_to_linux(oflags::O_RDWR | oflags::O_EXLOCK),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(
            open_flags_to_linux(oflags::O_RDONLY | oflags::O_SHLOCK),
            Err(LxError::EOPNOTSUPP)
        );
    }

    #[test]
    fn a_sandbox_is_never_dropped_in_silence() {
        assert_eq!(
            open_flags_to_linux(oflags::O_RDONLY | oflags::O_RESOLVE_BENEATH),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(
            at_flags_to_linux(oflags::AT_RESOLVE_BENEATH),
            Err(LxError::EOPNOTSUPP)
        );
    }

    #[test]
    fn every_open_flag_freebsd_defines_gets_an_answer() {
        // Not a table: the point is that no bit falls off the end.
        const ALL: &[(i32, &str)] = &[
            (oflags::O_NONBLOCK, "O_NONBLOCK"),
            (oflags::O_APPEND, "O_APPEND"),
            (oflags::O_SHLOCK, "O_SHLOCK"),
            (oflags::O_EXLOCK, "O_EXLOCK"),
            (oflags::O_ASYNC, "O_ASYNC"),
            (oflags::O_FSYNC, "O_FSYNC"),
            (oflags::O_NOFOLLOW, "O_NOFOLLOW"),
            (oflags::O_CREAT, "O_CREAT"),
            (oflags::O_TRUNC, "O_TRUNC"),
            (oflags::O_EXCL, "O_EXCL"),
            (oflags::O_NOCTTY, "O_NOCTTY"),
            (oflags::O_DIRECT, "O_DIRECT"),
            (oflags::O_DIRECTORY, "O_DIRECTORY"),
            (oflags::O_EXEC, "O_EXEC"),
            (oflags::O_TTY_INIT, "O_TTY_INIT"),
            (oflags::O_CLOEXEC, "O_CLOEXEC"),
            (oflags::O_VERIFY, "O_VERIFY"),
            (oflags::O_PATH, "O_PATH"),
            (oflags::O_RESOLVE_BENEATH, "O_RESOLVE_BENEATH"),
            (oflags::O_DSYNC, "O_DSYNC"),
            (oflags::O_EMPTY_PATH, "O_EMPTY_PATH"),
            (oflags::O_NAMEDATTR, "O_NAMEDATTR"),
            (oflags::O_CLOFORK, "O_CLOFORK"),
        ];
        for &(bit, name) in ALL {
            // Translated, ignored or EOPNOTSUPP -- but never EINVAL, which is
            // this module saying it has never heard of the bit.
            assert_ne!(
                open_flags_to_linux(bit),
                Err(LxError::EINVAL),
                "{} is a flag FreeBSD defines",
                name
            );
        }
    }

    #[test]
    fn a_bit_that_is_not_a_flag_anywhere_is_einval() {
        // 0x1000 and 0x2000 are unassigned in FreeBSD's fcntl.h.
        assert_eq!(open_flags_to_linux(0x1000), Err(LxError::EINVAL));
        assert_eq!(open_flags_to_linux(1 << 30), Err(LxError::EINVAL));
    }

    #[test]
    fn a_hint_nobody_can_miss_is_dropped_without_an_error() {
        assert_eq!(open_flags_to_linux(oflags::O_TTY_INIT), Ok(0));
        let lin = mmap_flags_to_linux(mman::MAP_PRIVATE | mman::MAP_NOCORE).unwrap();
        assert_eq!(lin, lin_mman::MAP_PRIVATE);
    }

    // ---- *at -------------------------------------------------------------

    #[test]
    fn at_nofollow_moves_from_0x200_to_0x100() {
        assert_eq!(
            at_flags_to_linux(oflags::AT_SYMLINK_NOFOLLOW).unwrap(),
            lin_oflags::AT_SYMLINK_NOFOLLOW
        );
        // AT_SYMLINK_FOLLOW happens to share 0x400 on both.
        assert_eq!(
            at_flags_to_linux(oflags::AT_SYMLINK_FOLLOW).unwrap(),
            lin_oflags::AT_SYMLINK_FOLLOW
        );
    }

    /// Linux reuses 0x200 for both, so the two FreeBSD bits meet there. It is
    /// safe only because no call takes both of them.
    #[test]
    fn eaccess_and_removedir_meet_on_one_linux_bit() {
        assert_eq!(
            at_flags_to_linux(oflags::AT_EACCESS).unwrap(),
            at_flags_to_linux(oflags::AT_REMOVEDIR).unwrap()
        );
        assert_eq!(
            at_flags_to_linux(oflags::AT_EACCESS).unwrap(),
            lin_oflags::AT_EACCESS
        );
    }

    #[test]
    fn at_empty_path_is_translated_not_dropped() {
        assert_eq!(
            at_flags_to_linux(oflags::AT_EMPTY_PATH).unwrap(),
            lin_oflags::AT_EMPTY_PATH
        );
    }

    // ---- mmap ------------------------------------------------------------

    #[test]
    fn mmap_anon_moves_from_0x1000_to_0x20() {
        let lin = mmap_flags_to_linux(mman::MAP_PRIVATE | mman::MAP_ANON).unwrap();
        assert_ne!(lin & lin_mman::MAP_PRIVATE, 0);
        assert_ne!(lin & lin_mman::MAP_ANONYMOUS, 0);
        assert_eq!(lin & !(lin_mman::MAP_PRIVATE | lin_mman::MAP_ANONYMOUS), 0);
    }

    /// The headline. FreeBSD says "this address or fail" with two flags where
    /// Linux says it with one, and the one was being dropped: the mapping
    /// landed anyway, over whatever was already there, and `mmap` said yes.
    #[test]
    fn this_address_or_nothing_survives_the_translation() {
        let bsd = mman::MAP_PRIVATE | mman::MAP_ANON | mman::MAP_FIXED | mman::MAP_EXCL;
        let lin = mmap_flags_to_linux(bsd).unwrap();
        assert_ne!(lin & lin_mman::MAP_FIXED_NOREPLACE, 0);
        assert_ne!(lin & lin_mman::MAP_FIXED, 0);
        // Without MAP_EXCL it is the ordinary "put it here, replacing".
        let plain = mmap_flags_to_linux(bsd & !mman::MAP_EXCL).unwrap();
        assert_eq!(plain & lin_mman::MAP_FIXED_NOREPLACE, 0);
    }

    #[test]
    fn excl_without_fixed_is_an_error_here_as_it_is_there() {
        let bsd = mman::MAP_PRIVATE | mman::MAP_ANON | mman::MAP_EXCL;
        assert_eq!(mmap_flags_to_linux(bsd), Err(LxError::EINVAL));
    }

    /// The call libthr makes for every thread stack: `MAP_STACK` alone, with
    /// no backing flag, because on FreeBSD it carries anonymous and private
    /// with it.
    #[test]
    fn a_stack_brings_its_backing_with_it() {
        let lin = mmap_flags_to_linux(mman::MAP_STACK).unwrap();
        assert_ne!(lin & lin_mman::MAP_PRIVATE, 0, "a stack is private");
        assert_ne!(lin & lin_mman::MAP_ANONYMOUS, 0, "a stack has no file");
    }

    #[test]
    fn a_mapping_with_no_backing_or_with_both_is_refused() {
        assert_eq!(mmap_flags_to_linux(mman::MAP_ANON), Err(LxError::EINVAL));
        assert_eq!(
            mmap_flags_to_linux(mman::MAP_SHARED | mman::MAP_PRIVATE),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_guard_page_is_refused_rather_than_handed_back_as_memory() {
        assert_eq!(
            mmap_flags_to_linux(mman::MAP_PRIVATE | mman::MAP_ANON | mman::MAP_GUARD),
            Err(LxError::EOPNOTSUPP)
        );
        assert_eq!(
            mmap_flags_to_linux(mman::MAP_PRIVATE | mman::MAP_ANON | mman::MAP_32BIT),
            Err(LxError::EOPNOTSUPP)
        );
    }

    /// `MAP_ALIGNED(n)` is a number, not a flag. Anything a page already
    /// satisfies costs nothing; anything larger is a request.
    #[test]
    fn an_alignment_a_page_already_gives_is_free_and_a_bigger_one_is_not() {
        let base = mman::MAP_PRIVATE | mman::MAP_ANON;
        let aligned = |n: i32| base | (n << mman::MAP_ALIGNMENT_SHIFT);
        for n in 2..=PAGE_SHIFT {
            assert!(
                mmap_flags_to_linux(aligned(n)).is_ok(),
                "MAP_ALIGNED({})",
                n
            );
        }
        // MAP_ALIGNED_SUPER is MAP_ALIGNED(1) and the manual calls it a hint.
        assert!(mmap_flags_to_linux(aligned(1)).is_ok());
        // 2 MiB and 1 GiB are real requests.
        assert_eq!(mmap_flags_to_linux(aligned(21)), Err(LxError::EOPNOTSUPP));
        assert_eq!(mmap_flags_to_linux(aligned(30)), Err(LxError::EOPNOTSUPP));
    }

    // ---- the bits actually reach the kernel ------------------------------

    /// A translation onto a bit the kernel's own parse throws away is the
    /// same silent drop, one layer down. `OpenFlags::from_bits_truncate` is
    /// where an untranslated `O_PATH` used to disappear.
    #[test]
    fn every_open_bit_this_module_produces_survives_openflags() {
        let mut produced = 0usize;
        for &(from, _) in OPEN_MAP {
            produced |= open_flags_to_linux(from).unwrap() as usize;
        }
        // O_RDWR is in the low two bits and passes through untouched.
        produced |= oflags::O_RDWR as usize;
        let kept = OpenFlags::from_bits_truncate(produced).bits();
        assert_eq!(
            kept,
            produced,
            "OpenFlags drops {:#o} of the {:#o} this module produces",
            produced & !kept,
            produced
        );
    }

    #[test]
    fn every_mmap_bit_this_module_produces_survives_mmapflags() {
        // Read the targets straight off the table: a single row on its own
        // would trip the backing check before it got here.
        let mut produced = lin_mman::MAP_FIXED_NOREPLACE as usize;
        for &(_, to) in MMAP_MAP {
            produced |= to as usize;
        }
        let kept = MmapFlags::from_bits_truncate(produced).bits();
        assert_eq!(
            kept,
            produced,
            "MmapFlags drops {:#x} of the {:#x} this module produces",
            produced & !kept,
            produced
        );
    }

    #[test]
    fn setfl_puts_nonblock_and_append_on_linux_bits() {
        assert_eq!(
            setfl_flags_to_linux(oflags::O_NONBLOCK),
            lin_oflags::O_NONBLOCK
        );
        assert_eq!(setfl_flags_to_linux(oflags::O_APPEND), lin_oflags::O_APPEND);
        assert_eq!(setfl_flags_to_linux(oflags::O_FSYNC), lin_oflags::O_SYNC);
        // Bits `F_SETFL` does not change are dropped, not refused.
        assert_eq!(setfl_flags_to_linux(oflags::O_CREAT | oflags::O_RDWR), 0);
    }

    #[test]
    fn getfl_reads_back_in_freebsd_spelling() {
        let lin = lin_oflags::O_RDWR | lin_oflags::O_NONBLOCK | lin_oflags::O_APPEND;
        assert_eq!(
            open_flags_from_linux(lin),
            oflags::O_RDWR | oflags::O_NONBLOCK | oflags::O_APPEND
        );
        assert_eq!(open_flags_from_linux(lin_oflags::O_SYNC), oflags::O_FSYNC);
        assert_eq!(open_flags_from_linux(lin_oflags::O_DSYNC), oflags::O_DSYNC);
        // A Linux bit FreeBSD's headers cannot name is not handed over.
        assert_eq!(
            open_flags_from_linux(lin_oflags::O_CLOEXEC | lin_oflags::O_WRONLY),
            oflags::O_WRONLY
        );
        // And the pair are inverses over the status flags.
        let bsd = oflags::O_NONBLOCK | oflags::O_ASYNC | oflags::O_DIRECT;
        assert_eq!(open_flags_from_linux(setfl_flags_to_linux(bsd)), bsd);
    }

    #[test]
    fn fcntl_commands_land_on_their_linux_numbers() {
        let lin = |cmd, arg| Fcntl::Linux { cmd, arg };
        assert_eq!(fcntl_to_linux(fcntl::F_GETFD, 0), Ok(lin(1, 0)));
        assert_eq!(
            fcntl_to_linux(fcntl::F_DUPFD_CLOEXEC, 10),
            Ok(lin(lin_fcntl::F_DUPFD_CLOEXEC, 10))
        );
        assert_eq!(
            fcntl_to_linux(fcntl::F_GETOWN, 0),
            Ok(lin(lin_fcntl::F_GETOWN, 0))
        );
        assert_eq!(
            fcntl_to_linux(fcntl::F_SETFL, oflags::O_NONBLOCK as usize),
            Ok(lin(lin_fcntl::F_SETFL, lin_oflags::O_NONBLOCK as usize))
        );
        assert_eq!(
            fcntl_to_linux(fcntl::F_DUP2FD, 7),
            Ok(Fcntl::Dup2 {
                target: 7,
                cloexec: false
            })
        );
        assert_eq!(
            fcntl_to_linux(fcntl::F_DUP2FD_CLOEXEC, 7),
            Ok(Fcntl::Dup2 {
                target: 7,
                cloexec: true
            })
        );
        // Record locks and anything unknown are EINVAL, as FreeBSD answers
        // for a command it does not have.
        assert_eq!(fcntl_to_linux(fcntl::F_SETLK, 0), Err(LxError::EINVAL));
        assert_eq!(fcntl_to_linux(99, 0), Err(LxError::EINVAL));
    }

    #[test]
    fn wait_options_move_to_their_linux_bits() {
        assert_eq!(wait_options_to_linux(0), Ok(0));
        assert_eq!(
            wait_options_to_linux(wait::WNOHANG | wait::WUNTRACED),
            Ok(lin_wait::WNOHANG | lin_wait::WUNTRACED)
        );
        assert_eq!(
            wait_options_to_linux(wait::WCONTINUED),
            Ok(lin_wait::WCONTINUED)
        );
        assert_eq!(wait_options_to_linux(wait::WNOWAIT), Ok(lin_wait::WNOWAIT));
        // The default interests are accepted and add nothing.
        assert_eq!(wait_options_to_linux(wait::WEXITED | wait::WTRAPPED), Ok(0));
        assert_eq!(
            wait_options_to_linux(wait::WLINUXCLONE),
            Ok(lin_wait::WCLONE)
        );
        assert_eq!(wait_options_to_linux(0x40), Err(LxError::EINVAL));
    }
}
