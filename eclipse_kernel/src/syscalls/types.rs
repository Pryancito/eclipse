//! Tipos y estructuras para syscalls
//! 
//! Este módulo define los tipos y estructuras utilizados por el sistema de syscalls.

use alloc::vec::Vec;

/// Descriptor de archivo
pub type FileDescriptor = i32;

/// PID de proceso
pub type ProcessId = i32;

/// UID de usuario
pub type UserId = u32;

/// GID de grupo
pub type GroupId = u32;

/// Modo de archivo
pub type FileMode = u32;

/// Flags de apertura de archivo
#[derive(Debug, Clone, Copy)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub create: bool,
    pub truncate: bool,
    pub exclusive: bool,
    pub no_follow: bool,
    pub directory: bool,
    pub no_access_time: bool,
    pub large_file: bool,
}

impl OpenFlags {
    /// Crear flags desde valor entero
    pub fn from_bits(bits: i32) -> Self {
        Self {
            read: (bits & 0o0) != 0,
            write: (bits & 0o1) != 0,
            append: (bits & 0o2000) != 0,
            create: (bits & 0o100) != 0,
            truncate: (bits & 0o1000) != 0,
            exclusive: (bits & 0o200) != 0,
            no_follow: (bits & 0o400000) != 0,
            directory: (bits & 0o200000) != 0,
            no_access_time: (bits & 0o40000) != 0,
            large_file: (bits & 0o100000) != 0,
        }
    }

    /// Convertir a bits
    pub fn to_bits(&self) -> i32 {
        let mut bits = 0;
        if self.read { bits |= 0o0; }
        if self.write { bits |= 0o1; }
        if self.append { bits |= 0o2000; }
        if self.create { bits |= 0o100; }
        if self.truncate { bits |= 0o1000; }
        if self.exclusive { bits |= 0o200; }
        if self.no_follow { bits |= 0o400000; }
        if self.directory { bits |= 0o200000; }
        if self.no_access_time { bits |= 0o40000; }
        if self.large_file { bits |= 0o100000; }
        bits
    }
}

/// Información de archivo (struct stat)
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
}

impl FileInfo {
    /// Crear información de archivo vacía
    pub fn new() -> Self {
        Self {
            st_dev: 0,
            st_ino: 0,
            st_mode: 0,
            st_nlink: 0,
            st_uid: 0,
            st_gid: 0,
            st_rdev: 0,
            st_size: 0,
            st_blksize: 512,
            st_blocks: 0,
            st_atime: 0,
            st_atime_nsec: 0,
            st_mtime: 0,
            st_mtime_nsec: 0,
            st_ctime: 0,
            st_ctime_nsec: 0,
        }
    }

    /// Serializar a bytes (formato struct stat de Linux)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        
        // st_dev (8 bytes)
        bytes.extend_from_slice(&self.st_dev.to_le_bytes());
        
        // st_ino (8 bytes)
        bytes.extend_from_slice(&self.st_ino.to_le_bytes());
        
        // st_mode (4 bytes)
        bytes.extend_from_slice(&self.st_mode.to_le_bytes());
        
        // st_nlink (4 bytes)
        bytes.extend_from_slice(&self.st_nlink.to_le_bytes());
        
        // st_uid (4 bytes)
        bytes.extend_from_slice(&self.st_uid.to_le_bytes());
        
        // st_gid (4 bytes)
        bytes.extend_from_slice(&self.st_gid.to_le_bytes());
        
        // st_rdev (8 bytes)
        bytes.extend_from_slice(&self.st_rdev.to_le_bytes());
        
        // st_size (8 bytes)
        bytes.extend_from_slice(&self.st_size.to_le_bytes());
        
        // st_blksize (8 bytes)
        bytes.extend_from_slice(&self.st_blksize.to_le_bytes());
        
        // st_blocks (8 bytes)
        bytes.extend_from_slice(&self.st_blocks.to_le_bytes());
        
        // st_atime (8 bytes)
        bytes.extend_from_slice(&self.st_atime.to_le_bytes());
        
        // st_atime_nsec (8 bytes)
        bytes.extend_from_slice(&self.st_atime_nsec.to_le_bytes());
        
        // st_mtime (8 bytes)
        bytes.extend_from_slice(&self.st_mtime.to_le_bytes());
        
        // st_mtime_nsec (8 bytes)
        bytes.extend_from_slice(&self.st_mtime_nsec.to_le_bytes());
        
        // st_ctime (8 bytes)
        bytes.extend_from_slice(&self.st_ctime.to_le_bytes());
        
        // st_ctime_nsec (8 bytes)
        bytes.extend_from_slice(&self.st_ctime_nsec.to_le_bytes());
        
        bytes
    }
}

/// Información de tiempo
#[derive(Debug, Clone)]
pub struct TimeVal {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl TimeVal {
    /// Crear TimeVal vacío
    pub fn new() -> Self {
        Self {
            tv_sec: 0,
            tv_usec: 0,
        }
    }

    /// Serializar a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.tv_sec.to_le_bytes());
        bytes.extend_from_slice(&self.tv_usec.to_le_bytes());
        bytes
    }
}

/// Información de zona horaria
#[derive(Debug, Clone)]
pub struct TimeZone {
    pub tz_minuteswest: i32,
    pub tz_dsttime: i32,
}

impl TimeZone {
    /// Crear TimeZone vacío
    pub fn new() -> Self {
        Self {
            tz_minuteswest: 0,
            tz_dsttime: 0,
        }
    }

    /// Serializar a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.tz_minuteswest.to_le_bytes());
        bytes.extend_from_slice(&self.tz_dsttime.to_le_bytes());
        bytes
    }
}

/// Información de uso de recursos
#[derive(Debug, Clone)]
pub struct ResourceUsage {
    pub ru_utime: TimeVal,
    pub ru_stime: TimeVal,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

impl ResourceUsage {
    /// Crear ResourceUsage vacío
    pub fn new() -> Self {
        Self {
            ru_utime: TimeVal::new(),
            ru_stime: TimeVal::new(),
            ru_maxrss: 0,
            ru_ixrss: 0,
            ru_idrss: 0,
            ru_isrss: 0,
            ru_minflt: 0,
            ru_majflt: 0,
            ru_nswap: 0,
            ru_inblock: 0,
            ru_oublock: 0,
            ru_msgsnd: 0,
            ru_msgrcv: 0,
            ru_nsignals: 0,
            ru_nvcsw: 0,
            ru_nivcsw: 0,
        }
    }

    /// Serializar a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.ru_utime.to_bytes());
        bytes.extend_from_slice(&self.ru_stime.to_bytes());
        bytes.extend_from_slice(&self.ru_maxrss.to_le_bytes());
        bytes.extend_from_slice(&self.ru_ixrss.to_le_bytes());
        bytes.extend_from_slice(&self.ru_idrss.to_le_bytes());
        bytes.extend_from_slice(&self.ru_isrss.to_le_bytes());
        bytes.extend_from_slice(&self.ru_minflt.to_le_bytes());
        bytes.extend_from_slice(&self.ru_majflt.to_le_bytes());
        bytes.extend_from_slice(&self.ru_nswap.to_le_bytes());
        bytes.extend_from_slice(&self.ru_inblock.to_le_bytes());
        bytes.extend_from_slice(&self.ru_oublock.to_le_bytes());
        bytes.extend_from_slice(&self.ru_msgsnd.to_le_bytes());
        bytes.extend_from_slice(&self.ru_msgrcv.to_le_bytes());
        bytes.extend_from_slice(&self.ru_nsignals.to_le_bytes());
        bytes.extend_from_slice(&self.ru_nvcsw.to_le_bytes());
        bytes.extend_from_slice(&self.ru_nivcsw.to_le_bytes());
        bytes
    }
}

/// Información del sistema
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
}

impl SystemInfo {
    /// Crear SystemInfo vacío
    pub fn new() -> Self {
        Self {
            uptime: 0,
            loads: [0, 0, 0],
            totalram: 0,
            freeram: 0,
            sharedram: 0,
            bufferram: 0,
            totalswap: 0,
            freeswap: 0,
            procs: 0,
            totalhigh: 0,
            freehigh: 0,
            mem_unit: 1,
        }
    }

    /// Serializar a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.uptime.to_le_bytes());
        bytes.extend_from_slice(&self.loads[0].to_le_bytes());
        bytes.extend_from_slice(&self.loads[1].to_le_bytes());
        bytes.extend_from_slice(&self.loads[2].to_le_bytes());
        bytes.extend_from_slice(&self.totalram.to_le_bytes());
        bytes.extend_from_slice(&self.freeram.to_le_bytes());
        bytes.extend_from_slice(&self.sharedram.to_le_bytes());
        bytes.extend_from_slice(&self.bufferram.to_le_bytes());
        bytes.extend_from_slice(&self.totalswap.to_le_bytes());
        bytes.extend_from_slice(&self.freeswap.to_le_bytes());
        bytes.extend_from_slice(&self.procs.to_le_bytes());
        bytes.extend_from_slice(&self.totalhigh.to_le_bytes());
        bytes.extend_from_slice(&self.freehigh.to_le_bytes());
        bytes.extend_from_slice(&self.mem_unit.to_le_bytes());
        bytes
    }
}

/// Información de archivo de sistema
#[derive(Debug, Clone)]
pub struct FileSystemInfo {
    pub f_type: i64,
    pub f_bsize: i64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: [i32; 2],
    pub f_namelen: i64,
    pub f_frsize: i64,
    pub f_flags: i64,
    pub f_spare: [i64; 4],
}

impl FileSystemInfo {
    /// Crear FileSystemInfo vacío
    pub fn new() -> Self {
        Self {
            f_type: 0,
            f_bsize: 4096,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            f_files: 0,
            f_ffree: 0,
            f_fsid: [0, 0],
            f_namelen: 255,
            f_frsize: 4096,
            f_flags: 0,
            f_spare: [0, 0, 0, 0],
        }
    }

    /// Serializar a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.f_type.to_le_bytes());
        bytes.extend_from_slice(&self.f_bsize.to_le_bytes());
        bytes.extend_from_slice(&self.f_blocks.to_le_bytes());
        bytes.extend_from_slice(&self.f_bfree.to_le_bytes());
        bytes.extend_from_slice(&self.f_bavail.to_le_bytes());
        bytes.extend_from_slice(&self.f_files.to_le_bytes());
        bytes.extend_from_slice(&self.f_ffree.to_le_bytes());
        bytes.extend_from_slice(&self.f_fsid[0].to_le_bytes());
        bytes.extend_from_slice(&self.f_fsid[1].to_le_bytes());
        bytes.extend_from_slice(&self.f_namelen.to_le_bytes());
        bytes.extend_from_slice(&self.f_frsize.to_le_bytes());
        bytes.extend_from_slice(&self.f_flags.to_le_bytes());
        bytes.extend_from_slice(&self.f_spare[0].to_le_bytes());
        bytes.extend_from_slice(&self.f_spare[1].to_le_bytes());
        bytes.extend_from_slice(&self.f_spare[2].to_le_bytes());
        bytes.extend_from_slice(&self.f_spare[3].to_le_bytes());
        bytes
    }
}

/// Constantes para syscalls

/// Descriptores de archivo estándar
pub const STDIN_FD: FileDescriptor = 0;
pub const STDOUT_FD: FileDescriptor = 1;
pub const STDERR_FD: FileDescriptor = 2;

/// Flags de apertura de archivo
pub const O_RDONLY: i32 = 0o0;
pub const O_WRONLY: i32 = 0o1;
pub const O_RDWR: i32 = 0o2;
pub const O_CREAT: i32 = 0o100;
pub const O_EXCL: i32 = 0o200;
pub const O_NOCTTY: i32 = 0o400;
pub const O_TRUNC: i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_DSYNC: i32 = 0o10000;
pub const O_FASYNC: i32 = 0o20000;
pub const O_DIRECT: i32 = 0o40000;
pub const O_LARGEFILE: i32 = 0o100000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NOFOLLOW: i32 = 0o400000;
pub const O_NOATIME: i32 = 0o40000;
pub const O_CLOEXEC: i32 = 0o2000000;

/// Modos de archivo
pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o60000;
pub const S_IFDIR: u32 = 0o40000;
pub const S_IFCHR: u32 = 0o20000;
pub const S_IFIFO: u32 = 0o10000;

pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;

pub const S_IRWXU: u32 = 0o700;
pub const S_IRUSR: u32 = 0o400;
pub const S_IWUSR: u32 = 0o200;
pub const S_IXUSR: u32 = 0o100;

pub const S_IRWXG: u32 = 0o70;
pub const S_IRGRP: u32 = 0o40;
pub const S_IWGRP: u32 = 0o20;
pub const S_IXGRP: u32 = 0o10;

pub const S_IRWXO: u32 = 0o7;
pub const S_IROTH: u32 = 0o4;
pub const S_IWOTH: u32 = 0o2;
pub const S_IXOTH: u32 = 0o1;

/// Macros para verificar tipos de archivo
pub fn S_ISREG(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFREG
}

pub fn S_ISDIR(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFDIR
}

pub fn S_ISCHR(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFCHR
}

pub fn S_ISBLK(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFBLK
}

pub fn S_ISFIFO(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFIFO
}

pub fn S_ISLNK(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFLNK
}

pub fn S_ISSOCK(mode: u32) -> bool {
    (mode & S_IFMT) == S_IFSOCK
}

/// Señales
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGABRT: i32 = 6;
pub const SIGFPE: i32 = 8;
pub const SIGSEGV: i32 = 11;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;

/// Códigos de error estándar
pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const ESRCH: i32 = 3;
pub const EINTR: i32 = 4;
pub const EIO: i32 = 5;
pub const ENXIO: i32 = 6;
pub const E2BIG: i32 = 7;
pub const ENOEXEC: i32 = 8;
pub const EBADF: i32 = 9;
pub const ECHILD: i32 = 10;
pub const EAGAIN: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const ENOTBLK: i32 = 15;
pub const EBUSY: i32 = 16;
pub const EEXIST: i32 = 17;
pub const EXDEV: i32 = 18;
pub const ENODEV: i32 = 19;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const ENFILE: i32 = 23;
pub const EMFILE: i32 = 24;
pub const ENOTTY: i32 = 25;
pub const ETXTBSY: i32 = 26;
pub const EFBIG: i32 = 27;
pub const ENOSPC: i32 = 28;
pub const ESPIPE: i32 = 29;
pub const EROFS: i32 = 30;
pub const EMLINK: i32 = 31;
pub const EPIPE: i32 = 32;
pub const EDOM: i32 = 33;
pub const ERANGE: i32 = 34;
pub const EDEADLK: i32 = 35;
pub const ENAMETOOLONG: i32 = 36;
pub const ENOLCK: i32 = 37;
pub const ENOSYS: i32 = 38;
pub const ENOTEMPTY: i32 = 39;
pub const ELOOP: i32 = 40;
pub const EWOULDBLOCK: i32 = EAGAIN;
pub const ENOMSG: i32 = 42;
pub const EIDRM: i32 = 43;
pub const ECHRNG: i32 = 44;
pub const EL2NSYNC: i32 = 45;
pub const EL3HLT: i32 = 46;
pub const EL3RST: i32 = 47;
pub const ELNRNG: i32 = 48;
pub const EUNATCH: i32 = 49;
pub const ENOCSI: i32 = 50;
pub const EL2HLT: i32 = 51;
pub const EBADE: i32 = 52;
pub const EBADR: i32 = 53;
pub const EXFULL: i32 = 54;
pub const ENOANO: i32 = 55;
pub const EBADRQC: i32 = 56;
pub const EBADSLT: i32 = 57;
pub const EDEADLOCK: i32 = EDEADLK;
pub const EBFONT: i32 = 59;
pub const ENOSTR: i32 = 60;
pub const ENODATA: i32 = 61;
pub const ETIME: i32 = 62;
pub const ENOSR: i32 = 63;
pub const ENONET: i32 = 64;
pub const ENOPKG: i32 = 65;
pub const EREMOTE: i32 = 66;
pub const ENOLINK: i32 = 67;
pub const EADV: i32 = 68;
pub const ESRMNT: i32 = 69;
pub const ECOMM: i32 = 70;
pub const EPROTO: i32 = 71;
pub const EMULTIHOP: i32 = 72;
pub const EDOTDOT: i32 = 73;
pub const EBADMSG: i32 = 74;
pub const EOVERFLOW: i32 = 75;
pub const ENOTUNIQ: i32 = 76;
pub const EBADFD: i32 = 77;
pub const EREMCHG: i32 = 78;
pub const ELIBACC: i32 = 79;
pub const ELIBBAD: i32 = 80;
pub const ELIBSCN: i32 = 81;
pub const ELIBMAX: i32 = 82;
pub const ELIBEXEC: i32 = 83;
pub const EILSEQ: i32 = 84;
pub const ERESTART: i32 = 85;
pub const ESTRPIPE: i32 = 86;
pub const EUSERS: i32 = 87;
pub const ENOTSOCK: i32 = 88;
pub const EDESTADDRREQ: i32 = 89;
pub const EMSGSIZE: i32 = 90;
pub const EPROTOTYPE: i32 = 91;
pub const ENOPROTOOPT: i32 = 92;
pub const EPROTONOSUPPORT: i32 = 93;
pub const ESOCKTNOSUPPORT: i32 = 94;
pub const EOPNOTSUPP: i32 = 95;
pub const EPFNOSUPPORT: i32 = 96;
pub const EAFNOSUPPORT: i32 = 97;
pub const EADDRINUSE: i32 = 98;
pub const EADDRNOTAVAIL: i32 = 99;
pub const ENETDOWN: i32 = 100;
pub const ENETUNREACH: i32 = 101;
pub const ENETRESET: i32 = 102;
pub const ECONNABORTED: i32 = 103;
pub const ECONNRESET: i32 = 104;
pub const ENOBUFS: i32 = 105;
pub const EISCONN: i32 = 106;
pub const ENOTCONN: i32 = 107;
pub const ESHUTDOWN: i32 = 108;
pub const ETOOMANYREFS: i32 = 109;
pub const ETIMEDOUT: i32 = 110;
pub const ECONNREFUSED: i32 = 111;
pub const EHOSTDOWN: i32 = 112;
pub const EHOSTUNREACH: i32 = 113;
pub const EALREADY: i32 = 114;
pub const EINPROGRESS: i32 = 115;
pub const ESTALE: i32 = 116;
pub const EUCLEAN: i32 = 117;
pub const ENOTNAM: i32 = 118;
pub const ENAVAIL: i32 = 119;
pub const EISNAM: i32 = 120;
pub const EREMOTEIO: i32 = 121;
pub const EDQUOT: i32 = 122;
pub const ENOMEDIUM: i32 = 123;
pub const EMEDIUMTYPE: i32 = 124;
pub const ECANCELED: i32 = 125;
pub const ENOKEY: i32 = 126;
pub const EKEYEXPIRED: i32 = 127;
pub const EKEYREVOKED: i32 = 128;
pub const EKEYREJECTED: i32 = 129;
pub const EOWNERDEAD: i32 = 130;
pub const ENOTRECOVERABLE: i32 = 131;
pub const ERFKILL: i32 = 132;
pub const EHWPOISON: i32 = 133;

