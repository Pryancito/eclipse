//! Linux Process

use crate::{
    error::{LxError, LxResult},
    fs::{File, FileDesc, FileLike, OpenFlags},
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
use rcore_fs::vfs::{FileSystem, FileType, INode, Metadata};

use zircon_object::{
    object::{KernelObject, KoID, Signal},
    signal::Futex,
    task::{Job, Process, Status, Thread, ROOT_JOB},
    ZxResult,
};

pub use rcore_fs::vfs::FsInfo;

/// Process extension for linux
pub trait ProcessExt {
    /// create Linux process with a fixed Linux PID (`pid`): 1 for init, or the
    /// reserved 101.. range for the per-terminal shells.
    fn create_linux(
        job: &Arc<Job>,
        rootfs: Arc<dyn FileSystem>,
        vt: usize,
        shared_root: Option<Arc<dyn INode>>,
        pid: KoID,
    ) -> ZxResult<Arc<Self>>;
    /// get linux process
    fn linux(&self) -> &LinuxProcess;
    /// fork from current linux process
    fn fork_from(parent: &Arc<Self>, vfork: bool) -> ZxResult<Arc<Self>>;
}

const ROOT_UID: u32 = 0;
const NO_ID: u32 = u32::MAX;
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
    fn create_linux(
        job: &Arc<Job>,
        rootfs: Arc<dyn FileSystem>,
        vt: usize,
        shared_root: Option<Arc<dyn INode>>,
        pid: KoID,
    ) -> ZxResult<Arc<Self>> {
        let linux_proc = match shared_root {
            Some(root) => LinuxProcess::with_root(root, vt),
            None => LinuxProcess::new(rootfs, vt),
        };
        // Each process is given an explicit, stable Linux PID by the boot code:
        // 1 for init and the reserved 101.. range for the per-terminal shells.
        // (Reusing PID 1 for every VT shell once made `top`/`ps` list PID 1 N
        // times and made `find_process(1)`, signals, `kill` and `/proc/1` all
        // resolve to whichever process happened to be enumerated first.)
        let proc = Process::create_with_fixed_id_ext(job, pid, "root", linux_proc)?;
        let weak_proc = Arc::downgrade(&proc);
        proc.add_signal_callback(Box::new(move |signal| {
            if signal.contains(Signal::PROCESS_TERMINATED) {
                if let Some(proc) = weak_proc.upgrade() {
                    let mut inner = proc.linux().inner.lock();
                    inner.files.clear();
                    inner.futexes.clear();
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
                cmdline: linux_parent_inner.cmdline.clone(),
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
                if let Some(child) = weak_proc.upgrade() {
                    let exit_code = match child.status() {
                        Status::Exited(code) => code,
                        _ => 0,
                    };
                    {
                        let mut inner = child.linux().inner.lock();
                        inner.files.clear();
                        inner.futexes.clear();
                        inner.semaphores = Default::default();
                        inner.shm_identifiers = Default::default();
                    }
                    parent.linux().record_child_exit(child.id(), exit_code);
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
pub async fn wait_child(
    proc: &Arc<Process>,
    pid: KoID,
    nonblock: bool,
    reap: bool,
) -> LxResult<ExitCode> {
    loop {
        {
            let mut inner = proc.linux().inner.lock();
            if let Some(code) = inner.reaped_children.get(&pid) {
                let code = *code;
                if reap {
                    inner.reaped_children.remove(&pid);
                }
                return Ok((code as i32) << 8);
            }
        }
        let child = {
            let inner = proc.linux().inner.lock();
            inner.children.get(&pid).cloned().ok_or(LxError::ECHILD)?
        };
        if let Status::Exited(code) = child.status() {
            if reap {
                let mut inner = proc.linux().inner.lock();
                inner.children.remove(&pid);
                inner.reaped_children.remove(&pid);
            }
            return Ok((code as i32) << 8);
        }
        if nonblock {
            return Err(LxError::EAGAIN);
        }
        let child_obj: Arc<dyn KernelObject> = child.clone();
        child_obj.wait_signal(Signal::PROCESS_TERMINATED).await;

        // Check again after wait
        if let Status::Exited(code) = child.status() {
            if reap {
                let mut inner = proc.linux().inner.lock();
                inner.children.remove(&pid);
                inner.reaped_children.remove(&pid);
            }
            return Ok((code as i32) << 8);
        }
        continue;
    }
}

/// Wait for state changes in a child of the calling process.
pub async fn wait_child_any(
    proc: &Arc<Process>,
    nonblock: bool,
    reap: bool,
) -> LxResult<(KoID, ExitCode)> {
    loop {
        let mut inner = proc.linux().inner.lock();
        if inner.children.is_empty() && inner.reaped_children.is_empty() {
            return Err(LxError::ECHILD);
        }
        if let Some((pid, code)) = inner.reaped_children.iter().next().map(|(&p, &c)| (p, c)) {
            if reap {
                inner.reaped_children.remove(&pid);
            }
            return Ok((pid, (code as i32) << 8));
        }
        let mut exited_pid = None;
        trace!("wait_child_any: checking {} children", inner.children.len());
        for (&pid, child) in inner.children.iter() {
            let status = child.status();
            trace!("  child {}: status={:?}", pid, status);
            if let Status::Exited(code) = status {
                exited_pid = Some((pid, code));
                break;
            }
        }
        if let Some((pid, code)) = exited_pid {
            trace!("wait_child_any: reaping child {}", pid);
            if reap {
                inner.children.remove(&pid);
                inner.reaped_children.remove(&pid);
            }
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
            trace!("wait_child_any: found exited child after clear, continuing");
            continue;
        }
        trace!("wait_child_any: waiting for SIGCHLD");
        proc_obj.wait_signal(Signal::SIGCHLD).await;
        trace!("wait_child_any: woke up from SIGCHLD");
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
    /// argv as seen by userland (`/proc/<pid>/cmdline`)
    cmdline: Vec<String>,
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
    /// Exit codes for children already detached (freed `Arc<Process>` at exit).
    reaped_children: HashMap<KoID, i64>,
    /// Signal actions
    signal_actions: SignalActions,
    /// Program break (top of heap).
    ///
    /// Initialized to 0; set to the end of the loaded ELF image by the loader
    /// via [`LinuxProcess::set_brk`] before the first user instruction runs.
    /// Updated by `sys_brk` as the heap grows or shrinks.
    brk: usize,
    /// Upper bound of the address range actually mapped to back the heap.
    ///
    /// `sys_brk` reserves heap pages in [`BRK_CHUNK`]-sized strides instead of
    /// one VMO per user-visible grow, so the user-visible `brk` typically
    /// trails `mapped_brk`. Lazily initialised on first `sys_brk`: a value of
    /// 0 means "matches `brk`".
    mapped_brk: usize,
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
    /// Drop the live child handle and keep only the exit code for a future `wait`.
    pub fn record_child_exit(&self, child_id: KoID, exit_code: i64) {
        let mut inner = self.inner.lock();
        inner.children.remove(&child_id);
        inner.reaped_children.insert(child_id, exit_code);
    }

    /// Create a new process bound to virtual terminal `vt`, building a fresh
    /// root filesystem.
    pub fn new(rootfs: Arc<dyn FileSystem>, vt: usize) -> Self {
        Self::with_root(crate::fs::create_root_fs(rootfs), vt)
    }

    /// Create a new process reusing an already-built root filesystem (shared by
    /// `Arc`, like `fork`), bound to virtual terminal `vt`. Used to spawn the
    /// extra per-VT shells without re-scanning disks / re-mounting.
    pub fn with_root(root_inode: Arc<dyn INode>, vt: usize) -> Self {
        let stdin = File::new(
            crate::fs::stdio::vt_stdin(vt),
            OpenFlags::RDONLY,
            String::from("/dev/stdin"),
        ) as Arc<dyn FileLike>;
        let stdout_dev = crate::fs::stdio::vt_stdout(vt);
        let stdout = File::new(
            stdout_dev.clone(),
            OpenFlags::WRONLY,
            String::from("/dev/stdout"),
        ) as Arc<dyn FileLike>;
        let stderr = File::new(
            stdout_dev,
            OpenFlags::WRONLY,
            String::from("/dev/stderr"),
        ) as Arc<dyn FileLike>;
        let mut files = HashMap::new();
        files.insert(0.into(), stdin);
        files.insert(1.into(), stdout);
        files.insert(2.into(), stderr);

        LinuxProcess {
            root_inode,
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
    ///
    /// Returns `None` if `uaddr` is null or not aligned for an `AtomicI32`;
    /// dereferencing such an address would otherwise fault or be undefined
    /// behaviour.
    #[allow(unsafe_code)]
    pub fn get_futex(&self, uaddr: VirtAddr) -> Option<Arc<Futex>> {
        if uaddr == 0 || uaddr % core::mem::align_of::<AtomicI32>() != 0 {
            return None;
        }
        let mut inner = self.inner.lock();
        Some(
            inner
                .futexes
                .entry(uaddr)
                .or_insert_with(|| {
                    let value = unsafe { &*(uaddr as *const AtomicI32) };
                    Futex::new(value)
                })
                .clone(),
        )
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

    /// Whether `pid` is a tracked child of this process (live or not yet reaped).
    pub fn has_child(&self, pid: KoID) -> bool {
        let inner = self.inner.lock();
        inner.children.contains_key(&pid) || inner.reaped_children.contains_key(&pid)
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
        let uid = if use_effective {
            creds.euid
        } else {
            creds.ruid
        };
        let gid = if use_effective {
            creds.egid
        } else {
            creds.rgid
        };
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
    pub fn check_access(
        &self,
        metadata: &Metadata,
        requested: u16,
        use_effective: bool,
    ) -> LxResult {
        let creds = self.credentials();
        let selected_uid = if use_effective {
            creds.euid
        } else {
            creds.ruid
        };
        let granted = Self::access_bits_for(&creds, metadata, use_effective);
        if selected_uid == ROOT_UID {
            // CAP_DAC_OVERRIDE semantics: root bypasses permission checks
            // except executing a non-directory with no exec bit set anywhere
            // (mode & 0o111 == 0). Directories are always searchable by root;
            // testing only the others-exec bit here used to lock root out of
            // 0700 directories (e.g. apk's /lib/apk/exec, breaking triggers).
            if requested & ACCESS_EXEC != 0
                && metadata.type_ != FileType::Dir
                && metadata.mode as u16 & 0o111 == 0
            {
                return Err(LxError::EACCES);
            }
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
    pub fn check_sticky(&self, dir_metadata: &Metadata, target_metadata: &Metadata) -> LxResult {
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
            metadata.mode &= !(MODE_SET_UID | MODE_SET_GID);
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
        metadata.mode &= !(MODE_SET_UID | MODE_SET_GID);
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

    /// Set argv for `/proc/<pid>/cmdline`.
    pub fn set_cmdline(&self, args: Vec<String>) {
        self.inner.lock().cmdline = args;
    }

    /// Get argv.
    pub fn cmdline(&self) -> Vec<String> {
        self.inner.lock().cmdline.clone()
    }

    /// Get the current program break (top of heap).
    pub fn brk(&self) -> usize {
        self.inner.lock().brk
    }

    /// Set the current program break.
    pub fn set_brk(&self, brk: usize) {
        self.inner.lock().brk = brk;
    }

    /// Get the heap address actually mapped by `sys_brk` (>= the user-visible
    /// `brk` whenever the heap has been grown in chunks). Zero before the
    /// first `sys_brk`.
    pub fn mapped_brk(&self) -> usize {
        self.inner.lock().mapped_brk
    }

    /// Record the new upper bound of the heap's actual mapping.
    pub fn set_mapped_brk(&self, mapped_brk: usize) {
        self.inner.lock().mapped_brk = mapped_brk;
    }

    /// Get signal action.
    pub fn signal_action(&self, signal: LinuxSignal) -> SignalAction {
        self.inner.lock().signal_actions.table[signal as u8 as usize]
    }

    /// Set signal action.
    pub fn set_signal_action(&self, signal: LinuxSignal, action: SignalAction) {
        self.inner.lock().signal_actions.table[signal as u8 as usize] = action;
    }

    /// Reset signal dispositions across `execve`, as POSIX requires: every
    /// signal that was being *caught* (a custom handler) is restored to
    /// `SIG_DFL`; signals set to `SIG_IGN` or already `SIG_DFL` are left
    /// untouched. Without this, a `fork`+`exec`'d child keeps the parent
    /// shell's handler addresses; since busybox is one static binary, those
    /// addresses are still valid code in the new image, so a delivered signal
    /// (e.g. SIGINT) jumps into the shell's handler with the new applet's
    /// uninitialised globals (`ptr_to_globals == NULL`) and crashes.
    pub fn reset_signal_actions_for_exec(&self) {
        use crate::signal::{SIG_DFL, SIG_IGN};
        let mut inner = self.inner.lock();
        for action in inner.signal_actions.table.iter_mut() {
            if action.handler != SIG_DFL && action.handler != SIG_IGN {
                *action = SignalAction::default();
            }
        }
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
/// Deliver SIGINT to the foreground terminal process group (job control).
pub fn deliver_sigint_to_foreground() {
    let pgid = crate::fs::stdio::get_foreground_pgrp();
    if pgid > 0 {
        let _ = send_signal_to_process(pgid as usize, LinuxSignal::SIGINT);
        return;
    }
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            let _ = send_signal_to_process(thread.proc().id() as usize, LinuxSignal::SIGINT);
        }
    }
}

pub fn check_and_deliver_tty_interrupt() -> LxResult<()> {
    if crate::fs::stdio::ctrl_c_pending_take() {
        deliver_sigint_to_foreground();
        return Err(LxError::EINTR);
    }
    check_signals()
}

fn collect_live_processes(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
    for id in job.process_ids() {
        if let Some(proc) = job.find_process(id) {
            if !matches!(proc.status(), Status::Exited(_)) {
                out.push(proc);
            }
        }
    }
    for child_id in job.children_ids() {
        if let Ok(child) = job.get_child(child_id) {
            if let Ok(child_job) = child.downcast_arc::<Job>() {
                collect_live_processes(&child_job, out);
            }
        }
    }
}

/// All non-exited processes in the root job tree.
pub fn all_live_processes() -> Vec<Arc<Process>> {
    let mut processes = Vec::new();
    collect_live_processes(&ROOT_JOB, &mut processes);
    processes
}

/// Insert `signal` into one unmasked thread of each live process under `ROOT_JOB`.
pub fn send_signal_to_all_processes(signal: LinuxSignal) -> LxResult<()> {
    let processes = all_live_processes();
    let mut any = false;
    for proc in processes {
        if send_signal_to_process(proc.id() as usize, signal).is_ok() {
            any = true;
        }
    }
    if any {
        Ok(())
    } else {
        Err(LxError::ESRCH)
    }
}

/// Check for pending signals and return EINTR if any.
pub fn check_signals() -> LxResult<()> {
    if let Some(arc) = kernel_hal::thread::get_current_thread() {
        if let Ok(thread) = arc.downcast::<Thread>() {
            use crate::thread::ThreadExt;
            use zircon_object::task::ThreadState;
            if thread.state() == ThreadState::Dying {
                return Err(LxError::EINTR);
            }
            if matches!(thread.proc().status(), Status::Exited(_)) {
                return Err(LxError::EINTR);
            }
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
