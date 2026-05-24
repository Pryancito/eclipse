//! Linux Process

use crate::{
    error::{LxError, LxResult},
    fs::{File, FileDesc, FileLike, OpenFlags, STDIN, STDOUT},
    ipc::*,
    net::SOCKET_FD,
    signal::{Signal as LinuxSignal, SignalAction},
};
use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::AtomicI32;
use hashbrown::HashMap;
use kernel_hal::VirtAddr;
use lock::{Mutex, MutexGuard};
use rcore_fs::vfs::{FileSystem, INode, Metadata};

use zircon_object::{
    object::{KernelObject, KoID, Signal},
    signal::Futex,
    task::{Job, Process, Status, Thread, ROOT_JOB},
    ZxResult,
};

pub use rcore_fs::vfs::FsInfo;

/// Process extension for linux
pub trait ProcessExt {
    /// create Linux process
    fn create_linux(job: &Arc<Job>, rootfs: Arc<dyn FileSystem>) -> ZxResult<Arc<Self>>;
    /// get linux process
    fn linux(&self) -> &LinuxProcess;
    /// fork from current linux process
    fn fork_from(parent: &Arc<Self>, vfork: bool) -> ZxResult<Arc<Self>>;
}

const ROOT_UID: u32 = 0;
const NO_ID: u32 = u32::MAX;
const ACCESS_READ: u16 = 0o4;
const ACCESS_WRITE: u16 = 0o2;
const ACCESS_EXEC: u16 = 0o1;
const MODE_PERM_MASK: u16 = 0o7777;
const MODE_SET_UID: u16 = 0o4000;
const MODE_SET_GID: u16 = 0o2000;
const MODE_STICKY: u16 = 0o1000;

#[derive(Clone, Debug)]
pub struct Credentials {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub groups: Vec<u32>,
    pub umask: u16,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            ruid: ROOT_UID,
            euid: ROOT_UID,
            suid: ROOT_UID,
            rgid: ROOT_UID,
            egid: ROOT_UID,
            sgid: ROOT_UID,
            groups: vec![ROOT_UID],
            umask: 0o022,
        }
    }
}

impl ProcessExt for Process {
    fn create_linux(job: &Arc<Job>, rootfs: Arc<dyn FileSystem>) -> ZxResult<Arc<Self>> {
        let linux_proc = LinuxProcess::new(rootfs);
        let proc = Process::create_with_ext(job, "root", linux_proc)?;
        let weak_proc = Arc::downgrade(&proc);
        proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                if let Some(proc) = weak_proc.upgrade() {
                    let mut inner = proc.linux().inner.lock();
                    inner.files.clear();
                    inner.semaphores = Default::default();
                    inner.shm_identifiers = Default::default();
                }
                return true;
            }
            false
        }));
        Ok(proc)
    }

    fn linux(&self) -> &LinuxProcess {
        self.ext().downcast_ref::<LinuxProcess>().unwrap()
    }

    /// [Fork] the process.
    ///
    /// [Fork]: http://man7.org/linux/man-pages/man2/fork.2.html
    fn fork_from(parent: &Arc<Self>, _vfork: bool) -> ZxResult<Arc<Self>> {
        let linux_parent = parent.linux();
        let mut linux_parent_inner = linux_parent.inner.lock();
        let new_linux_proc = LinuxProcess {
            root_inode: linux_parent.root_inode.clone(),
            parent: Arc::downgrade(parent),
            inner: Mutex::new(LinuxProcessInner {
                execute_path: linux_parent_inner.execute_path.clone(),
                current_working_directory: linux_parent_inner.current_working_directory.clone(),
                files: linux_parent_inner.files.clone(),
                signal_actions: linux_parent_inner.signal_actions.clone(),
                credentials: linux_parent_inner.credentials.clone(),
                ..Default::default()
            }),
        };
        let new_proc = Process::create_with_ext(&parent.job(), "", new_linux_proc)?;
        new_proc.vmar().fork_from(&parent.vmar())?;
        new_proc.set_status_running();
        linux_parent_inner
            .children
            .insert(new_proc.id(), new_proc.clone());

        // notify parent on terminated
        let parent = parent.clone();
        let weak_proc = Arc::downgrade(&new_proc);
        new_proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                parent.signal_set(Signal::SIGCHLD);
                if let Some(proc) = weak_proc.upgrade() {
                    let mut inner = proc.linux().inner.lock();
                    inner.files.clear();
                    inner.semaphores = Default::default();
                    inner.shm_identifiers = Default::default();
                }
                return true;
            }
            false
        }));
        Ok(new_proc)
    }
}

/// Wait for state changes in a child of the calling process, and obtain information about
/// the child whose state has changed.
///
/// A state change is considered to be:
/// - the child terminated.
/// - the child was stopped by a signal. TODO
/// - the child was resumed by a signal. TODO
pub async fn wait_child(proc: &Arc<Process>, pid: KoID, nonblock: bool) -> LxResult<ExitCode> {
    let child = {
        let inner = proc.linux().inner.lock();
        inner.children.get(&pid).cloned().ok_or(LxError::ECHILD)?
    };
    loop {
        if let Status::Exited(code) = child.status() {
            let mut inner = proc.linux().inner.lock();
            inner.children.remove(&pid);
            return Ok((code as i32) << 8);
        }
        if nonblock {
            return Err(LxError::EAGAIN);
        }
        let child_obj: Arc<dyn KernelObject> = child.clone();
        child_obj.wait_signal(Signal::PROCESS_TERMINATED).await;

        // Check again after wait
        if let Status::Exited(code) = child.status() {
            let mut inner = proc.linux().inner.lock();
            inner.children.remove(&pid);
            return Ok((code as i32) << 8);
        }
        continue;
    }
}

/// Wait for state changes in a child of the calling process.
pub async fn wait_child_any(proc: &Arc<Process>, nonblock: bool) -> LxResult<(KoID, ExitCode)> {
    loop {
        let mut inner = proc.linux().inner.lock();
        if inner.children.is_empty() {
            return Err(LxError::ECHILD);
        }
        let mut exited_pid = None;
        warn!("wait_child_any: checking {} children", inner.children.len());
        for (&pid, child) in inner.children.iter() {
            let status = child.status();
            warn!("  child {}: status={:?}", pid, status);
            if let Status::Exited(code) = status {
                exited_pid = Some((pid, code));
                break;
            }
        }
        if let Some((pid, code)) = exited_pid {
            warn!("wait_child_any: reaping child {}", pid);
            inner.children.remove(&pid);
            return Ok((pid, (code as i32) << 8));
        }
        if nonblock {
            return Err(LxError::EAGAIN);
        }
        let proc_obj: Arc<dyn KernelObject> = proc.clone();
        proc_obj.signal_clear(Signal::SIGCHLD);
        // Check again after clear to avoid race
        let mut found_exited = false;
        for child in inner.children.values() {
            if let Status::Exited(_) = child.status() {
                found_exited = true;
                break;
            }
        }
        drop(inner);
        if found_exited {
            warn!("wait_child_any: found exited child after clear, continuing");
            continue;
        }
        warn!("wait_child_any: waiting for SIGCHLD");
        proc_obj.wait_signal(Signal::SIGCHLD).await;
        warn!("wait_child_any: woke up from SIGCHLD");
    }
}

/// Linux specific process information.
pub struct LinuxProcess {
    /// The root INode of file system
    root_inode: Arc<dyn INode>,
    /// Parent process
    parent: Weak<Process>,
    /// Inner
    inner: Mutex<LinuxProcessInner>,
}

/// Linux process mut inner data
#[derive(Default)]
struct LinuxProcessInner {
    /// Execute path
    execute_path: String,
    /// Current Working Directory
    ///
    /// Omit leading '/'.
    current_working_directory: String,
    /// file open number limit
    file_limit: RLimit,
    /// Opened files
    files: HashMap<FileDesc, Arc<dyn FileLike>>,
    /// Semaphore
    semaphores: SemProc,
    /// Share Memory
    shm_identifiers: ShmProc,
    /// Futexes
    futexes: HashMap<VirtAddr, Arc<Futex>>,
    /// Child processes
    children: HashMap<KoID, Arc<Process>>,
    /// Signal actions
    signal_actions: SignalActions,
    /// Program break (top of heap).
    ///
    /// Initialized to 0; set to the end of the loaded ELF image by the loader
    /// via [`LinuxProcess::set_brk`] before the first user instruction runs.
    /// Updated by `sys_brk` as the heap grows or shrinks.
    brk: usize,
    /// Process credentials.
    credentials: Credentials,
}

#[derive(Clone)]
struct SignalActions {
    table: [SignalAction; LinuxSignal::RTMAX + 1],
}

impl Default for SignalActions {
    fn default() -> Self {
        Self {
            table: [SignalAction::default(); LinuxSignal::RTMAX + 1],
        }
    }
}

/// resource limit
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct RLimit {
    /// soft limit
    pub cur: u64,
    /// hard limit
    pub max: u64,
}

impl Default for RLimit {
    fn default() -> Self {
        RLimit {
            cur: 1024,
            max: 1024,
        }
    }
}

/// The type of process exit code.
pub type ExitCode = i32;

impl LinuxProcess {
    /// Create a new process.
    pub fn new(rootfs: Arc<dyn FileSystem>) -> Self {
        let stdin = File::new(
            STDIN.clone(), // FIXME: stdin
            OpenFlags::RDONLY,
            String::from("/dev/stdin"),
        ) as Arc<dyn FileLike>;
        let stdout = File::new(
            STDOUT.clone(), // TODO: open from '/dev/stdout'
            OpenFlags::WRONLY,
            String::from("/dev/stdout"),
        ) as Arc<dyn FileLike>;
        let stderr = File::new(
            STDOUT.clone(), // TODO: open from '/dev/stderr'
            OpenFlags::WRONLY,
            String::from("/dev/stderr"),
        ) as Arc<dyn FileLike>;
        let mut files = HashMap::new();
        files.insert(0.into(), stdin);
        files.insert(1.into(), stdout);
        files.insert(2.into(), stderr);

        LinuxProcess {
            root_inode: crate::fs::create_root_fs(rootfs), //Arc::clone(&ROOT_INODE),访问磁盘可能更快？
            parent: Weak::default(),
            inner: Mutex::new(LinuxProcessInner {
                files,
                ..Default::default()
            }),
        }
    }

    /// Get the parent zircon process.
    pub fn zircon_process(&self) -> Arc<Process> {
        self.parent.upgrade().unwrap()
    }

    /// Get futex object.
    #[allow(unsafe_code)]
    pub fn get_futex(&self, uaddr: VirtAddr) -> Arc<Futex> {
        let mut inner = self.inner.lock();
        inner
            .futexes
            .entry(uaddr)
            .or_insert_with(|| {
                let value = unsafe { &*(uaddr as *const AtomicI32) };
                Futex::new(value)
            })
            .clone()
    }

    /// Get lowest free fd
    pub fn get_free_fd(&self) -> FileDesc {
        self.inner.lock().get_free_fd()
    }

    /// get the lowest available fd great than or equal to `start`.
    pub fn get_free_fd_from(&self, start: usize) -> FileDesc {
        self.inner.lock().get_free_fd_from(start)
    }

    /// Add a file to the file descriptor table.
    pub fn add_file(&self, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let inner = self.inner.lock();
        let fd = inner.get_free_fd();
        self.insert_file(inner, fd, file)
    }

    /// Add a socket to the fd table.
    pub fn add_socket(&self, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let inner = self.inner.lock();
        let fd = inner.get_free_fd_from(SOCKET_FD);
        self.insert_file(inner, fd, file)
    }

    /// Add a file to the file descriptor table at given `fd`.
    pub fn add_file_at(&self, fd: FileDesc, file: Arc<dyn FileLike>) -> LxResult<FileDesc> {
        let inner = self.inner.lock();
        self.insert_file(inner, fd, file)
    }

    /// insert a file and fd into the file descriptor table
    fn insert_file(
        &self,
        mut inner: MutexGuard<LinuxProcessInner>,
        fd: FileDesc,
        file: Arc<dyn FileLike>,
    ) -> LxResult<FileDesc> {
        if inner.files.len() < inner.file_limit.cur as usize {
            inner.files.insert(fd, file);
            Ok(fd)
        } else {
            Err(LxError::EMFILE)
        }
    }

    /// get and set file limit number
    pub fn file_limit(&self, new_limit: Option<RLimit>) -> RLimit {
        let mut inner = self.inner.lock();
        let old = inner.file_limit;
        if let Some(limit) = new_limit {
            inner.file_limit = limit;
        }
        old
    }

    /// Get the `File` with given `fd`.
    pub fn get_file(&self, fd: FileDesc) -> LxResult<Arc<File>> {
        let file = self
            .get_file_like(fd)?
            .downcast_arc::<File>()
            .map_err(|_| LxError::EBADF)?;
        Ok(file)
    }

    /*
        /// Get the `Socket` with given `fd`.
        pub fn get_socket(&self, fd: FileDesc) -> LxResult<Arc<dyn Socket>> {
            let socket = self
                .get_file_like(fd)?
                .as_socket()
            .map_err(|_| LxError::EBADF)?;
            Ok(Arc::new(socket))
        }
    */

    /// Get the `FileLike` with given `fd`.
    pub fn get_file_like(&self, fd: FileDesc) -> LxResult<Arc<dyn FileLike>> {
        let inner = self.inner.lock();
        trace!("get_file_like: {:x?}", inner.files);
        inner.files.get(&fd).cloned().ok_or(LxError::EBADF)
    }

    /// get all files
    pub fn get_files(&self) -> LxResult<HashMap<FileDesc, Arc<dyn FileLike>>> {
        let inner = self.inner.lock();
        Ok(inner.files.clone())
    }

    /// Close file descriptor `fd`.
    pub fn close_file(&self, fd: FileDesc) -> LxResult {
        let mut inner = self.inner.lock();
        inner.files.remove(&fd).map(|_| ()).ok_or(LxError::EBADF)
    }

    /// Close all file descriptors between `first` and `last`.
    pub fn close_range(&self, first: FileDesc, last: FileDesc) {
        let mut inner = self.inner.lock();
        let fds: Vec<_> = inner
            .files
            .keys()
            .filter(|&&fd| fd >= first && fd <= last)
            .cloned()
            .collect();
        for fd in fds {
            inner.files.remove(&fd);
        }
    }

    /// Get root INode of the process.
    pub fn root_inode(&self) -> &Arc<dyn INode> {
        &self.root_inode
    }

    /// Get a snapshot of current credentials.
    pub fn credentials(&self) -> Credentials {
        self.inner.lock().credentials.clone()
    }

    /// Get real uid.
    pub fn uid(&self) -> u32 {
        self.inner.lock().credentials.ruid
    }

    /// Get effective uid.
    pub fn euid(&self) -> u32 {
        self.inner.lock().credentials.euid
    }

    /// Get saved uid.
    pub fn suid(&self) -> u32 {
        self.inner.lock().credentials.suid
    }

    /// Get real gid.
    pub fn gid(&self) -> u32 {
        self.inner.lock().credentials.rgid
    }

    /// Get effective gid.
    pub fn egid(&self) -> u32 {
        self.inner.lock().credentials.egid
    }

    /// Get saved gid.
    pub fn sgid(&self) -> u32 {
        self.inner.lock().credentials.sgid
    }

    /// Get supplementary groups.
    pub fn groups(&self) -> Vec<u32> {
        self.inner.lock().credentials.groups.clone()
    }

    /// Get umask.
    pub fn umask(&self) -> u16 {
        self.inner.lock().credentials.umask
    }

    /// Set umask and return the previous one.
    pub fn set_umask(&self, mask: u16) -> u16 {
        let mut inner = self.inner.lock();
        let old = inner.credentials.umask;
        inner.credentials.umask = mask & 0o777;
        old
    }

    /// Whether the current effective uid is root.
    pub fn is_superuser(&self) -> bool {
        self.euid() == ROOT_UID
    }

    /// Apply umask to file creation mode.
    pub fn apply_umask(&self, mode: u16) -> u16 {
        mode & !self.umask()
    }

    fn gid_in_groups(creds: &Credentials, gid: u32) -> bool {
        creds.egid == gid || creds.rgid == gid || creds.groups.iter().any(|group| *group == gid)
    }

    fn allowed_uid(creds: &Credentials, uid: u32) -> bool {
        uid == creds.ruid || uid == creds.euid || uid == creds.suid
    }

    fn allowed_gid(creds: &Credentials, gid: u32) -> bool {
        gid == creds.rgid || gid == creds.egid || gid == creds.sgid
    }

    fn check_requested_access(mode: u16, requested: u16) -> bool {
        requested == 0 || (mode & requested) == requested
    }

    fn access_bits_for(creds: &Credentials, metadata: &Metadata, use_effective: bool) -> u16 {
        let uid = if use_effective { creds.euid } else { creds.ruid };
        let gid = if use_effective { creds.egid } else { creds.rgid };
        if uid == ROOT_UID {
            return metadata.mode as u16 & 0o777;
        }
        if uid == metadata.uid as u32 {
            return ((metadata.mode as u16) >> 6) & 0o7;
        }
        let in_group = if use_effective {
            Self::gid_in_groups(creds, metadata.gid as u32)
        } else {
            gid == metadata.gid as u32
                || creds
                    .groups
                    .iter()
                    .any(|group| *group == metadata.gid as u32)
        };
        if in_group {
            return ((metadata.mode as u16) >> 3) & 0o7;
        }
        metadata.mode as u16 & 0o7
    }

    /// Check inode access against current credentials.
    pub fn check_access(&self, metadata: &Metadata, requested: u16, use_effective: bool) -> LxResult {
        let creds = self.credentials();
        let granted = Self::access_bits_for(&creds, metadata, use_effective);
        if creds.euid == ROOT_UID && requested == ACCESS_EXEC && granted & ACCESS_EXEC == 0 {
            return Err(LxError::EACCES);
        }
        if creds.euid == ROOT_UID
            && Self::check_requested_access(granted, requested & (ACCESS_READ | ACCESS_WRITE))
        {
            return Ok(());
        }
        if Self::check_requested_access(granted, requested) {
            Ok(())
        } else {
            Err(LxError::EACCES)
        }
    }

    /// Check inode access by fetching metadata first.
    pub fn check_inode_access(
        &self,
        inode: &Arc<dyn INode>,
        requested: u16,
        use_effective: bool,
    ) -> LxResult {
        let metadata = inode.metadata()?;
        self.check_access(&metadata, requested, use_effective)
    }

    /// Check parent directory mutation rights.
    pub fn check_directory_write(&self, inode: &Arc<dyn INode>) -> LxResult {
        self.check_inode_access(inode, ACCESS_WRITE | ACCESS_EXEC, true)
    }

    /// Check if sticky-directory removal/rename is allowed.
    pub fn check_sticky(
        &self,
        dir_metadata: &Metadata,
        target_metadata: &Metadata,
    ) -> LxResult {
        if (dir_metadata.mode as u16 & MODE_STICKY) == 0 {
            return Ok(());
        }
        let creds = self.credentials();
        if creds.euid == ROOT_UID
            || creds.euid == dir_metadata.uid as u32
            || creds.euid == target_metadata.uid as u32
        {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Change mode if current process is owner or root.
    pub fn chmod_metadata(&self, metadata: &mut Metadata, mode: u16) -> LxResult {
        let creds = self.credentials();
        if creds.euid != ROOT_UID && creds.euid != metadata.uid as u32 {
            return Err(LxError::EPERM);
        }
        metadata.mode = (metadata.mode as u16 & !MODE_PERM_MASK | (mode & MODE_PERM_MASK)) as _;
        if creds.euid != ROOT_UID {
            metadata.mode &= !(MODE_SET_UID | MODE_SET_GID) as u32;
        }
        Ok(())
    }

    /// Change owner/group following a conservative POSIX-compatible policy.
    pub fn chown_metadata(&self, metadata: &mut Metadata, uid: u32, gid: u32) -> LxResult {
        let creds = self.credentials();
        let privileged = creds.euid == ROOT_UID;
        if !privileged {
            if uid != NO_ID && uid != metadata.uid as u32 {
                return Err(LxError::EPERM);
            }
            if creds.euid != metadata.uid as u32 {
                return Err(LxError::EPERM);
            }
            if gid != NO_ID && !Self::gid_in_groups(&creds, gid) {
                return Err(LxError::EPERM);
            }
        }
        if uid != NO_ID {
            metadata.uid = uid as _;
        }
        if gid != NO_ID {
            metadata.gid = gid as _;
        }
        metadata.mode &= !(MODE_SET_UID | MODE_SET_GID) as u32;
        Ok(())
    }

    /// Set owner/group for a newly created inode.
    pub fn initialize_created_metadata(
        &self,
        inode: &Arc<dyn INode>,
        parent_metadata: Option<&Metadata>,
        mode: u16,
        is_dir: bool,
    ) -> LxResult {
        let creds = self.credentials();
        let mut metadata = inode.metadata()?;
        metadata.uid = creds.euid as _;
        metadata.gid = parent_metadata
            .filter(|meta| (meta.mode as u16 & MODE_SET_GID) != 0)
            .map(|meta| meta.gid)
            .unwrap_or(creds.egid as _);
        let mut final_mode = mode & MODE_PERM_MASK;
        if let Some(parent) = parent_metadata {
            if (parent.mode as u16 & MODE_SET_GID) != 0 && is_dir {
                final_mode |= MODE_SET_GID;
            }
        }
        metadata.mode = (metadata.mode as u16 & !MODE_PERM_MASK | final_mode) as _;
        inode.set_metadata(&metadata)?;
        Ok(())
    }

    /// Apply setuid/setgid exec transitions.
    pub fn apply_exec_metadata(&self, metadata: &Metadata) {
        let mut inner = self.inner.lock();
        if (metadata.mode as u16 & MODE_SET_UID) != 0 {
            inner.credentials.euid = metadata.uid as u32;
            inner.credentials.suid = metadata.uid as u32;
        }
        if (metadata.mode as u16 & MODE_SET_GID) != 0 {
            inner.credentials.egid = metadata.gid as u32;
            inner.credentials.sgid = metadata.gid as u32;
        }
    }

    /// Set supplementary groups.
    pub fn set_groups(&self, groups: Vec<u32>) {
        self.inner.lock().credentials.groups = groups;
    }

    /// Set uid according to current privileges.
    pub fn set_uid(&self, uid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if privileged {
            inner.credentials.ruid = uid;
            inner.credentials.euid = uid;
            inner.credentials.suid = uid;
            return Ok(());
        }
        if Self::allowed_uid(&inner.credentials, uid) {
            inner.credentials.euid = uid;
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Set gid according to current privileges.
    pub fn set_gid(&self, gid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if privileged {
            inner.credentials.rgid = gid;
            inner.credentials.egid = gid;
            inner.credentials.sgid = gid;
            return Ok(());
        }
        if Self::allowed_gid(&inner.credentials, gid) {
            inner.credentials.egid = gid;
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Set real/effective uid.
    pub fn set_reuid(&self, ruid: u32, euid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            if ruid != NO_ID && !Self::allowed_uid(&inner.credentials, ruid) {
                return Err(LxError::EPERM);
            }
            if euid != NO_ID && !Self::allowed_uid(&inner.credentials, euid) {
                return Err(LxError::EPERM);
            }
        }
        let old_ruid = inner.credentials.ruid;
        if ruid != NO_ID {
            inner.credentials.ruid = ruid;
        }
        if euid != NO_ID {
            inner.credentials.euid = euid;
        }
        if privileged || ruid != NO_ID || (euid != NO_ID && euid != old_ruid) {
            inner.credentials.suid = inner.credentials.euid;
        }
        Ok(())
    }

    /// Set real/effective gid.
    pub fn set_regid(&self, rgid: u32, egid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            if rgid != NO_ID && !Self::allowed_gid(&inner.credentials, rgid) {
                return Err(LxError::EPERM);
            }
            if egid != NO_ID && !Self::allowed_gid(&inner.credentials, egid) {
                return Err(LxError::EPERM);
            }
        }
        let old_rgid = inner.credentials.rgid;
        if rgid != NO_ID {
            inner.credentials.rgid = rgid;
        }
        if egid != NO_ID {
            inner.credentials.egid = egid;
        }
        if privileged || rgid != NO_ID || (egid != NO_ID && egid != old_rgid) {
            inner.credentials.sgid = inner.credentials.egid;
        }
        Ok(())
    }

    /// Set real/effective/saved uid.
    pub fn set_resuid(&self, ruid: u32, euid: u32, suid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            for uid in [ruid, euid, suid] {
                if uid != NO_ID && !Self::allowed_uid(&inner.credentials, uid) {
                    return Err(LxError::EPERM);
                }
            }
        }
        if ruid != NO_ID {
            inner.credentials.ruid = ruid;
        }
        if euid != NO_ID {
            inner.credentials.euid = euid;
        }
        if suid != NO_ID {
            inner.credentials.suid = suid;
        }
        Ok(())
    }

    /// Set real/effective/saved gid.
    pub fn set_resgid(&self, rgid: u32, egid: u32, sgid: u32) -> LxResult {
        let mut inner = self.inner.lock();
        let privileged = inner.credentials.euid == ROOT_UID;
        if !privileged {
            for gid in [rgid, egid, sgid] {
                if gid != NO_ID && !Self::allowed_gid(&inner.credentials, gid) {
                    return Err(LxError::EPERM);
                }
            }
        }
        if rgid != NO_ID {
            inner.credentials.rgid = rgid;
        }
        if egid != NO_ID {
            inner.credentials.egid = egid;
        }
        if sgid != NO_ID {
            inner.credentials.sgid = sgid;
        }
        Ok(())
    }

    /// Get parent process.
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.upgrade()
    }

    /// Get current working directory.
    pub fn current_working_directory(&self) -> String {
        String::from("/") + &self.inner.lock().current_working_directory
    }

    /// Get absolute path from dirfd and relative path.
    pub fn get_absolute_path(&self, dirfd: FileDesc, path: &str) -> LxResult<String> {
        if path.is_empty() {
            return Ok(String::from("/"));
        }
        let base_path = if path.starts_with('/') {
            String::new()
        } else if dirfd == FileDesc::CWD {
            self.inner.lock().current_working_directory.clone()
        } else {
            let file = self.get_file(dirfd)?;
            let file_path = file.path().clone();
            if file_path.starts_with('/') {
                String::from(&file_path[1..])
            } else {
                file_path
            }
        };
        let mut cwd_vec: Vec<_> = base_path.split('/').filter(|x| !x.is_empty()).collect();
        for seg in path.split('/') {
            match seg {
                ".." => {
                    cwd_vec.pop();
                }
                "." | "" => {}
                _ => cwd_vec.push(seg),
            }
        }
        Ok(String::from("/") + &cwd_vec.join("/"))
    }

    /// Change working directory.
    pub fn change_directory(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        let cwd = match path.as_bytes()[0] {
            b'/' => String::new(),
            _ => inner.current_working_directory.clone(),
        };
        let mut cwd_vec: Vec<_> = cwd.split('/').filter(|x| !x.is_empty()).collect();
        for seg in path.split('/') {
            match seg {
                ".." => {
                    cwd_vec.pop();
                }
                "." | "" => {} // nothing to do here.
                _ => cwd_vec.push(seg),
            }
        }
        inner.current_working_directory = cwd_vec.join("/");
    }

    /// Get execute path.
    pub fn execute_path(&self) -> String {
        self.inner.lock().execute_path.clone()
    }

    /// Set execute path.
    pub fn set_execute_path(&self, path: &str) {
        self.inner.lock().execute_path = String::from(path);
    }

    /// Get the current program break (top of heap).
    pub fn brk(&self) -> usize {
        self.inner.lock().brk
    }

    /// Set the current program break.
    pub fn set_brk(&self, brk: usize) {
        self.inner.lock().brk = brk;
    }

    /// Get signal action.
    pub fn signal_action(&self, signal: LinuxSignal) -> SignalAction {
        self.inner.lock().signal_actions.table[signal as u8 as usize]
    }

    /// Set signal action.
    pub fn set_signal_action(&self, signal: LinuxSignal, action: SignalAction) {
        self.inner.lock().signal_actions.table[signal as u8 as usize] = action;
    }

    /// Close file that FD_CLOEXEC is set
    pub fn remove_cloexec_files(&self) {
        let mut inner = self.inner.lock();
        let close_fds = inner
            .files
            .iter()
            .filter_map(|(fd, file_like)| {
                if file_like.flags().close_on_exec() {
                    Some(*fd)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for fd in close_fds {
            inner.files.remove(&fd).map(|_| ()).unwrap();
        }
    }

    /// Insert a `SemArray` and return its ID
    pub fn semaphores_add(&self, array: Arc<SemArray>) -> usize {
        self.inner.lock().semaphores.add(array)
    }

    /// Get an semaphore set by `id`
    pub fn semaphores_get(&self, id: usize) -> Option<Arc<SemArray>> {
        self.inner.lock().semaphores.get(id)
    }

    /// Add an undo operation
    pub fn semaphores_add_undo(&self, id: usize, num: u16, op: i16) {
        self.inner.lock().semaphores.add_undo(id, num, op)
    }

    /// Remove an `SemArray` by ID
    pub fn semaphores_remove(&self, id: usize) {
        self.inner.lock().semaphores.remove(id)
    }

    /// get ShmId from Virtual Addr
    pub fn shm_get_id(&self, id: usize) -> Option<usize> {
        self.inner.lock().shm_identifiers.get_id(id)
    }

    /// get the ShmIdentifier from shm_identifiers
    pub fn shm_get(&self, id: usize) -> Option<ShmIdentifier> {
        self.inner.lock().shm_identifiers.get(id)
    }

    /// Delete the ShmIdentifier from shm_identifiers
    pub fn shm_pop(&self, id: usize) {
        self.inner.lock().shm_identifiers.pop(id)
    }

    /// Insert the `SharedGuard` and return its ID
    pub fn shm_add(&self, shared_guard: Arc<Mutex<ShmGuard>>) -> usize {
        self.inner.lock().shm_identifiers.add(shared_guard)
    }

    /// Set Virtual Addr for shared memory
    pub fn shm_set(&self, id: usize, shm_id: ShmIdentifier) {
        self.inner.lock().shm_identifiers.set(id, shm_id)
    }
}

impl LinuxProcessInner {
    fn get_free_fd(&self) -> FileDesc {
        self.get_free_fd_from(0)
    }

    fn get_free_fd_from(&self, start: usize) -> FileDesc {
        (start..)
            .map(|i| i.into())
            .find(|fd| !self.files.contains_key(fd))
            .unwrap()
    }
}
pub fn check_and_deliver_tty_interrupt() -> LxResult<()> {
    if crate::fs::stdio::ctrl_c_pending_take() {
        if let Some(arc) = kernel_hal::thread::get_current_thread() {
            if let Ok(thread) = arc.downcast::<Thread>() {
                use crate::thread::ThreadExt;
                thread.lock_linux().signals.insert(LinuxSignal::SIGINT);
            }
        }
        return Err(LxError::EINTR);
    }
    check_signals()
}

/// Check for pending signals and return EINTR if any.
pub fn check_signals() -> LxResult<()> {
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            use crate::thread::ThreadExt;
            let linux_thread = thread.lock_linux();
            let pending = linux_thread.signals.mask_with(&linux_thread.signal_mask);
            if pending.is_not_empty() {
                return Err(LxError::EINTR);
            }
        }
    }
    Ok(())
}

/// Send a signal to a process by its KoID.
pub fn send_signal_to_process(pid: usize, signal: LinuxSignal) -> LxResult<()> {
    if let Some(process) = ROOT_JOB.find_process(pid as KoID) {
        let tids = process.thread_ids();
        for tid in tids {
            if let Ok(thread_obj) = process.get_child(tid) {
                if let Ok(thread) = thread_obj.downcast_arc::<Thread>() {
                    use crate::thread::ThreadExt;
                    let mut thread_linux = thread.lock_linux();
                    if thread_linux.signal_mask.contains(signal) {
                        continue;
                    } else {
                        thread_linux.signals.insert(signal);
                        break;
                    }
                }
            }
        }
        Ok(())
    } else {
        Err(LxError::ESRCH)
    }
}
