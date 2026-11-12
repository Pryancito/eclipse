//! The microkernel's servers, for `lunarmon`'s SERVIDORES list: the
//! processes that drive a device the kernel left to userspace
//! (`udriver=` on its command line) and the stack that may sit on one.
//!
//! What is put together here, all of it read and none of it asked:
//!
//! - eclipse-init's state file: which services drive a device (its
//!   `udriver` column), their PID, and how often init has started them;
//! - each server's own file under `/run/eclipse-servers` (see
//!   `udrivers::status` in tools/eclipse-udrivers): the program, the card's
//!   MAC or the disk's size, and for a stack the driver it runs and how many
//!   it has started, which init never sees;
//! - `/sys/class/net/*/address` and `/sys/block/*/size`, for what the
//!   kernel made of the device: `eth0`, `vdb`.
//!
//! A file whose `pid` is not a live process is one that has gone and says
//! nothing. Pure over its inputs, so the tests need no `/run`.

use crate::sysmon::Svc;
use std::path::Path;

/// Where the servers write about themselves.
pub const DIR: &str = "/run/eclipse-servers";

/// One file of [`DIR`]: its name (`driver.virtio-net@0`) and `key=value`s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Said {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

impl Said {
    pub fn parse(name: &str, text: &str) -> Said {
        let fields = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        Said { name: name.to_string(), fields }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn pid(&self) -> Option<i32> {
        self.get("pid")?.parse().ok()
    }
}

/// Every file of `dir`, parsed. None at all when there is no `dir`.
pub fn read_dir(dir: &Path) -> Vec<Said> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Said> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.ends_with(".tmp") {
                return None;
            }
            Some(Said::parse(&name, &std::fs::read_to_string(e.path()).ok()?))
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `(name, value)` of one file in each directory of `dir`
/// (`/sys/class/net/eth0/address`), trimmed.
pub fn read_each(dir: &Path, file: &str) -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, String)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let v = std::fs::read_to_string(e.path().join(file)).ok()?;
            Some((name, v.trim().to_string()))
        })
        .collect();
    out.sort();
    out
}

/// What the kernel's side looks like: interfaces by MAC and disks by size.
#[derive(Clone, Debug, Default)]
pub struct Kernel {
    /// `(eth0, 52:54:00:12:34:56)`.
    pub ifaces: Vec<(String, String)>,
    /// `(vdb, sectors)`.
    pub disks: Vec<(String, String)>,
}

impl Kernel {
    pub fn read() -> Kernel {
        Kernel {
            ifaces: read_each(Path::new("/sys/class/net"), "address"),
            disks: read_each(Path::new("/sys/block"), "size"),
        }
    }
}

/// What a server gives the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gives {
    /// A kernel interface (`eth0`), or `None` when none has its MAC yet.
    Iface(Option<String>, String),
    /// An address of the stack's own: the kernel has no interface for it.
    Stack(String, String),
    /// A kernel disk (`vdb`, or `None` when no disk, or several, have its
    /// size) and its size in sectors.
    Disk(Option<String>, u64),
    /// The server has not said yet.
    Unknown,
}

/// One server as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// The init service (`unet`).
    pub service: String,
    pub state: String,
    /// The device (`virtio-net@0`).
    pub device: String,
    /// The program that serves it: what the service runs (`eclipse-netstack`,
    /// `udrv-virtio-blk`).
    pub program: String,
    /// The service's process.
    pub pid: Option<i32>,
    /// The driver's process, when it is another (a stack's driver).
    pub driver_pid: Option<i32>,
    pub gives: Gives,
    /// Times init started the service again this boot.
    pub init_restarts: u32,
    /// Times a stack started its driver again, which init never sees.
    pub driver_restarts: u32,
}

impl Server {
    pub fn restarts(&self) -> u32 {
        self.init_restarts + self.driver_restarts
    }
}

fn base(exec: &str) -> &str {
    let first = exec.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

/// The servers among `svcs`: each service with a device, with what its
/// processes said (`said`, of which only the live ones count, by `alive`)
/// and what the kernel shows (`kernel`).
pub fn servers(svcs: &[Svc], said: &[Said], kernel: &Kernel, alive: &dyn Fn(i32) -> bool) -> Vec<Server> {
    let live = |name: &str| {
        said.iter()
            .find(|s| s.name == name)
            .filter(|s| s.pid().is_some_and(alive))
    };
    svcs.iter()
        .filter_map(|svc| {
            // A oneshot with a device (`ublk-mounts`, which mounts what the
            // driver serves) runs at boot and is no server.
            if svc.kind != "respawn" {
                return None;
            }
            let device = svc.udriver.clone()?;
            let running = svc.pid.is_some();
            let stack = live(&format!("stack.{device}")).filter(|s| running && s.pid() == svc.pid);
            let driver = live(&format!("driver.{device}")).filter(|_| running);
            let program = stack
                .or(driver)
                .and_then(|s| s.get("program"))
                .map(str::to_string)
                .unwrap_or_else(|| base(&svc.exec).to_string());
            let mac = |s: &Said| s.get("mac").map(str::to_string);
            let gives = if let Some(st) = stack {
                Gives::Stack(st.get("addr").unwrap_or("?").to_string(), mac(st).unwrap_or_default())
            } else if let Some(d) = driver {
                if d.get("serves") == Some("stack") {
                    // A stack that has not said its address: the kernel has
                    // no interface for the card all the same.
                    Gives::Stack("?".into(), mac(d).unwrap_or_default())
                } else if let Some(m) = mac(d) {
                    let iface = kernel.ifaces.iter().find(|(_, a)| a.eq_ignore_ascii_case(&m)).map(|(n, _)| n.clone());
                    Gives::Iface(iface, m)
                } else if let Some(sectors) = d.get("sectors").and_then(|v| v.parse::<u64>().ok()) {
                    let same: Vec<&String> = kernel
                        .disks
                        .iter()
                        .filter(|(_, v)| v.parse::<u64>().ok() == Some(sectors))
                        .map(|(n, _)| n)
                        .collect();
                    Gives::Disk((same.len() == 1).then(|| same[0].clone()), sectors)
                } else {
                    Gives::Unknown
                }
            } else {
                Gives::Unknown
            };
            let driver_pid = match stack {
                Some(st) => st.get("driver_pid").and_then(|v| v.parse().ok()),
                None => driver.and_then(Said::pid).filter(|p| Some(*p) != svc.pid),
            };
            let driver_restarts = stack
                .and_then(|st| st.get("driver_starts"))
                .and_then(|v| v.parse::<u32>().ok())
                .map_or(0, |n| n.saturating_sub(1));
            Some(Server {
                service: svc.name.clone(),
                state: svc.state.clone(),
                device,
                program,
                pid: svc.pid,
                driver_pid,
                gives,
                init_restarts: svc.starts.saturating_sub(1),
                driver_restarts,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysmon::parse_services;

    const SVCS: &str = "# x\n\
        ntpd\trespawn\trunning\t455\t1\t100\t-\t/usr/local/bin/eclipse-ntpd\t-\n\
        ublk\trespawn\trunning\t231\t3\t100\t-\t/usr/local/bin/eclipse-ublk\tvirtio-blk@1\n\
        ublk-mounts\toneshot\tidle\t-\t0\t-\t-\t/usr/local/bin/eclipse-ublk-mounts\tvirtio-blk@1\n\
        unet\trespawn\trunning\t240\t1\t100\t-\t/usr/local/bin/eclipse-unet\tvirtio-net@0\n";

    fn kernel() -> Kernel {
        Kernel {
            ifaces: vec![("eth0".into(), "52:54:00:12:34:56".into()), ("lo".into(), "00:00:00:00:00:00".into())],
            disks: vec![("vda".into(), "2097152".into()), ("vdb".into(), "131072".into())],
        }
    }

    #[test]
    fn only_services_with_a_device_are_servers_and_init_restarts_are_starts_less_one() {
        let said = vec![
            Said::parse("driver.virtio-blk@1", "pid=231\nprogram=udrv-virtio-blk\nsectors=131072\n"),
            Said::parse("driver.virtio-net@0", "pid=240\nprogram=udrv-virtio-net\nmac=52:54:00:12:34:56\n"),
        ];
        let s = servers(&parse_services(SVCS), &said, &kernel(), &|_| true);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].service, "ublk");
        assert_eq!(s[0].program, "udrv-virtio-blk");
        assert_eq!(s[0].gives, Gives::Disk(Some("vdb".into()), 131072));
        assert_eq!(s[0].init_restarts, 2);
        assert_eq!(s[0].driver_pid, None, "the service's process is the driver");
        assert_eq!(s[1].gives, Gives::Iface(Some("eth0".into()), "52:54:00:12:34:56".into()));
        assert_eq!(s[1].restarts(), 0);
    }

    #[test]
    fn a_stack_names_its_driver_and_counts_the_drivers_it_started() {
        let said = vec![
            Said::parse(
                "stack.virtio-net@0",
                "pid=240\nprogram=eclipse-netstack\nmac=52:54:00:12:34:56\naddr=10.0.9.15/24\ndriver_pid=260\ndriver_starts=4\n",
            ),
            Said::parse("driver.virtio-net@0", "pid=260\nprogram=udrv-virtio-net\nserves=stack\n"),
        ];
        let s = servers(&parse_services(SVCS), &said, &kernel(), &|_| true);
        let net = &s[1];
        assert_eq!(net.program, "eclipse-netstack");
        assert_eq!(net.gives, Gives::Stack("10.0.9.15/24".into(), "52:54:00:12:34:56".into()));
        assert_eq!((net.pid, net.driver_pid), (Some(240), Some(260)));
        assert_eq!(net.driver_restarts, 3);
    }

    #[test]
    fn a_driver_that_serves_a_stack_gives_no_kernel_interface_even_alone() {
        let said = vec![Said::parse("driver.virtio-net@0", "pid=260\nserves=stack\nmac=52:54:00:12:34:56\n")];
        let s = servers(&parse_services(SVCS), &said, &kernel(), &|_| true);
        assert_eq!(s[1].gives, Gives::Stack("?".into(), "52:54:00:12:34:56".into()));
        assert_eq!(s[1].driver_pid, Some(260));
    }

    #[test]
    fn what_a_dead_process_said_is_not_believed() {
        let said = vec![Said::parse("stack.virtio-net@0", "pid=99\nprogram=eclipse-netstack\naddr=10.0.9.15/24\n")];
        let s = servers(&parse_services(SVCS), &said, &kernel(), &|p| p != 99);
        assert_eq!(s[1].program, "eclipse-unet", "the service's exec, then");
        assert_eq!(s[1].gives, Gives::Unknown);
    }

    /// What an x86_64 boot under QEMU with `udriver=virtio-blk@0,virtio-net@0`
    /// wrote after one `kill -9` of both drivers.
    #[test]
    fn the_files_of_a_real_boot() {
        let svcs = "# name\tkind\tstate\tpid\tstarts\tstarted\tlog\texec\tudriver\n\
            ublk\trespawn\trunning\t1029\t2\t1794500433\t/tmp/ublk.log\t/usr/local/bin/eclipse-ublk\tvirtio-blk@0\n\
            ublk-mounts\toneshot\tidle\t-\t0\t-\t-\t/usr/local/bin/eclipse-ublk-mounts\tvirtio-blk@0\n\
            unet\trespawn\trunning\t1038\t2\t1794500433\t/tmp/unet.log\t/usr/local/bin/eclipse-unet\tvirtio-net@0\n";
        let said = vec![
            Said::parse("driver.virtio-blk@0", "pid=1029\nprogram=udrv-virtio-blk\ndevice=virtio-blk@0\nserves=kernel\nsectors=65536\n"),
            Said::parse("driver.virtio-net@0", "pid=1038\nprogram=udrv-virtio-net\ndevice=virtio-net@0\nserves=kernel\nmac=52:54:00:12:34:57\n"),
        ];
        let k = Kernel {
            ifaces: vec![
                ("eth0".into(), "52:54:00:12:34:56".into()),
                ("eth1".into(), "52:54:00:12:34:57".into()),
                ("lo".into(), "00:00:00:00:00:00".into()),
            ],
            disks: vec![("sda".into(), "1323008".into()), ("vda".into(), "65536".into())],
        };
        let s = servers(&parse_services(svcs), &said, &k, &|_| true);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].gives.clone(), s[0].restarts()), (Gives::Disk(Some("vda".into()), 65536), 1));
        assert_eq!((s[1].gives.clone(), s[1].restarts()), (Gives::Iface(Some("eth1".into()), "52:54:00:12:34:57".into()), 1));
        assert_eq!(s[1].program, "udrv-virtio-net");
    }

    #[test]
    fn two_disks_of_the_same_size_name_neither() {
        let mut k = kernel();
        k.disks.push(("vdc".into(), "131072".into()));
        let said = vec![Said::parse("driver.virtio-blk@1", "pid=231\nsectors=131072\n")];
        let s = servers(&parse_services(SVCS), &said, &k, &|_| true);
        assert_eq!(s[0].gives, Gives::Disk(None, 131072));
    }

    #[test]
    fn an_init_without_the_device_column_has_no_servers() {
        let old = "ublk\trespawn\trunning\t231\t3\t100\t-\t/usr/local/bin/eclipse-ublk\n";
        assert!(servers(&parse_services(old), &[], &kernel(), &|_| true).is_empty());
    }
}
