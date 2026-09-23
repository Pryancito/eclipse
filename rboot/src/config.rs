//! `\EFI\Boot\rboot.conf`: the bootloader's only input besides the kernel.
//!
//! The one rule of this file: **nothing here may panic**. rboot is the first
//! thing that runs; a panic is a machine that does not boot and shows no
//! reason why. Every malformed key, value, line or byte keeps the default,
//! warns, and carries on. (An unparsable `resolution` once bricked boot
//! outright: an old bootloader binary reading a newer conf's `auto` died with
//! a `ParseIntError` before drawing anything.)

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
    /// The offset into the virtual address space where the physical memory is mapped
    pub physical_memory_offset: u64,
    /// The path of kernel ELF
    pub kernel_path: &'a str,
    /// The resolution of graphic output
    pub resolution: Resolution,
    /// The path of initramfs
    pub initramfs: Option<&'a str>,
    /// Kernel command line
    pub cmdline: &'a str,
}

pub const PAGE_SIZE: u64 = 0x1000;

/// Largest `kernel_stack_size` we accept, in 4 KiB pages (1 GiB).
///
/// Anything above this is a typo, and the loader would sit in
/// `map_stack` asking the firmware for frames until it runs out.
pub const MAX_STACK_PAGES: u64 = 256 * 1024;

const DEFAULT_CONFIG: Config = Config {
    kernel_stack_address: 0xFFFF_FF01_0000_0000,
    kernel_stack_size: 512,
    physical_memory_offset: 0xFFFF_8000_0000_0000,
    kernel_path: "\\EFI\\rCore\\kernel.elf",
    resolution: Resolution::Keep,
    initramfs: None,
    cmdline: "",
};

impl<'a> Config<'a> {
    pub fn parse(content: &'a [u8]) -> Self {
        // A config file is written by a human with an editor, so it can be
        // anything at all -- including UTF-16 (Notepad's "Unicode"), which is
        // not UTF-8 from byte 0. Parse the valid prefix and keep the defaults
        // for the rest rather than dying here.
        let content = match core::str::from_utf8(content) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "config is not valid utf-8 at byte {}; ignoring the rest",
                    e.valid_up_to()
                );
                // SAFETY-free: `valid_up_to()` is by definition a valid
                // boundary, so this second call cannot fail.
                core::str::from_utf8(&content[..e.valid_up_to()]).unwrap_or("")
            }
        };
        // A UTF-8 BOM *is* valid UTF-8, so it survives the decode and glues
        // itself to the first key, silently dropping the first setting.
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let mut config = DEFAULT_CONFIG;
        for line in content.split('\n') {
            let line = line.trim();
            // skip empty and comment
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // parse 'key=value'
            let mut iter = line.splitn(2, '=');
            let key = iter.next().unwrap_or("");
            let Some(value) = iter.next() else {
                // A line with no '=' used to panic here, which means one
                // stray word in rboot.conf was an unbootable machine.
                warn!("config line without '=': {:?}; ignored", line);
                continue;
            };
            config.process(key.trim(), value.trim());
        }
        config.validate();
        config
    }

    /// Reject values that are individually parsable but together describe a
    /// stack the loader cannot map, restoring the default instead.
    fn validate(&mut self) {
        // `Page::containing_address` rounds the base down, so an unaligned
        // base maps [align_down(addr), align_down(addr) + size) while
        // `stack_top()` points `addr % 4096` bytes past the end of it -- and
        // the pre-ExitBootServices probe writes exactly there.
        let aligned = self.kernel_stack_address & !(PAGE_SIZE - 1);
        if aligned != self.kernel_stack_address {
            warn!(
                "kernel_stack_address {:#x} is not page aligned; using {:#x}",
                self.kernel_stack_address, aligned
            );
            self.kernel_stack_address = aligned;
        }
        if self.kernel_stack_size == 0 || self.kernel_stack_size > MAX_STACK_PAGES {
            warn!(
                "kernel_stack_size {} out of range (1..={}); using {}",
                self.kernel_stack_size, MAX_STACK_PAGES, DEFAULT_CONFIG.kernel_stack_size
            );
            self.kernel_stack_size = DEFAULT_CONFIG.kernel_stack_size;
        }
        // In release an overflow here wraps silently and the kernel is handed
        // a stack pointer that is nowhere near its stack.
        if self
            .kernel_stack_size
            .checked_mul(PAGE_SIZE)
            .and_then(|bytes| self.kernel_stack_address.checked_add(bytes))
            .is_none()
        {
            warn!(
                "kernel_stack_address {:#x} + {} pages overflows; using defaults",
                self.kernel_stack_address, self.kernel_stack_size
            );
            self.kernel_stack_address = DEFAULT_CONFIG.kernel_stack_address;
            self.kernel_stack_size = DEFAULT_CONFIG.kernel_stack_size;
        }
    }

    /// First address past the kernel stack, i.e. the initial `rsp`.
    ///
    /// [`Config::parse`] has already refused an address/size pair that would
    /// overflow or leave this past the mapped range, so this is always the
    /// end of what `map_stack` mapped.
    pub fn stack_top(&self) -> u64 {
        self.kernel_stack_address + self.kernel_stack_size * PAGE_SIZE
    }

    fn process(&mut self, key: &str, value: &'a str) {
        // Tolerant numeric parsing: a malformed value keeps the default and
        // warns instead of panicking (same never-brick-boot rule as
        // `resolution` below; the old `&value[2..]` also sliced out of bounds
        // on short values).
        let r10 = || match u64::from_str(value) {
            Ok(v) => Some(v),
            Err(_) => {
                warn!("invalid number for {}: {:?}; keeping default", key, value);
                None
            }
        };
        let r16 = || {
            let digits = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .unwrap_or(value);
            match u64::from_str_radix(digits, 16) {
                Ok(v) => Some(v),
                Err(_) => {
                    warn!("invalid hex for {}: {:?}; keeping default", key, value);
                    None
                }
            }
        };
        match key {
            "kernel_stack_address" => {
                if let Some(v) = r16() {
                    self.kernel_stack_address = v;
                }
            }
            "kernel_stack_size" => {
                if let Some(v) = r10() {
                    self.kernel_stack_size = v;
                }
            }
            "physical_memory_offset" => {
                if let Some(v) = r16() {
                    self.physical_memory_offset = v;
                }
            }
            "kernel_path" => self.kernel_path = value,
            "resolution" => {
                // NEVER panic on a config value: unknown/malformed values
                // degrade to Auto with a warning -- the machine always boots.
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
            _ => warn!("undefined config key: {}", key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `rboot.conf`, verbatim.
    const SHIPPED: &str = include_str!("../rboot.conf");

    #[test]
    fn the_shipped_config_parses_to_what_it_documents() {
        let c = Config::parse(SHIPPED.as_bytes());
        assert_eq!(c.kernel_stack_address, 0xFFFF_FF01_0000_0000);
        assert_eq!(c.kernel_stack_size, 512);
        assert_eq!(c.physical_memory_offset, 0xFFFF_8000_0000_0000);
        assert_eq!(c.kernel_path, "\\EFI\\rCore\\kernel.elf");
        assert_eq!(c.resolution, Resolution::Auto);
        assert_eq!(c.cmdline, "");
        assert_eq!(c.initramfs, None); // the line is commented out
    }

    #[test]
    fn an_empty_config_is_all_defaults() {
        let c = Config::parse(b"");
        assert_eq!(c.kernel_stack_address, DEFAULT_CONFIG.kernel_stack_address);
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
        assert_eq!(c.resolution, Resolution::Keep);
        assert_eq!(c.kernel_path, DEFAULT_CONFIG.kernel_path);
    }

    #[test]
    fn a_line_without_an_equals_sign_does_not_brick_boot() {
        // This used to be `iter.next().expect("failed to parse value")`.
        let c = Config::parse(b"resolution auto\nkernel_stack_size=64\ngarbage\n");
        assert_eq!(c.kernel_stack_size, 64);
        assert_eq!(c.resolution, Resolution::Keep);
    }

    #[test]
    fn a_config_that_is_not_utf8_does_not_brick_boot() {
        // UTF-16LE with BOM, what Notepad writes as "Unicode".
        let mut utf16 = alloc::vec![0xFFu8, 0xFE];
        for b in "kernel_stack_size=64\n".bytes() {
            utf16.push(b);
            utf16.push(0);
        }
        let c = Config::parse(&utf16);
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);

        // A stray non-UTF-8 byte keeps everything before it.
        let mut mixed = alloc::vec![];
        mixed.extend_from_slice(b"kernel_stack_size=64\n");
        mixed.push(0xFF);
        mixed.extend_from_slice(b"\nresolution=auto\n");
        assert_eq!(Config::parse(&mixed).kernel_stack_size, 64);
    }

    #[test]
    fn a_utf8_bom_does_not_eat_the_first_key() {
        let mut bom = alloc::vec![0xEFu8, 0xBB, 0xBF];
        bom.extend_from_slice(b"kernel_stack_size=64\nresolution=auto\n");
        let c = Config::parse(&bom);
        assert_eq!(c.kernel_stack_size, 64);
        assert_eq!(c.resolution, Resolution::Auto);
    }

    #[test]
    fn spaces_around_the_equals_sign_are_not_part_of_the_value() {
        // Silently keeping the default because someone wrote `key = value`
        // is worse than a warning: the machine boots with a stack it did not
        // ask for and nothing says so.
        let c = Config::parse(b"kernel_stack_size = 64\nresolution = 1024x768\n");
        assert_eq!(c.kernel_stack_size, 64);
        assert_eq!(c.resolution, Resolution::Exact(1024, 768));
    }

    #[test]
    fn crlf_line_endings_parse() {
        let c = Config::parse(b"# comment\r\nkernel_stack_size=64\r\nresolution=auto\r\n");
        assert_eq!(c.kernel_stack_size, 64);
        assert_eq!(c.resolution, Resolution::Auto);
    }

    #[test]
    fn hex_keys_take_0x_or_bare_digits_and_either_case() {
        for text in [
            "kernel_stack_address=0xFFFFFF0100000000",
            "kernel_stack_address=0XFFFFFF0100000000",
            "kernel_stack_address=FFFFFF0100000000",
            "kernel_stack_address=ffffff0100000000",
        ] {
            assert_eq!(
                Config::parse(text.as_bytes()).kernel_stack_address,
                0xFFFF_FF01_0000_0000,
                "{text}"
            );
        }
    }

    #[test]
    fn an_unparsable_number_keeps_the_default() {
        let c = Config::parse(b"kernel_stack_size=lots\nphysical_memory_offset=0xZZ\n");
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
        assert_eq!(
            c.physical_memory_offset,
            DEFAULT_CONFIG.physical_memory_offset
        );
    }

    #[test]
    fn a_number_that_does_not_fit_in_u64_keeps_the_default() {
        let c = Config::parse(b"kernel_stack_size=99999999999999999999999\n");
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
    }

    #[test]
    fn resolution_accepts_auto_in_any_case_and_exact_pairs() {
        assert_eq!(
            Config::parse(b"resolution=auto").resolution,
            Resolution::Auto
        );
        assert_eq!(
            Config::parse(b"resolution=AUTO").resolution,
            Resolution::Auto
        );
        assert_eq!(
            Config::parse(b"resolution=1920x1080").resolution,
            Resolution::Exact(1920, 1080)
        );
    }

    #[test]
    fn a_malformed_resolution_degrades_to_auto() {
        for text in [
            "resolution=",
            "resolution=x",
            "resolution=0x0",
            "resolution=1920",
            "resolution=1920x",
            "resolution=1920xNaN",
            "resolution=-1x-1",
        ] {
            assert_eq!(
                Config::parse(text.as_bytes()).resolution,
                Resolution::Auto,
                "{text}"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_a_warning_not_a_failure() {
        let c = Config::parse(b"nonsense=1\nkernel_stack_size=64\n");
        assert_eq!(c.kernel_stack_size, 64);
    }

    #[test]
    fn paths_and_cmdline_are_taken_verbatim() {
        let c = Config::parse(
            b"kernel_path=\\EFI\\eclipse\\kernel.elf\n\
              initramfs=\\EFI\\eclipse\\initramfs.img\n\
              cmdline=LOG=debug:ROOTPROC=/bin/busybox?sh\n",
        );
        assert_eq!(c.kernel_path, "\\EFI\\eclipse\\kernel.elf");
        assert_eq!(c.initramfs, Some("\\EFI\\eclipse\\initramfs.img"));
        // The value keeps its own '=' signs: only the first one is the split.
        assert_eq!(c.cmdline, "LOG=debug:ROOTPROC=/bin/busybox?sh");
    }

    #[test]
    fn stack_top_is_the_end_of_what_map_stack_maps() {
        let c = Config::parse(b"kernel_stack_address=0x1000000\nkernel_stack_size=2\n");
        assert_eq!(c.stack_top(), 0x1000000 + 2 * PAGE_SIZE);
    }

    #[test]
    fn an_unaligned_stack_address_is_rounded_down_to_the_mapped_page() {
        // `map_stack` maps `Page::containing_address(addr) .. + size`, so an
        // unaligned base leaves `stack_top() - 8` -- the address the
        // pre-ExitBootServices probe writes to -- past the last mapped page.
        let c = Config::parse(b"kernel_stack_address=0x1000800\nkernel_stack_size=2\n");
        assert_eq!(c.kernel_stack_address, 0x1000000);
        assert_eq!(c.stack_top(), 0x1000000 + 2 * PAGE_SIZE);
    }

    #[test]
    fn a_zero_page_stack_is_refused() {
        // `stack_top() == kernel_stack_address` with nothing mapped there.
        let c = Config::parse(b"kernel_stack_size=0\n");
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
    }

    #[test]
    fn an_absurd_stack_size_is_refused_instead_of_exhausting_the_firmware() {
        let c = Config::parse(b"kernel_stack_size=1000000000\n");
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
        assert!(c.kernel_stack_size <= MAX_STACK_PAGES);
    }

    #[test]
    fn a_stack_that_would_wrap_the_address_space_is_refused() {
        // In release `address + size * 4096` wraps silently, and the kernel
        // starts with an `rsp` that is nowhere near its stack.
        let c =
            Config::parse(b"kernel_stack_address=0xFFFFFFFFFFFFF000\nkernel_stack_size=262144\n");
        assert_eq!(c.kernel_stack_address, DEFAULT_CONFIG.kernel_stack_address);
        assert_eq!(c.kernel_stack_size, DEFAULT_CONFIG.kernel_stack_size);
        assert!(c
            .kernel_stack_address
            .checked_add(c.kernel_stack_size * PAGE_SIZE)
            .is_some());
    }

    #[test]
    fn every_stack_config_that_parses_yields_a_usable_stack_top() {
        for addr in [
            "0",
            "0x1000",
            "0x800",
            "0xFFFFFFFFFFFFFFFF",
            "0xFFFFFF0100000000",
        ] {
            for size in ["1", "512", "262144", "262145", "18446744073709551615", "0"] {
                let text =
                    alloc::format!("kernel_stack_address={addr}\nkernel_stack_size={size}\n");
                let c = Config::parse(text.as_bytes());
                assert_eq!(c.kernel_stack_address % PAGE_SIZE, 0, "{text}");
                assert!(c.kernel_stack_size >= 1, "{text}");
                assert!(c.kernel_stack_size <= MAX_STACK_PAGES, "{text}");
                assert!(
                    c.kernel_stack_address
                        .checked_add(c.kernel_stack_size * PAGE_SIZE)
                        .is_some(),
                    "{text}"
                );
            }
        }
    }
}
