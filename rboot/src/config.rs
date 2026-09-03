use core::str::FromStr;
use log::warn;

/// Graphic-mode selection policy (the `resolution=` config key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Key absent: keep whatever mode the firmware already set.
    Keep,
    /// `resolution=auto`: pick the GOP mode matching the display's
    /// EDID-preferred timing; fall back to the largest offered mode whose
    /// area is at most 4K (`3840×2160`). Uncapped "largest mode" is unsafe:
    /// VirtualBox EFI GOP advertises VRAM-filling 8K modes that are not a
    /// real panel. This is what a fixed value can't do portably — e.g. a
    /// 1366x768 TV stretched an exact `1024x768` (4:3 on 16:9) while its
    /// firmware offered better modes.
    Auto,
    /// `resolution=WxH`: request that exact mode (kept if unavailable).
    Exact(usize, usize),
}

/// Config for the bootloader
#[derive(Debug)]
pub struct Config<'a> {
    /// The address at which the kernel stack is placed
    pub kernel_stack_address: u64,
    /// The size of the kernel stack, given in number of 4KiB pages
    pub kernel_stack_size: u64,
    /// The virtual address offset from which physical memory is mapped
    pub physical_memory_offset: u64,
    /// The path of kernel ELF
    pub kernel_path: &'a str,
    /// The resolution of graphic output
    pub resolution: Resolution,
    /// The path of initramfs
    pub initramfs: Option<&'a str>,
    /// Kernel command line
    pub cmdline: &'a str,
    /// UART base physical address (aarch64)
    pub uart_base: usize,
    /// GIC base physical address (aarch64)
    pub gic_base: usize,
    /// Firmware type (aarch64)
    pub firmware_type: &'a str,
}

#[cfg(target_arch = "aarch64")]
pub const DEFAULT_CONFIG: Config = Config {
    kernel_stack_address: 0xFFFF_0000_8000_0000,
    kernel_stack_size: 512,
    physical_memory_offset: 0xFFFF_0000_0000_0000,
    kernel_path: "\\os",
    resolution: Resolution::Keep,
    initramfs: None,
    cmdline: "",
    uart_base: 0x0900_0000,
    gic_base: 0x0800_0000,
    firmware_type: "QEMU",
};

#[cfg(not(target_arch = "aarch64"))]
pub const DEFAULT_CONFIG: Config = Config {
    kernel_stack_address: 0xFFFF_FF01_0000_0000,
    kernel_stack_size: 512,
    physical_memory_offset: 0xFFFF_8000_0000_0000,
    kernel_path: "\\EFI\\rCore\\kernel.elf",
    resolution: Resolution::Keep,
    initramfs: None,
    cmdline: "",
    uart_base: 0,
    gic_base: 0,
    firmware_type: "PC",
};

/// Tolerant numeric parsing: `0x`/`0X` prefixed hex or plain decimal. `None`
/// (keep the default and warn) instead of panicking on a malformed value —
/// the same never-brick-boot rule as `resolution` below; the old
/// `&value[2..]` also sliced out of bounds on short values.
fn parse_num(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        u64::from_str(value).ok()
    }
}

impl<'a> Config<'a> {
    pub fn parse(content: &'a [u8]) -> Self {
        let content = core::str::from_utf8(content).expect("failed to parse config as utf8");
        let mut config = DEFAULT_CONFIG;
        for line in content.lines() {
            // skip empty and comment lines
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // parse 'key=value'; lines without '=' are ignored instead of
            // aborting the boot.
            let mut iter = line.splitn(2, '=');
            let key = match iter.next() {
                Some(k) => k.trim(),
                None => continue,
            };
            let value = match iter.next() {
                Some(v) => v.trim(),
                None => continue,
            };
            config.process(key, value);
        }
        config
    }

    fn process(&mut self, key: &str, value: &'a str) {
        let num = || {
            let v = parse_num(value);
            if v.is_none() {
                warn!("invalid number for {}: {:?}; keeping default", key, value);
            }
            v
        };
        match key {
            "kernel_stack_address" => {
                if let Some(v) = num() {
                    self.kernel_stack_address = v;
                }
            }
            "kernel_stack_size" => {
                if let Some(v) = num() {
                    self.kernel_stack_size = v;
                }
            }
            "physical_memory_offset" => {
                if let Some(v) = num() {
                    self.physical_memory_offset = v;
                }
            }
            "kernel_path" => self.kernel_path = value,
            "resolution" => {
                // NEVER panic on a config value: an unparsable resolution once
                // bricked boot outright (an old bootloader binary reading a
                // newer conf's `auto` died with ParseIntError before drawing
                // anything). Unknown/malformed values degrade to Auto with a
                // warning — the machine always boots.
                if value.eq_ignore_ascii_case("auto") {
                    self.resolution = Resolution::Auto;
                } else {
                    let mut iter = value.split('x');
                    let x = iter.next().and_then(|v| usize::from_str(v).ok());
                    let y = iter.next().and_then(|v| usize::from_str(v).ok());
                    match (x, y) {
                        (Some(x), Some(y)) if x > 0 && y > 0 => {
                            self.resolution = Resolution::Exact(x, y);
                        }
                        _ => {
                            warn!("invalid resolution {:?}; using auto", value);
                            self.resolution = Resolution::Auto;
                        }
                    }
                }
            }
            "initramfs" => self.initramfs = Some(value),
            "cmdline" => self.cmdline = value,
            "uart_base" => {
                if let Some(v) = num() {
                    self.uart_base = v as usize;
                }
            }
            "gic_base" => {
                if let Some(v) = num() {
                    self.gic_base = v as usize;
                }
            }
            "firmware_type" => self.firmware_type = value,
            _ => warn!("undefined config key: {}", key),
        }
    }
}
