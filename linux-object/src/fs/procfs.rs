//! Minimal procfs implementation for Linux userland compatibility.

use alloc::{fmt::Write as _, string::String, sync::Arc, vec::Vec};
use core::any::Any;

use kernel_hal::drivers;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zircon_object::object::KernelObject;
use zircon_object::task::{Job, Process, Status, Thread, ROOT_JOB};

use crate::fs::pseudo::Pseudo;
use crate::process::ProcessExt;
use smoltcp::wire::{IpAddress, IpCidr};

const PROC_ROOT_STATIC: [&str; 7] = [
    "net", "meminfo", "uptime", "mounts", "self", "stat", "loadavg",
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

fn proc_pid_stat(proc: &Process) -> String {
    let pid = proc.id();
    let comm = proc_comm(proc);
    let state = proc_state_char(proc.status());
    let ppid = proc_ppid(proc);
    format!(
        "{} ({}) {} {} 0 0 0 0 0 4194560 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid, comm, state, ppid
    )
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
        Arc::new(ProcRootINode)
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
        if id < PROC_ROOT_STATIC.len() {
            return Ok(PROC_ROOT_STATIC[id].into());
        }
        let idx = id - PROC_ROOT_STATIC.len();
        let procs = all_processes();
        if idx >= procs.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(alloc::format!("{}", procs[idx].id()))
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
            "." => Ok(Arc::new(ProcRootINode)),
            ".." => Ok(Arc::new(ProcRootINode)),
            "net" => Ok(Arc::new(ProcNetDirINode)),
            "meminfo" => Ok(Arc::new(Pseudo::new(
                &proc_meminfo_content(),
                FileType::File,
            ))),
            "uptime" => Ok(Arc::new(Pseudo::new(
                &proc_uptime_content(),
                FileType::File,
            ))),
            "mounts" => Ok(Arc::new(Pseudo::new(
                &proc_mounts_content(),
                FileType::File,
            ))),
            "stat" => Ok(Arc::new(Pseudo::new(
                &proc_stat_content(),
                FileType::File,
            ))),
            "loadavg" => Ok(Arc::new(Pseudo::new(
                &proc_loadavg_content(),
                FileType::File,
            ))),
            "self" => {
                let target = current_process_id()
                    .map(|id| alloc::format!("{}", id))
                    .unwrap_or_else(|| "1".into());
                Ok(Arc::new(Pseudo::new(&target, FileType::SymLink)))
            }
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
        let proc = self.process().ok_or(FsError::EntryNotFound)?;
        match name {
            "." => Ok(Arc::new(ProcPidDirINode { pid: self.pid })),
            ".." => Ok(Arc::new(ProcRootINode)),
            "stat" => Ok(Arc::new(Pseudo::new(
                &proc_pid_stat(&proc),
                FileType::File,
            ))),
            "cmdline" => Ok(Arc::new(Pseudo::new_bytes(
                proc_pid_cmdline(&proc),
                FileType::File,
            ))),
            "status" => Ok(Arc::new(Pseudo::new(
                &proc_pid_status(&proc),
                FileType::File,
            ))),
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
            "." => Ok(Arc::new(ProcNetDirINode)),
            ".." => Ok(Arc::new(ProcRootINode)),
            "dev" => Ok(Arc::new(ProcNetDevINode)),
            "route" => Ok(Arc::new(Pseudo::new(
                &proc_net_route_content(),
                FileType::File,
            ))),
            "arp" => Ok(Arc::new(Pseudo::new(
                &proc_net_arp_content(),
                FileType::File,
            ))),
            "if_inet6" => Ok(Arc::new(Pseudo::new(
                &proc_net_if_inet6_content(),
                FileType::File,
            ))),
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

/// `/proc/net/dev` — regenerated on every read so BusyBox `ifconfig` sees live counters.
struct ProcNetDevINode;

impl INode for ProcNetDevINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let content = proc_net_dev_content();
        let bytes = content.as_bytes();
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
        let size = proc_net_dev_content().len();
        Ok(Metadata {
            dev: 0,
            inode: 30,
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
    let running = procs
        .iter()
        .filter(|p| matches!(p.status(), Status::Running))
        .count();
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
    let running = procs
        .iter()
        .filter(|p| matches!(p.status(), Status::Running))
        .count();
    let last_pid = procs.last().map(|p| p.id()).unwrap_or(1);
    format!("0.00 0.00 0.00 {}/{total} {last_pid}\n", running)
}

fn proc_meminfo_content() -> String {
    let (used, total) = kernel_hal::mem::memory_usage();
    let free = total.saturating_sub(used);
    let mut s = String::new();
    let _ = writeln!(s, "MemTotal:     {:>10} kB", total / 1024);
    let _ = writeln!(s, "MemFree:      {:>10} kB", free / 1024);
    let _ = writeln!(s, "MemAvailable: {:>10} kB", free / 1024);
    let _ = writeln!(s, "Buffers:               0 kB");
    let _ = writeln!(s, "Cached:                0 kB");
    s
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
                let flags_hex = if addr.is_loopback() {
                    "80"
                } else {
                    "00"
                };
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