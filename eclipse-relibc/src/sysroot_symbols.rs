//! Símbolos que Rust's std necesita de libc en modo rustc-dep-of-std.
//! Solo se compila cuando feature = "rustc-dep-of-std".
//!
//! Contiene: tipos extra, constantes y declaraciones extern "C"
//! que eclipse-relibc no exporta en modo sysroot por defecto.

use core::prelude::v1::derive;
use core::{
    clone::Clone,
    marker::Copy,
    option::Option,
};
use crate::types::*;

// ── Tipos adicionales ─────────────────────────────────────────────────────────

pub type uintptr_t = usize;
pub type intptr_t  = isize;
pub type sa_family_t = u16;

#[repr(C)]
pub struct DIR { _opaque: [u8; 0] }

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iovec {
    pub iov_base: *mut c_void,
    pub iov_len:  size_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct linger {
    pub l_onoff:  c_int,
    pub l_linger: c_int,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct pollfd {
    pub fd:      c_int,
    pub events:  c_short,
    pub revents: c_short,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr {
    pub sa_family: sa_family_t,
    pub sa_data:   [c_char; 14],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_storage {
    pub ss_family: sa_family_t,
    _pad: [u8; 126],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_un {
    pub sun_family: sa_family_t,
    pub sun_path:   [c_char; 108],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_in {
    pub sin_family: sa_family_t,
    pub sin_port:   u16,
    pub sin_addr:   in_addr,
    _pad: [u8; 8],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct in_addr {
    pub s_addr: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ip_mreq {
    pub imr_multiaddr: in_addr,
    pub imr_interface: in_addr,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ipv6_mreq {
    pub ipv6mr_multiaddr: crate::types::in6_addr,
    pub ipv6mr_interface: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct dirent {
    pub d_ino:    u64,
    pub d_off:    i64,
    pub d_reclen: u16,
    pub d_type:   u8,
    pub d_name:   [c_char; 256],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct addrinfo {
    pub ai_flags:     c_int,
    pub ai_family:    c_int,
    pub ai_socktype:  c_int,
    pub ai_protocol:  c_int,
    pub ai_addrlen:   socklen_t,
    pub ai_addr:      *mut sockaddr,
    pub ai_canonname: *mut c_char,
    pub ai_next:      *mut addrinfo,
}

// ── Módulo platform (libc expone platform::raw y platform::fs para std) ───────

pub mod platform {
    use super::*;
    pub mod raw {
        use super::*;
        pub use crate::types::stat;
        pub type mode_t  = super::mode_t;
        pub type ino_t   = super::ino_t;
        pub type dev_t   = super::dev_t;
        pub type nlink_t = super::c_uint;
        pub type blksize_t = super::c_long;
        pub type blkcnt_t  = super::c_long;
        pub type time_t    = super::time_t;
        pub type off_t     = super::off_t;
    }
    pub mod fs {
        pub use super::raw::*;
    }
}

// ── Constantes ────────────────────────────────────────────────────────────────

// fd / file status
pub const STDIN_FILENO:  c_int = 0;
pub const STDOUT_FILENO: c_int = 1;
pub const STDERR_FILENO: c_int = 2;

pub const O_ACCMODE:      c_int = 0x0003;
pub const F_DUPFD_CLOEXEC:c_int = 1030;
pub const F_GETFD:        c_int = 1;
pub const F_GETFL:        c_int = 3;
pub const F_SETFL:        c_int = 4;
pub const FIOCLEX:        c_ulong = 0x5451;
pub const FIONBIO:        c_ulong = 0x5421;

// exit
pub const EXIT_SUCCESS: c_int = 0;
pub const EXIT_FAILURE: c_int = 1;

// poll
pub const POLLIN:  c_short = 0x001;
pub const POLLOUT: c_short = 0x004;
pub const POLLERR: c_short = 0x008;
pub const POLLHUP: c_short = 0x010;
pub const POLLNVAL:c_short = 0x020;

// socket
pub const SOL_SOCKET:   c_int = 1;
pub const SO_ERROR:     c_int = 4;
pub const SO_LINGER:    c_int = 13;
pub const SO_RCVTIMEO:  c_int = 20;
pub const SO_SNDTIMEO:  c_int = 21;
pub const SOMAXCONN:    c_int = 128;
pub const AF_UNSPEC:    c_int = 0;
pub const IPPROTO_TCP:  c_int = 6;
pub const TCP_NODELAY:  c_int = 1;
pub const MSG_PEEK:     c_int = 0x02;
pub const SHUT_RD:      c_int = 0;
pub const SHUT_WR:      c_int = 1;
pub const SHUT_RDWR:    c_int = 2;

// wait
pub const WNOHANG: c_int = 1;

// at-file flags
pub const AT_FDCWD:            c_int = -100;
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x100;
pub const AT_REMOVEDIR:        c_int = 0x200;
pub const UTIME_OMIT:          c_long = 1073741822;

// stat mode bits
pub const S_IFMT:  mode_t = 0o170000;
pub const S_IFSOCK:mode_t = 0o140000;
pub const S_IFLNK: mode_t = 0o120000;
pub const S_IFREG: mode_t = 0o100000;
pub const S_IFBLK: mode_t = 0o060000;
pub const S_IFDIR: mode_t = 0o040000;
pub const S_IFCHR: mode_t = 0o020000;
pub const S_IFIFO: mode_t = 0o010000;
pub const S_ISUID: mode_t = 0o004000;
pub const S_ISGID: mode_t = 0o002000;
pub const S_ISVTX: mode_t = 0o001000;
pub const S_IRUSR: mode_t = 0o400;
pub const S_IWUSR: mode_t = 0o200;
pub const S_IXUSR: mode_t = 0o100;
pub const S_IRGRP: mode_t = 0o040;
pub const S_IWGRP: mode_t = 0o020;
pub const S_IXGRP: mode_t = 0o010;
pub const S_IROTH: mode_t = 0o004;
pub const S_IWOTH: mode_t = 0o002;
pub const S_IXOTH: mode_t = 0o001;

// signals extra
pub const SIGKILL: c_int = 9;
pub const SIG_ERR: sighandler_t = !0usize as sighandler_t;

// sysconf
pub const _SC_PAGESIZE:         c_int = 30;
pub const _SC_GETPW_R_SIZE_MAX: c_int = 70;
pub const _SC_HOST_NAME_MAX:    c_int = 180;

// getaddrinfo
pub const EAI_SYSTEM: c_int = -11;

// pthread mutex types
pub const PTHREAD_MUTEX_NORMAL:    c_int = 0;
pub const PTHREAD_MUTEX_RECURSIVE: c_int = 1;
pub const PTHREAD_MUTEX_ERRORCHECK:c_int = 2;

// EINPROGRESS
pub const EINPROGRESS: c_int = 115;

// dirent types
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO:    u8 = 1;
pub const DT_CHR:     u8 = 2;
pub const DT_DIR:     u8 = 4;
pub const DT_BLK:     u8 = 6;
pub const DT_REG:     u8 = 8;
pub const DT_LNK:     u8 = 10;
pub const DT_SOCK:    u8 = 12;

// ── W* macros de waitpid como funciones inline ────────────────────────────────
#[inline] pub fn WIFEXITED(status: c_int)   -> bool { (status & 0x7f) == 0 }
#[inline] pub fn WEXITSTATUS(status: c_int) -> c_int { (status >> 8) & 0xff }
#[inline] pub fn WIFSIGNALED(status: c_int) -> bool { ((status & 0x7f) + 1) as i8 >= 2 }
#[inline] pub fn WTERMSIG(status: c_int)    -> c_int { status & 0x7f }
#[inline] pub fn WIFSTOPPED(status: c_int)  -> bool { (status & 0xff) == 0x7f }
#[inline] pub fn WSTOPSIG(status: c_int)    -> c_int { (status >> 8) & 0xff }
#[inline] pub fn WIFCONTINUED(status: c_int)-> bool { status == 0xffff }
#[inline] pub fn WCOREDUMP(status: c_int) -> bool { (status & 0x80) != 0 }

// ── Errnos adicionales (Linux x86-64) que std usa ─────────────────────────────
pub const E2BIG:           c_int = 7;
pub const EBUSY:           c_int = 16;
pub const ECHILD:          c_int = 10;
pub const ECONNABORTED:    c_int = 103;
pub const ECONNREFUSED:    c_int = 111;
pub const ECONNRESET:      c_int = 104;
pub const EDEADLK:         c_int = 35;
pub const EDQUOT:          c_int = 122;
pub const EFBIG:           c_int = 27;
pub const EHOSTUNREACH:    c_int = 113;
pub const EISCONN:         c_int = 106;
pub const EISDIR:          c_int = 21;
pub const ELOOP:           c_int = 40;
pub const EMLINK:          c_int = 31;
pub const EMSGSIZE:        c_int = 90;
pub const ENAMETOOLONG:    c_int = 36;
pub const ENETDOWN:        c_int = 100;
pub const ENETUNREACH:     c_int = 101;
pub const ENOBUFS:         c_int = 105;
pub const ENODEV:          c_int = 19;
pub const ENOEXEC:         c_int = 8;
pub const ENOLCK:          c_int = 37;
pub const ENOSPC:          c_int = 28;
pub const ENOTCONN:        c_int = 107;
pub const ENOTEMPTY:       c_int = 39;
pub const ENOTSOCK:        c_int = 88;
pub const ENOTTY:          c_int = 25;
pub const ENXIO:           c_int = 6;
pub const EOPNOTSUPP:      c_int = 95;
pub const EPROTO:          c_int = 71;
pub const EROFS:           c_int = 30;
pub const ESPIPE:          c_int = 29;
pub const ESTALE:          c_int = 116;
pub const ETXTBSY:         c_int = 26;
pub const EXDEV:           c_int = 18;
pub const ECANCELED:       c_int = 125;

// ── Señales adicionales (Linux) ───────────────────────────────────────────────
pub const SIGHUP:    c_int = 1;
pub const SIGINT:    c_int = 2;
pub const SIGQUIT:   c_int = 3;
pub const SIGTRAP:   c_int = 5;
pub const SIGCHLD:   c_int = 17;
pub const SIGCONT:   c_int = 18;
pub const SIGSTOP:   c_int = 19;
pub const SIGTSTP:   c_int = 20;
pub const SIGTTIN:   c_int = 21;
pub const SIGTTOU:   c_int = 22;
pub const SIGURG:    c_int = 23;
pub const SIGXCPU:   c_int = 24;
pub const SIGXFSZ:   c_int = 25;
pub const SIGVTALRM: c_int = 26;
pub const SIGPROF:   c_int = 27;
pub const SIGWINCH:  c_int = 28;
pub const SIGIO:     c_int = 29;
pub const SIGUSR1:   c_int = 10;
pub const SIGUSR2:   c_int = 12;
pub const SIGSYS:    c_int = 31;

// ── Opciones de socket / IP (Linux) ───────────────────────────────────────────
pub const IPPROTO_IP:   c_int = 0;
pub const IPPROTO_IPV6: c_int = 41;
pub const IPPROTO_UDP:  c_int = 17;
pub const IP_TTL:            c_int = 2;
pub const IP_MULTICAST_LOOP: c_int = 34;
pub const IP_MULTICAST_TTL:  c_int = 33;
pub const IP_ADD_MEMBERSHIP: c_int = 35;
pub const IP_DROP_MEMBERSHIP:c_int = 37;
pub const IPV6_V6ONLY:       c_int = 26;
pub const IPV6_MULTICAST_LOOP:c_int = 19;
pub const SO_REUSEADDR: c_int = 2;
pub const SO_BROADCAST: c_int = 6;
pub const SO_DEBUG:     c_int = 1;
pub const SO_TYPE:      c_int = 3;
pub const SO_DONTROUTE: c_int = 5;
pub const SO_SNDBUF:    c_int = 7;
pub const SO_RCVBUF:    c_int = 8;
pub const SO_KEEPALIVE: c_int = 9;
pub const SO_OOBINLINE: c_int = 10;
pub const SO_PEERCRED:  c_int = 17;

// ── Declaraciones extern "C" adicionales ─────────────────────────────────────
extern "C" {
    // Procesos y señales
    pub fn fork() -> pid_t;
    pub fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    pub fn waitpid(pid: pid_t, status: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn signal(signum: c_int, handler: sighandler_t) -> sighandler_t;
    pub fn sigemptyset(set: *mut sigset_t) -> c_int;
    pub fn sigaddset(set: *mut sigset_t, signum: c_int) -> c_int;
    pub fn sched_yield() -> c_int;
    pub fn sysconf(name: c_int) -> c_long;

    // Descriptores de archivo
    pub fn dup(oldfd: c_int) -> c_int;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn pipe(pipefd: *mut c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn fsync(fd: c_int) -> c_int;
    pub fn ftruncate(fd: c_int, length: off_t) -> c_int;
    pub fn pread(fd: c_int, buf: *mut c_void, count: size_t, offset: off_t) -> ssize_t;
    pub fn pwrite(fd: c_int, buf: *const c_void, count: size_t, offset: off_t) -> ssize_t;
    pub fn readv(fd: c_int, iov: *const iovec, iovcnt: c_int) -> ssize_t;
    pub fn writev(fd: c_int, iov: *const iovec, iovcnt: c_int) -> ssize_t;

    // Sistema de archivos
    pub fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    pub fn unlink(path: *const c_char) -> c_int;
    pub fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int;
    pub fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int;
    pub fn mkdir(path: *const c_char, mode: mode_t) -> c_int;
    pub fn rmdir(path: *const c_char) -> c_int;
    pub fn mkfifo(path: *const c_char, mode: mode_t) -> c_int;
    pub fn chmod(path: *const c_char, mode: mode_t) -> c_int;
    pub fn fchmod(fd: c_int, mode: mode_t) -> c_int;
    pub fn chown(path: *const c_char, owner: uid_t, group: gid_t) -> c_int;
    pub fn fchown(fd: c_int, owner: uid_t, group: gid_t) -> c_int;
    pub fn lchown(path: *const c_char, owner: uid_t, group: gid_t) -> c_int;
    pub fn chroot(path: *const c_char) -> c_int;
    pub fn lstat(path: *const c_char, buf: *mut stat) -> c_int;
    pub fn readlink(path: *const c_char, buf: *mut c_char, bufsiz: size_t) -> ssize_t;
    pub fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char;
    pub fn linkat(
        olddirfd: c_int, oldpath: *const c_char,
        newdirfd: c_int, newpath: *const c_char, flags: c_int,
    ) -> c_int;
    pub fn futimens(fd: c_int, times: *const timespec) -> c_int;
    pub fn utimensat(
        dirfd: c_int, path: *const c_char,
        times: *const timespec, flags: c_int,
    ) -> c_int;

    // Directorios
    pub fn opendir(name: *const c_char) -> *mut DIR;
    pub fn fdopendir(fd: c_int) -> *mut DIR;
    pub fn closedir(dir: *mut DIR) -> c_int;
    pub fn dirfd(dir: *mut DIR) -> c_int;
    pub fn readdir_r(dir: *mut DIR, entry: *mut dirent, result: *mut *mut dirent) -> c_int;

    // Sockets
    pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    pub fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn listen(sockfd: c_int, backlog: c_int) -> c_int;
    pub fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
    pub fn getsockname(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
    pub fn getpeername(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
    pub fn send(sockfd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t;
    pub fn recv(sockfd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t;
    pub fn sendto(
        sockfd: c_int, buf: *const c_void, len: size_t, flags: c_int,
        dest_addr: *const sockaddr, addrlen: socklen_t,
    ) -> ssize_t;
    pub fn recvfrom(
        sockfd: c_int, buf: *mut c_void, len: size_t, flags: c_int,
        src_addr: *mut sockaddr, addrlen: *mut socklen_t,
    ) -> ssize_t;
    pub fn setsockopt(
        sockfd: c_int, level: c_int, optname: c_int,
        optval: *const c_void, optlen: socklen_t,
    ) -> c_int;
    pub fn getsockopt(
        sockfd: c_int, level: c_int, optname: c_int,
        optval: *mut c_void, optlen: *mut socklen_t,
    ) -> c_int;
    pub fn shutdown(sockfd: c_int, how: c_int) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: nfds_t, timeout: c_int) -> c_int;

    // DNS / red
    pub fn getaddrinfo(
        node: *const c_char, service: *const c_char,
        hints: *const addrinfo, res: *mut *mut addrinfo,
    ) -> c_int;
    pub fn freeaddrinfo(res: *mut addrinfo);
    pub fn gai_strerror(errcode: c_int) -> *const c_char;
    pub fn gethostname(name: *mut c_char, len: size_t) -> c_int;

    // Usuarios / entorno (getenv está en lib.rs — no duplicar)
    pub fn getuid() -> uid_t;
    pub fn setuid(uid: uid_t) -> c_int;
    pub fn setgid(gid: gid_t) -> c_int;
    pub fn setgroups(size: size_t, list: *const gid_t) -> c_int;
    pub fn setpgid(pid: pid_t, pgid: pid_t) -> c_int;
    pub fn setsid() -> pid_t;
    pub fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    pub fn unsetenv(name: *const c_char) -> c_int;
    pub fn getpwuid_r(
        uid: uid_t, pwd: *mut passwd, buf: *mut c_char,
        buflen: size_t, result: *mut *mut passwd,
    ) -> c_int;

    // Memoria
    pub fn posix_memalign(memptr: *mut *mut c_void, align: size_t, size: size_t) -> c_int;

    // pthread mutexattr
    pub fn pthread_mutexattr_init(attr: *mut pthread_mutexattr_t) -> c_int;
    pub fn pthread_mutexattr_settype(attr: *mut pthread_mutexattr_t, kind: c_int) -> c_int;
    pub fn pthread_mutexattr_destroy(attr: *mut pthread_mutexattr_t) -> c_int;

    // rwlock (std lo usa internamente)
    pub fn pthread_rwlock_init(
        rwlock: *mut pthread_rwlock_t,
        attr: *const pthread_rwlockattr_t,
    ) -> c_int;
    pub fn pthread_rwlock_rdlock(rwlock: *mut pthread_rwlock_t) -> c_int;
    pub fn pthread_rwlock_tryrdlock(rwlock: *mut pthread_rwlock_t) -> c_int;
    pub fn pthread_rwlock_wrlock(rwlock: *mut pthread_rwlock_t) -> c_int;
    pub fn pthread_rwlock_trywrlock(rwlock: *mut pthread_rwlock_t) -> c_int;
    pub fn pthread_rwlock_unlock(rwlock: *mut pthread_rwlock_t) -> c_int;
    pub fn pthread_rwlock_destroy(rwlock: *mut pthread_rwlock_t) -> c_int;

    // Formato de números
    pub fn strtod(nptr: *const c_char, endptr: *mut *mut c_char) -> c_double;
    pub fn strtof(nptr: *const c_char, endptr: *mut *mut c_char) -> c_float;
    pub fn strtol(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long;
    pub fn strtoul(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong;
    pub fn strnlen(s: *const c_char, maxlen: size_t) -> size_t;
    pub fn symlink(target: *const c_char, linkpath: *const c_char) -> c_int;
    pub fn socketpair(domain: c_int, ty: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
}

// ── Tipos pthread extra ───────────────────────────────────────────────────────

#[repr(C)]
pub struct pthread_rwlock_t { _data: [u8; 56] }
impl pthread_rwlock_t {
    pub const fn new() -> Self { pthread_rwlock_t { _data: [0; 56] } }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct pthread_rwlockattr_t { _data: [u8; 8] }

pub const PTHREAD_RWLOCK_INITIALIZER: pthread_rwlock_t = pthread_rwlock_t { _data: [0; 56] };
