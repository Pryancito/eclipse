//! Minimal procfs implementation for Linux userland compatibility.

use alloc::{fmt::Write as _, string::String, sync::Arc};
use core::any::Any;

use kernel_hal::drivers;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};

use crate::fs::pseudo::Pseudo;
use smoltcp::wire::{IpAddress, IpCidr};

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
    fn entries() -> [&'static str; 4] {
        ["net", "meminfo", "uptime", "mounts"]
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
            mode: 0,
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
            mode: 0,
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
            mode: 0,
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