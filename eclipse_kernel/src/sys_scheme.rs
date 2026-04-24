use crate::scheme::{Scheme, Stat};
use crate::scheme::error::*;
use alloc::string::String;
use alloc::vec::Vec;

pub struct SysScheme;

impl SysScheme {
    pub fn new() -> Self {
        Self
    }
}

/// Jerarquía virtual mínima de `/sys` (udev, libdrm, libinput quirks).
/// Los IDs usan índices explícitos (no hash mod 12 bits) para evitar colisiones
/// entre p. ej. `dev/char/226:0/device` y `firmware/devicetree/base`.
const TYPE_DIR: usize = 0x1000;
const TYPE_FILE: usize = 0x2000;

#[derive(Clone, Copy)]
enum SysKind {
    Dir(u16),
    File(u16),
}

fn classify(path: &str) -> Option<SysKind> {
    let normalized = path.trim_start_matches('/');
    if let Some(i) = dir_index(normalized) {
        return Some(SysKind::Dir(i));
    }
    if let Some(i) = file_index(normalized) {
        return Some(SysKind::File(i));
    }
    None
}

fn dir_index(path: &str) -> Option<u16> {
    Some(match path {
        "" => 0,
        "dev" => 1,
        "dev/block" => 2,
        "dev/char" => 3,
        "class" => 4,
        "class/drm" => 5,
        "class/drm/card0" => 6,
        "class/drm/renderD128" => 27,
        "class/graphics" => 7,
        "class/graphics/fb0" => 8,
        "bus" => 30,
        "bus/pci" => 31,
        "dev/char/226:0" => 9,
        "dev/char/226:0/device" => 10,
        "dev/char/226:0/device/drm" => 11,
        "dev/char/226:0/device/drm/card0" => 35,
        "dev/char/226:0/device/drm/renderD128" => 36,
        "dev/char/226:128" => 15,
        "dev/char/226:128/device" => 16,
        "dev/char/226:128/device/drm" => 17,
        "dev/char/226:128/device/drm/card0" => 37,
        "dev/char/226:128/device/drm/renderD128" => 38,
        "dev/char/29:0" => 12,
        "dev/char/29:0/device" => 13,
        "dev/char/29:0/device/graphics" => 14,
        "devices" => 20,
        "devices/pci0000:00" => 32,
        "devices/pci0000:00/0000:00:02.0" => 33,
        "devices/pci0000:00/0000:00:02.0/drm" => 34,
        "devices/pci0000:00/0000:00:02.0/drm/card0" => 39,
        "devices/pci0000:00/0000:00:02.0/drm/renderD128" => 40,
        "devices/virtual" => 21,
        "devices/virtual/dmi" => 22,
        "devices/virtual/dmi/id" => 23,
        "firmware" => 24,
        "firmware/devicetree" => 25,
        "firmware/devicetree/base" => 26,
        "class/input" => 50,
        "class/input/event0" => 51,
        "class/input/event1" => 52,
        "dev/char/13:64" => 53,
        "dev/char/13:65" => 54,
        _ => return None,
    })
}

fn file_index(path: &str) -> Option<u16> {
    Some(match path {
        "class/drm/card0/dev" => 0,
        "class/drm/card0/uevent" => 1,
        "class/drm/card0/device" => 15,
        "class/drm/renderD128/dev" => 8,
        "class/drm/renderD128/uevent" => 9,
        "class/drm/renderD128/device" => 16,
        "dev/char/226:0/uevent" => 2,
        "dev/char/226:0/device/subsystem" => 17,
        "dev/char/226:128/uevent" => 10,
        "dev/char/226:128/device/subsystem" => 18,
        "class/graphics/fb0/dev" => 3,
        "class/graphics/fb0/uevent" => 4,
        "class/graphics/fb0/device" => 19,
        "dev/char/29:0/uevent" => 5,
        "devices/virtual/dmi/id/uevent" => 6,
        "firmware/devicetree/base/compatible" => 7,
        "class/input/event0/dev" => 27,
        "class/input/event0/uevent" => 28,
        "class/input/event1/dev" => 29,
        "class/input/event1/uevent" => 30,
        "dev/char/13:64/uevent" => 31,
        "dev/char/13:65/uevent" => 32,
        // PCI device uevent (shared between card0/renderD128/pci device paths)
        "dev/char/226:0/device/uevent"
        | "dev/char/226:128/device/uevent"
        | "devices/pci0000:00/0000:00:02.0/uevent" => 20,
        // PCI vendor ID
        "dev/char/226:0/device/vendor"
        | "dev/char/226:128/device/vendor"
        | "devices/pci0000:00/0000:00:02.0/vendor" => 21,
        // PCI device ID
        "dev/char/226:0/device/device"
        | "dev/char/226:128/device/device"
        | "devices/pci0000:00/0000:00:02.0/device" => 22,
        // PCI class code
        "dev/char/226:0/device/class"
        | "dev/char/226:128/device/class"
        | "devices/pci0000:00/0000:00:02.0/class" => 23,
        // PCI revision
        "dev/char/226:0/device/revision"
        | "dev/char/226:128/device/revision"
        | "devices/pci0000:00/0000:00:02.0/revision" => 24,
        // PCI subsystem vendor
        "dev/char/226:0/device/subsystem_vendor"
        | "dev/char/226:128/device/subsystem_vendor"
        | "devices/pci0000:00/0000:00:02.0/subsystem_vendor" => 25,
        // PCI subsystem device
        "dev/char/226:0/device/subsystem_device"
        | "dev/char/226:128/device/subsystem_device"
        | "devices/pci0000:00/0000:00:02.0/subsystem_device" => 26,
        // devices/pci0000:00/0000:00:02.0/subsystem is a symlink (content via readlink)
        "devices/pci0000:00/0000:00:02.0/subsystem" => 27,
        _ => return None,
    })
}

fn dir_listing(idx: u16) -> &'static str {
    match idx {
        0 => "dev\nclass\ndevices\nfirmware\nbus\n",
        1 => "block\nchar\n",
        2 => "",
        3 => "226:0\n226:128\n29:0\n",
        4 => "drm\ngraphics\n",
        5 => "card0\nrenderD128\n",
        6 => "dev\nuevent\ndevice\n",
        7 => "fb0\n",
        8 => "dev\nuevent\ndevice\n",
        9 => "device\nuevent\n",
        // dev/char/226:128 — char device directory
        15 => "device\nuevent\n",
        // dev/char/226:0/device — includes PCI attribute files
        10 => "drm\nsubsystem\nuevent\nvendor\ndevice\nclass\nrevision\nsubsystem_vendor\nsubsystem_device\n",
        // dev/char/226:0/device/drm — list DRM nodes for this device
        11 => "card0\nrenderD128\n",
        12 => "device\nuevent\n",
        13 => "graphics\n",
        14 => "",
        // dev/char/226:128/device — includes PCI attribute files
        16 => "drm\nsubsystem\nuevent\nvendor\ndevice\nclass\nrevision\nsubsystem_vendor\nsubsystem_device\n",
        // dev/char/226:128/device/drm — list DRM nodes for this device
        17 => "card0\nrenderD128\n",
        20 => "pci0000:00\nvirtual\n",
        21 => "dmi\n",
        22 => "id\n",
        23 => "uevent\n",
        24 => "devicetree\n",
        25 => "base\n",
        26 => "compatible\n",
        27 => "dev\nuevent\ndevice\n",
        30 => "pci\n",
        31 => "",
        32 => "0000:00:02.0\n",
        // devices/pci0000:00/0000:00:02.0 — includes PCI attribute files and drm subdir
        33 => "drm\nsubsystem\nuevent\nvendor\ndevice\nclass\nrevision\nsubsystem_vendor\nsubsystem_device\n",
        // devices/pci0000:00/0000:00:02.0/drm — list DRM nodes for this device
        34 => "card0\nrenderD128\n",
        // dev/char/226:0/device/drm/card0, renderD128 — leaf dirs
        35 | 36 | 37 | 38 | 39 | 40 | 51 | 52 | 53 | 54 => "dev\nuevent\n",
        50 => "event0\nevent1\n",
        _ => "",
    }
}

fn file_content(idx: u16) -> &'static str {
    match idx {
        0 => "226:0\n",
        1 => "MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\nDEVTYPE=drm_minor\n",
        8 => "226:128\n",
        9 => "MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\nDEVTYPE=drm_minor\n",
        2 => "MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\nDEVTYPE=drm_minor\n",
        10 => "MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\nDEVTYPE=drm_minor\n",
        3 => "29:0\n",
        4 => "MAJOR=29\nMINOR=0\nDEVNAME=fb0\n",
        5 => "MAJOR=29\nMINOR=0\nDEVNAME=fb0\n",
        // libinput → udev_device_new_from_syspath + set_properties_from_uevent lee MODALIAS
        6 => "MODALIAS=dmi:bvnEclipse:bvr1.0:bd01010101:svnEclipse:pnGeneric:pvr1:rvnEclipse:rnVirtual:rvr1:cvnEclipse:ct1:cvr1:\n",
        // init_dt (libinput): primera cadena compatible; sin DT real en Eclipse
        7 => "eclipse,generic\n",
        // PCI device uevent — libdrm drmParsePciBusInfo reads DRIVER, PCI_ID, PCI_SLOT_NAME
        20 => "DRIVER=eclipse\nPCI_ID=1af4:1050\nPCI_SLOT_NAME=0000:00:02.0\n",
        // PCI vendor: VirtIO/Red Hat (0x1af4)
        21 => "0x1af4\n",
        // PCI device: VirtIO GPU (0x1050)
        22 => "0x1050\n",
        // PCI class: Display controller / VGA compatible (0x030000)
        23 => "0x030000\n",
        // PCI revision
        24 => "0x00\n",
        // PCI subsystem vendor
        25 => "0x1af4\n",
        // PCI subsystem device
        26 => "0x1050\n",
        // devices/pci0000:00/0000:00:02.0/subsystem — symlink, content via readlink
        // devices/pci0000:00/0000:00:02.0/subsystem — symlink, content via readlink
        27 => "13:64\n",
        28 => "MAJOR=13\nMINOR=64\nDEVNAME=input/event0\n",
        29 => "13:65\n",
        30 => "MAJOR=13\nMINOR=65\nDEVNAME=input/event1\n",
        31 => "MAJOR=13\nMINOR=64\nDEVNAME=input/event0\n",
        32 => "MAJOR=13\nMINOR=65\nDEVNAME=input/event1\n",
        _ => "",
    }
}

fn file_size(idx: u16) -> u64 {
    file_content(idx).len() as u64
}

impl Scheme for SysScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        match classify(path).ok_or(ENOENT)? {
            SysKind::Dir(i) => Ok(TYPE_DIR | i as usize),
            SysKind::File(i) => Ok(TYPE_FILE | i as usize),
        }
    }

    fn read(&self, id: usize, buf: &mut [u8], _offset: u64) -> Result<usize, usize> {
        let res_type = id >> 12;
        if res_type == 1 {
            let idx = (id & 0xFFF) as u16;
            let content = dir_listing(idx);
            let bytes = content.as_bytes();
            let len = core::cmp::min(buf.len(), bytes.len());
            buf[..len].copy_from_slice(&bytes[..len]);
            return Ok(len);
        }
        if res_type == 2 {
            let idx = (id & 0xFFF) as u16;
            let content = file_content(idx);
            let bytes = content.as_bytes();
            let len = core::cmp::min(buf.len(), bytes.len());
            buf[..len].copy_from_slice(&bytes[..len]);
            return Ok(len);
        }
        Err(EBADF)
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(ENOSYS)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let res_type = id >> 12;
        stat.mode = match res_type {
            1 => 0o40755,
            2 => 0o100444,
            _ => 0o100444,
        };
        stat.size = if res_type == 1 {
            0
        } else {
            file_size((id & 0xFFF) as u16)
        };
        Ok(0)
    }

    fn getdents(&self, id: usize) -> Result<Vec<String>, usize> {
        let res_type = id >> 12;
        if res_type != 1 {
            return Err(ENOTDIR);
        }
        let idx = (id & 0xFFF) as u16;
        let content = dir_listing(idx);
        let mut list = Vec::new();
        for line in content.split('\n') {
            if line.is_empty() {
                continue;
            }
            list.push(String::from(line));
        }
        Ok(list)
    }
}
