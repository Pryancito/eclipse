use crate::scheme::{Scheme, Stat};
use crate::scheme::error::*;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use spin::Mutex;

pub struct SysScheme;

impl SysScheme {
    pub fn new() -> Self {
        Self
    }
}

impl Scheme for SysScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Minimal virtual /sys hierarchy for udev/wlroots
        // /sys/dev/block/
        // /sys/dev/char/
        // /sys/class/drm/card0/dev -> 226:0
        // /sys/class/graphics/fb0/dev -> 29:0
        
        let normalized = path.trim_start_matches('/');
        
        match normalized {
            "" | "dev" | "dev/block" | "dev/char" | "class" | "class/drm" | "class/drm/card0" | "class/graphics" | "class/graphics/fb0" => {
                // Directory
                Ok(0x1000 | (hash_path(normalized) as usize & 0xFFF))
            }
            "class/drm/card0/dev" | "class/drm/card0/uevent" | "class/graphics/fb0/dev" | "class/graphics/fb0/uevent" => {
                // File
                Ok(0x2000 | (hash_path(normalized) as usize & 0xFFF))
            }
            "dev/char/226:0" | "dev/char/29:0" => {
                // Symlink — use resource type 0x3000
                Ok(0x3000 | (hash_path(normalized) as usize & 0xFFF))
            }
            _ => Err(ENOENT),
        }
    }

    fn read(&self, id: usize, buf: &mut [u8], _offset: u64) -> Result<usize, usize> {
        // Identify the resource by its fake ID
        let res_type = id >> 12;
        
        if res_type == 1 { // Directory
             // Determine which directory
             let content = match id & 0xFFF {
                 h if h == hash_path("") as usize & 0xFFF => "dev\nclass\n",
                 h if h == hash_path("dev") as usize & 0xFFF => "block\nchar\n",
                 h if h == hash_path("dev/block") as usize & 0xFFF => "",
                 h if h == hash_path("dev/char") as usize & 0xFFF => "226:0\n29:0\n",
                 h if h == hash_path("class") as usize & 0xFFF => "drm\ngraphics\n",
                 h if h == hash_path("class/drm") as usize & 0xFFF => "card0\n",
                 h if h == hash_path("class/drm/card0") as usize & 0xFFF => "dev\nuevent\n",
                 h if h == hash_path("class/graphics") as usize & 0xFFF => "fb0\n",
                 h if h == hash_path("class/graphics/fb0") as usize & 0xFFF => "dev\nuevent\n",
                 _ => return Ok(0),
             };
             
             let bytes = content.as_bytes();
             let len = core::cmp::min(buf.len(), bytes.len());
             buf[..len].copy_from_slice(&bytes[..len]);
             return Ok(len);
        }
        
        // For files ("dev" and "uevent" entries):
        let content = if id == 0x2000 | (hash_path("class/drm/card0/dev") as usize & 0xFFF) {
            "226:0\n"
        } else if id == 0x2000 | (hash_path("class/drm/card0/uevent") as usize & 0xFFF) {
            "MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\nDEVTYPE=drm_minor\n"
        } else if id == 0x2000 | (hash_path("class/graphics/fb0/dev") as usize & 0xFFF) {
            "29:0\n"
        } else if id == 0x2000 | (hash_path("class/graphics/fb0/uevent") as usize & 0xFFF) {
            "MAJOR=29\nMINOR=0\nDEVNAME=fb0\n"
        } else if id == 0x3000 | (hash_path("dev/char/226:0") as usize & 0xFFF) {
            "../../class/drm/card0"
        } else if id == 0x3000 | (hash_path("dev/char/29:0") as usize & 0xFFF) {
            "../../class/graphics/fb0"
        } else {
            return Err(EBADF);
        };
        
        let bytes = content.as_bytes();
        let len = core::cmp::min(buf.len(), bytes.len());
        buf[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
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
            1 => 0o40755,   // directory
            3 => 0o120644,  // symlink (S_IFLNK)
            _ => 0o100444,  // regular file
        };
        stat.size = if res_type == 1 { 0 } else { 6 };
        Ok(0)
    }

    fn getdents(&self, id: usize) -> Result<Vec<String>, usize> {
        let res_type = id >> 12;
        if res_type != 1 { return Err(ENOTDIR); }
        
        let h = id & 0xFFF;
        let mut list = Vec::new();
        
        if h == hash_path("") as usize & 0xFFF {
            list.push(String::from("dev"));
            list.push(String::from("class"));
        } else if h == hash_path("dev") as usize & 0xFFF {
            list.push(String::from("block"));
            list.push(String::from("char"));
        } else if h == hash_path("dev/char") as usize & 0xFFF {
            list.push(String::from("226:0"));
            list.push(String::from("29:0"));
        } else if h == hash_path("class") as usize & 0xFFF {
            list.push(String::from("drm"));
            list.push(String::from("graphics"));
        } else if h == hash_path("class/drm") as usize & 0xFFF {
            list.push(String::from("card0"));
        } else if h == hash_path("class/drm/card0") as usize & 0xFFF {
            list.push(String::from("dev"));
            list.push(String::from("uevent"));
        } else if h == hash_path("class/graphics") as usize & 0xFFF {
            list.push(String::from("fb0"));
        } else if h == hash_path("class/graphics/fb0") as usize & 0xFFF {
            list.push(String::from("dev"));
            list.push(String::from("uevent"));
        }
        
        Ok(list)
    }
}

fn hash_path(path: &str) -> u32 {
    let mut h = 0u32;
    for b in path.as_bytes() {
        h = h.wrapping_mul(31).wrapping_add(*b as u32);
    }
    h
}
