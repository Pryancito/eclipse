//! Minimal procfs implementation for Linux userland compatibility.

use alloc::{fmt::Write as _, string::String, sync::Arc, vec::Vec};
use core::any::Any;
use lazy_static::lazy_static;

use kernel_hal::drivers;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Status, Thread, ROOT_JOB};

use crate::process::ProcessExt;
use smoltcp::wire::{IpAddress, IpCidr};

const PROC_ROOT_STATIC: [&str; 10] = [
    "net", "meminfo", "cpuinfo", "swaps", "uptime", "mounts", "self", "stat", "loadavg", "sys",
];

fn collect_processes(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
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
                collect_processes(&child_job, out);
            }
        }
    }
}

fn all_processes() -> Vec<Arc<Process>> {
    let mut out = Vec::new();
    collect_processes(&ROOT_JOB, &mut out);
    out
}

fn current_process_id() -> Option<u64> {
    let arc = kernel_hal::thread::get_current_thread()?;
    let thread = arc.downcast::<Thread>().ok()?;
    Some(thread.proc().id() as u64)
}

fn sanitize_comm(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let mut s = String::new();
    for c in base.chars().take(15) {
        let ch = match c {
            '(' | ')' | '\0' => '_',
            _ => c,
        };
        s.push(ch);
    }
    if s.is_empty() {
        s.push_str("process");
    }
    s
}

fn proc_state_char(status: Status) -> char {
    match status {
        Status::Running => 'R',
        Status::Init => 'S',
        Status::Exited(_) => 'Z',
    }
}

fn proc_comm(proc: &Process) -> String {
    let path = proc.linux().execute_path();
    if !path.is_empty() {
        let base = path.rsplit('/').next().unwrap_or(&path);
        return sanitize_comm(base);
    }
    sanitize_comm(&proc.name())
}

fn proc_ppid(proc: &Process) -> u64 {
    proc.linux().parent().map(|p| p.id()).unwrap_or(0)
}

/// The first (leader) thread of a process, if any, for reporting its
/// scheduling attributes.
fn proc_first_thread(proc: &Process) -> Option<Arc<Thread>> {
    let id = *proc.thread_ids().first()?;
    proc.get_child(id).ok()?.downcast_arc::<Thread>().ok()
}

fn proc_pid_stat(proc: &Process) -> String {
    let pid = proc.id();
    let comm = proc_comm(proc);
    let state = proc_state_char(proc.status());
    let ppid = proc_ppid(proc);

    let nthreads = proc.thread_ids().len().max(1) as i64;
    // priority(18)/nice(19)/rt_priority(40)/policy(41) come from the leader
    // thread. Per proc(5): for real-time policies the priority field is
    // `-1 - rt_priority`; for the fair policies it is `20 + nice`.
    let (priority, nice, rt_priority, policy) = match proc_first_thread(proc) {
        Some(t) => {
            let rt = t.sched_rt_priority() as i64;
            let n = t.sched_nice() as i64;
            let prio = if t.sched_is_realtime() {
                -1 - rt
            } else {
                20 + n
            };
            (prio, n, rt, t.sched_policy() as i64)
        }
        None => (20, 0, 0, 0),
    };

    // Fields 5..=52 of /proc/[pid]/stat (proc(5)); 0 where not tracked. Indexed
    // by `field - 5` to keep the field numbers obvious.
    let mut rest = [0i64; 48];
    rest[18 - 5] = priority;
    rest[19 - 5] = nice;
    rest[20 - 5] = nthreads;
    rest[40 - 5] = rt_priority;
    rest[41 - 5] = policy;

    let mut out = format!("{} ({}) {} {}", pid, comm, state, ppid);
    for v in rest.iter() {
        let _ = write!(out, " {}", v);
    }
    out.push('\n');
    out
}

fn proc_pid_status(proc: &Process) -> String {
    let pid = proc.id();
    let name = proc_comm(proc);
    let ppid = proc_ppid(proc);
    let state = match proc.status() {
        Status::Running => "R (running)",
        Status::Init => "S (sleeping)",
        Status::Exited(_) => "Z (zombie)",
    };
    format!(
        "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n",
        name, state, pid, pid, ppid
    )
}

fn proc_pid_cmdline(proc: &Process) -> Vec<u8> {
    let args = proc.linux().cmdline();
    if !args.is_empty() {
        let mut out = Vec::new();
        for arg in args {
            out.extend_from_slice(arg.as_bytes());
            out.push(0);
        }
        return out;
    }
    let path = proc.linux().execute_path();
    let mut out = path.into_bytes();
    out.push(0);
    out
}

/// A minimal `procfs` with a few common files.
pub struct ProcFS;

impl ProcFS {
    /// Create a new procfs instance.
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for ProcFS {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        PROC_ROOT.clone()
    }

    fn info(&self) -> FsInfo {
        // Virtual FS: report conservative, non-zero values.
        FsInfo {
            bsize: 4096,
            frsize: 4096,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

struct ProcRootINode;

impl ProcRootINode {
    fn entry_name(id: usize) -> Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i if i - 2 < PROC_ROOT_STATIC.len() => Ok(PROC_ROOT_STATIC[i - 2].into()),
            i => {
                let idx = i - 2 - PROC_ROOT_STATIC.len();
                let procs = all_processes();
                if idx >= procs.len() {
                    return Err(FsError::EntryNotFound);
                }
                Ok(alloc::format!("{}", procs[idx].id()))
            }
        }
    }
}

impl INode for ProcRootINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 10,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            nlinks: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(PROC_ROOT.clone()),
            "net" => Ok(PROC_NET_DIR.clone()),
            "meminfo" => Ok(PROC_MEMINFO.clone()),
            "cpuinfo" => Ok(PROC_CPUINFO.clone()),
            "swaps" => Ok(PROC_SWAPS.clone()),
            "uptime" => Ok(PROC_UPTIME.clone()),
            "mounts" => Ok(PROC_MOUNTS.clone()),
            "stat" => Ok(PROC_STAT.clone()),
            "loadavg" => Ok(PROC_LOADAVG.clone()),
            "sys" => Ok(PROC_SYS_DIR.clone()),
            "self" => Ok(PROC_SELF_SYM.clone()),
            name => {
                if let Ok(pid) = name.parse::<u64>() {
                    if ROOT_JOB.find_process(pid as _).is_some() {
                        return Ok(Arc::new(ProcPidDirINode { pid }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        Self::entry_name(id)
    }
}

/// `/proc/<pid>/` — `stat`, `cmdline`, `status` for BusyBox `ps`.
struct ProcPidDirINode {
    pid: u64,
}

impl ProcPidDirINode {
    fn process(&self) -> Option<Arc<Process>> {
        ROOT_JOB.find_process(self.pid as _)
    }

    fn entries() -> [&'static str; 5] {
        [".", "..", "stat", "cmdline", "status"]
    }
}

impl INode for ProcPidDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 100 + self.pid as usize,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            nlinks: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        if self.process().is_none() {
            return Err(FsError::EntryNotFound);
        }
        match name {
            "." => Ok(Arc::new(ProcPidDirINode { pid: self.pid })),
            ".." => Ok(PROC_ROOT.clone()),
            "stat" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Stat,
            })),
            "cmdline" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Cmdline,
            })),
            "status" => Ok(Arc::new(ProcPidFileINode {
                pid: self.pid,
                kind: ProcPidFileKind::Status,
            })),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

struct ProcNetDirINode;

impl ProcNetDirINode {
    fn entries() -> [&'static str; 4] {
        ["dev", "route", "arp", "if_inet6"]
    }
}

impl INode for ProcNetDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 20,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            nlinks: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_NET_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "dev" => Ok(PROC_NET_DEV.clone()),
            "route" => Ok(PROC_NET_ROUTE.clone()),
            "arp" => Ok(PROC_NET_ARP.clone()),
            "if_inet6" => Ok(PROC_NET_IF_INET6.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

/// `/proc/sys` — only the `kernel/` subtree is populated (enough for `perf`).
struct ProcSysDirINode;

impl INode for ProcSysDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(40))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_DIR.clone()),
            ".." => Ok(PROC_ROOT.clone()),
            "kernel" => Ok(PROC_SYS_KERNEL_DIR.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("kernel".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

/// `/proc/sys/kernel` — the few knobs `perf` probes before profiling.
struct ProcSysKernelDirINode;

impl INode for ProcSysKernelDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(41))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(PROC_SYS_KERNEL_DIR.clone()),
            ".." => Ok(PROC_SYS_DIR.clone()),
            // -1 = no restrictions: let `perf` open kernel/CPU-wide events.
            "perf_event_paranoid" => Ok(PROC_PERF_PARANOID.clone()),
            "kptr_restrict" => Ok(PROC_KPTR_RESTRICT.clone()),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(".".into()),
            1 => Ok("..".into()),
            2 => Ok("perf_event_paranoid".into()),
            3 => Ok("kptr_restrict".into()),
            _ => Err(FsError::EntryNotFound),
        }
    }
}

fn dir_metadata(inode: usize) -> Metadata {
    Metadata {
        dev: 0,
        inode,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::Dir,
        mode: 0o555,
        nlinks: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

fn proc_perf_event_paranoid_content() -> String {
    String::from("-1\n")
}

fn proc_kptr_restrict_content() -> String {
    String::from("0\n")
}

/// Proc file that regenerates text on each read (no snapshot in `find()`).
struct ProcSeqINode {
    inode: usize,
    generate: fn() -> String,
}

fn seq_read_at(generate: fn() -> String, offset: usize, buf: &mut [u8]) -> Result<usize> {
    let content = generate();
    let bytes = content.as_bytes();
    if offset >= bytes.len() {
        return Ok(0);
    }
    let len = (bytes.len() - offset).min(buf.len());
    buf[..len].copy_from_slice(&bytes[offset..offset + len]);
    Ok(len)
}

impl INode for ProcSeqINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(self.generate, offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        // Linux reports size 0 for seq_file pseudo entries; content is generated on read.
        Ok(Metadata {
            dev: 0,
            inode: self.inode,
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o444,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

/// `/proc/self` — target pid is resolved on read, not at `find()` time.
struct ProcSelfSymINode;

impl INode for ProcSelfSymINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let target = current_process_id()
            .map(|id| alloc::format!("{}", id))
            .unwrap_or_else(|| "1".into());
        let bytes = target.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let target = current_process_id()
            .map(|id| alloc::format!("{}", id))
            .unwrap_or_else(|| "1".into());
        Ok(Metadata {
            dev: 0,
            inode: 12,
            size: target.len(),
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::SymLink,
            mode: 0o777,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

#[derive(Clone, Copy)]
enum ProcPidFileKind {
    Stat,
    Cmdline,
    Status,
}

/// `/proc/<pid>/{stat,cmdline,status}` without snapshotting at lookup time.
struct ProcPidFileINode {
    pid: u64,
    kind: ProcPidFileKind,
}

impl ProcPidFileINode {
    fn bytes(&self) -> Result<Vec<u8>> {
        let proc = ROOT_JOB
            .find_process(self.pid as _)
            .ok_or(FsError::EntryNotFound)?;
        Ok(match self.kind {
            ProcPidFileKind::Stat => proc_pid_stat(&proc).into_bytes(),
            ProcPidFileKind::Cmdline => proc_pid_cmdline(&proc),
            ProcPidFileKind::Status => proc_pid_status(&proc).into_bytes(),
        })
    }
}

impl INode for ProcPidFileINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let bytes = self.bytes()?;
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let size = self.bytes()?.len();
        Ok(Metadata {
            dev: 0,
            inode: 200 + self.pid as usize,
            size,
            blk_size: 4096,
            blocks: (size + 4095) / 4096,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o444,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

fn proc_net_dev_content() -> String {
    // Linux-like procfs content used by BusyBox `ifconfig`.
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Inter-|   Receive                                                |  Transmit"
    );
    let _ = writeln!(
        s,
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed"
    );

    let ifaces = kernel_hal::net::get_net_device();
    if ifaces.is_empty() {
        let _ = writeln!(s, "{:>6}: {}", "lo", "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0");
        return s;
    }

    for iface in ifaces.iter() {
        let name = iface.get_ifname();
        let stats = iface.get_stats();
        let _ = writeln!(
            s,
            "{:>6}: {:>7} {:>7} {:>4} {:>4}    0     0          0         0 {:>8} {:>8} {:>4} {:>4}    0     0       0          0",
            name,
            stats.rx_bytes,
            stats.rx_packets,
            stats.rx_errors,
            stats.rx_dropped,
            stats.tx_bytes,
            stats.tx_packets,
            stats.tx_errors,
            stats.tx_dropped,
        );
    }
    s
}

fn proc_net_route_content() -> String {
    use crate::net::ipv4_netmask;

    let mut s = String::new();
    let _ = writeln!(
        s,
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT"
    );

    let ifaces = drivers::all_net().as_vec();
    for iface in ifaces.iter() {
        let name = iface.get_ifname();
        for route in iface.get_routes() {
            if let IpCidr::Ipv4(dst_cidr) = route.dst {
                let dst = u32::from_ne_bytes(dst_cidr.address().0);
                let gateway = match route.gateway {
                    Some(IpAddress::Ipv4(gw)) => u32::from_ne_bytes(gw.0),
                    _ => 0,
                };
                let mask = u32::from_ne_bytes(ipv4_netmask(dst_cidr.prefix_len()).0);
                let flags = if route.gateway.is_some() {
                    0x0003 // RTF_UP | RTF_GATEWAY
                } else {
                    0x0001 // RTF_UP
                };

                let _ = writeln!(
                    s,
                    "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                    name, dst, gateway, flags, mask
                );
            }
        }
    }
    s
}

fn proc_uptime_content() -> String {
    // Format: "<uptime_seconds> <idle_seconds>\n"
    let now = kernel_hal::timer::timer_now();
    let uptime = now.as_secs_f64();
    // We don't currently track aggregated idle time; report 0.
    format!("{:.2} 0.00\n", uptime)
}

/// `/proc/stat` — aggregate CPU counters (BusyBox `top` reads this after chdir to `/proc`).
fn proc_stat_content() -> String {
    let procs = all_processes();
    let running = crate::loadavg::runnable_count();
    format!(
        "cpu  0 0 0 1 0 0 0 0\n\
         intr 0\n\
         ctxt 0\n\
         btime 0\n\
         processes {}\n\
         procs_running {}\n\
         procs_blocked 0\n",
        procs.len(),
        running
    )
}

/// `/proc/loadavg` — one-line load averages for `top` header.
fn proc_loadavg_content() -> String {
    let procs = all_processes();
    let total = procs.len().max(1);
    // Runnable count (excludes idle/blocked tasks), not the live-process count —
    // see `loadavg::runnable_count`. `+1` so the field is never below 1: the
    // process reading `/proc/loadavg` is itself runnable but is excluded by the
    // sampler's self-subtraction, and Linux always reports at least 1 here.
    let running = crate::loadavg::runnable_count() + 1;
    let last_pid = procs.last().map(|p| p.id()).unwrap_or(1);
    let [l1, l5, l15] = crate::loadavg::loadavg_f64();
    format!("{l1:.2} {l5:.2} {l15:.2} {running}/{total} {last_pid}\n")
}

fn proc_meminfo_content() -> String {
    let (used, total) = kernel_hal::mem::memory_usage();
    let free = total.saturating_sub(used);
    let mut s = String::with_capacity(128);
    let _ = writeln!(s, "MemTotal:     {:>10} kB", total / 1024);
    let _ = writeln!(s, "MemFree:      {:>10} kB", free / 1024);
    let _ = writeln!(s, "MemAvailable: {:>10} kB", free / 1024);
    let _ = writeln!(s, "Buffers:               0 kB");
    let _ = writeln!(s, "Cached:                0 kB");
    s
}

/// Minimal `/proc/cpuinfo` for fastfetch CPU detection on x86_64.
fn proc_cpuinfo_content() -> String {
    let mut brand = kernel_hal::cpu::cpu_brand();
    if brand.is_empty() {
        brand = "Eclipse CPU".into();
    }
    let cpu_count = kernel_hal::cpu::cpu_count() as usize;
    let mut s = String::new();
    for i in 0..cpu_count {
        #[cfg(target_arch = "x86_64")]
        {
            use core::fmt::Write;
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 vendor_id\t: GenuineIntel\n\
                 model name\t: {}\n\
                 stepping\t: 0\n\
                 cpu MHz\t\t: {:.3}\n\
                 cache size\t: 4096 KB\n\
                 physical id\t: {}\n\
                 core id\t\t: {}\n\
                 cpu cores\t: {}",
                i,
                brand,
                kernel_hal::cpu::cpu_frequency() as f64,
                i,
                i,
                cpu_count
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            use core::fmt::Write;
            let _ = writeln!(
                s,
                "processor\t: {}\n\
                 model name\t: {}\n\
                 cpu cores\t: {}",
                i, brand, cpu_count
            );
        }
    }
    s
}

/// Empty swap table (header only) — fastfetch falls back to meminfo for swap stats.
fn proc_swaps_content() -> String {
    "Filename\t\tType\t\tSize\t\tUsed\t\tPriority\n".into()
}

fn proc_mounts_content() -> String {
    super::proc_mounts_content()
}

fn proc_net_arp_content() -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "IP address       HW type     Flags       HW address            Mask     Device"
    );
    let entries = crate::net::arp_cache::get_entries();
    for (ip, mac) in entries {
        let dev_name = if let Ok(dev) = crate::net::netdev_for_ipv4(ip) {
            dev.get_ifname()
        } else {
            kernel_hal::net::get_net_device()
                .iter()
                .find(|d| d.get_ifname() != "loopback")
                .map(|d| d.get_ifname())
                .unwrap_or_else(|| "eth0".into())
        };
        let mac_bytes = mac.as_bytes();
        let _ = writeln!(
            s,
            "{:<15}  0x1         0x2         {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}     *        {}",
            ip,
            mac_bytes[0],
            mac_bytes[1],
            mac_bytes[2],
            mac_bytes[3],
            mac_bytes[4],
            mac_bytes[5],
            dev_name
        );
    }
    s
}

fn proc_net_if_inet6_content() -> String {
    let mut s = String::new();
    let ifaces = kernel_hal::net::get_net_device();
    for (idx, iface) in ifaces.iter().enumerate() {
        crate::net::ensure_ipv6_link_local(iface.as_ref());
        let name = iface.get_ifname();
        let ifindex = idx + 1;
        for ip in iface.get_ip_address() {
            if let IpCidr::Ipv6(cidr) = ip {
                let addr = cidr.address();
                if addr.is_unspecified() {
                    continue;
                }
                let mut addr_hex = String::new();
                for &byte in addr.as_bytes() {
                    let _ = write!(addr_hex, "{:02x}", byte);
                }
                let ifindex_hex = format!("{:08x}", ifindex);
                let prefix_hex = format!("{:02x}", cidr.prefix_len());
                let scope_hex = if addr.is_loopback() {
                    "10"
                } else if addr.is_link_local() {
                    "20"
                } else {
                    "00"
                };
                let flags_hex = if addr.is_loopback() { "80" } else { "00" };
                let _ = writeln!(
                    s,
                    "{} {} {} {} {} {}",
                    addr_hex, ifindex_hex, prefix_hex, scope_hex, flags_hex, name
                );
            }
        }
    }
    s
}

/// Resolve an absolute `/proc/...` path without walking the ext2 backing store.
pub(crate) fn lookup_path(path: &str, follow_times: usize) -> Result<Arc<dyn INode>> {
    let path = path.trim_end_matches('/');
    if path == "/proc" {
        return Ok(PROC_ROOT.clone());
    }
    let rest = path.strip_prefix("/proc/").ok_or(FsError::EntryNotFound)?;
    if rest.is_empty() {
        return Ok(PROC_ROOT.clone());
    }
    PROC_ROOT.lookup_follow(rest, follow_times)
}

lazy_static! {
    static ref PROC_ROOT: Arc<dyn INode> = Arc::new(ProcRootINode);
    static ref PROC_NET_DIR: Arc<dyn INode> = Arc::new(ProcNetDirINode);
    static ref PROC_SYS_DIR: Arc<dyn INode> = Arc::new(ProcSysDirINode);
    static ref PROC_SYS_KERNEL_DIR: Arc<dyn INode> = Arc::new(ProcSysKernelDirINode);
    static ref PROC_PERF_PARANOID: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 42,
        generate: proc_perf_event_paranoid_content,
    });
    static ref PROC_KPTR_RESTRICT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 43,
        generate: proc_kptr_restrict_content,
    });
    static ref PROC_SELF_SYM: Arc<dyn INode> = Arc::new(ProcSelfSymINode);
    static ref PROC_MEMINFO: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 11,
        generate: proc_meminfo_content,
    });
    static ref PROC_CPUINFO: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 12,
        generate: proc_cpuinfo_content,
    });
    static ref PROC_SWAPS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 18,
        generate: proc_swaps_content,
    });
    static ref PROC_UPTIME: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 13,
        generate: proc_uptime_content,
    });
    static ref PROC_MOUNTS: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 14,
        generate: proc_mounts_content,
    });
    static ref PROC_STAT: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 15,
        generate: proc_stat_content,
    });
    static ref PROC_LOADAVG: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 16,
        generate: proc_loadavg_content,
    });
    static ref PROC_NET_DEV: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 30,
        generate: proc_net_dev_content,
    });
    static ref PROC_NET_ROUTE: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 31,
        generate: proc_net_route_content,
    });
    static ref PROC_NET_ARP: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 32,
        generate: proc_net_arp_content,
    });
    static ref PROC_NET_IF_INET6: Arc<dyn INode> = Arc::new(ProcSeqINode {
        inode: 33,
        generate: proc_net_if_inet6_content,
    });
}
