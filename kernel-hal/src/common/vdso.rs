use core::fmt::{Debug, Error, Formatter};
use git_version::git_version;

/// This struct contains constants that are initialized by the kernel
/// once at boot time.  From the vDSO code's perspective, they are
/// read-only data that can never change.  Hence, no synchronization is
/// required to read them.
#[repr(C)]
#[derive(Debug)]
pub struct VdsoConstants {
    /// Maximum number of CPUs that might be online during the lifetime
    /// of the booted system.
    pub max_num_cpus: u32,
    /// Bit map indicating features.
    pub features: Features,
    /// Number of bytes in a data cache line.
    pub dcache_line_size: u32,
    /// Number of bytes in an instruction cache line.
    pub icache_line_size: u32,
    /// System page size in bytes.
    pub page_size: u32,
    pub padding: u32,
    /// Total amount of physical memory in the system, in bytes.
    pub physmem: u64,
    /// Actual length of `version_string`, not including the NUL terminator.
    pub version_string_len: u64,
    /// A NUL-terminated UTF-8 string returned by `zx_system_get_version_string`.
    pub version_string: VersionString,
}

/// Bit map indicating features.
///
/// For specific feature bits, see zircon/features.h.
#[repr(C)]
#[derive(Debug)]
pub struct Features {
    pub cpu: u32,
    /// Total amount of debug registers available in the system.
    pub hw_breakpoint_count: u32,
    pub hw_watchpoint_count: u32,
    /// Bitmask indicating which address tagging features are available.
    pub address_tagging: u32,
    /// Bitmask indicating which virtual memory features are available.
    pub vm: u32,
}

impl VdsoConstants {
    /// Set version string.
    ///
    /// A name longer than the field is cut on a character boundary rather
    /// than at the last byte that fits: a cut inside a multi-byte character
    /// leaves half of one behind, and [`VersionString::as_str`] panics on
    /// that instead of returning a shorter name. The rest of the field is
    /// then cleared, so the name is NUL-terminated as the field's own
    /// documentation says it is, and a second call cannot leave the tail of
    /// the first name standing behind the new one.
    pub fn set_version_string(&mut self, s: &str) {
        let mut len = s.len().min(self.version_string.0.len());
        while !s.is_char_boundary(len) {
            len -= 1;
        }
        self.version_string_len = len as u64;
        self.version_string.0[..len].copy_from_slice(&s.as_bytes()[..len]);
        self.version_string.0[len..].fill(0);
    }
}

#[repr(C)]
pub struct VersionString([u8; 64]);

impl VersionString {
    /// The name, without the padding behind it.
    ///
    /// The field is a fixed sixty-four bytes holding a NUL-terminated name,
    /// so everything from the NUL on is padding. This used to hand back the
    /// whole field, which put the fourteen NULs of a fifty-character name
    /// into the middle of `/proc/version`, where procps and neofetch read
    /// it, and into `/proc/sys/kernel/version`. [`Debug`] for the same field
    /// already stopped at the NUL.
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&c| c == 0).unwrap_or(self.0.len());
        core::str::from_utf8(&self.0[..end]).unwrap()
    }
}

impl Default for VersionString {
    fn default() -> Self {
        VersionString([0; 64])
    }
}

impl Debug for VersionString {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        for &c in self.0.iter().take_while(|&&c| c != 0) {
            write!(f, "{}", c as char)?;
        }
        Ok(())
    }
}

pub(crate) fn vdso_constants_template() -> VdsoConstants {
    let mut constants = VdsoConstants {
        max_num_cpus: crate::cpu::cpu_count() as u32,
        features: Features {
            cpu: 0,
            hw_breakpoint_count: 0,
            hw_watchpoint_count: 0,
            address_tagging: 0,
            vm: 0,
        },
        dcache_line_size: 0,
        icache_line_size: 0,
        page_size: crate::PAGE_SIZE as u32,
        padding: 0,
        physmem: 0,
        version_string_len: 0,
        version_string: Default::default(),
    };
    constants.set_version_string(git_version!(
        prefix = "git-",
        args = ["--always", "--abbrev=40", "--dirty=-dirty"]
    ));
    constants
}

/// The vDSO constants as the vDSO code and `/proc` read them: by offset for
/// the numbers, and through [`VersionString::as_str`] for the name, which
/// `linux_object::uname::os_version` hands to `/proc/version`,
/// `/proc/sys/kernel/version` and `uname(2)`.
#[cfg(test)]
mod vdso_tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    /// `hw_breakpoint_count` and `hw_watchpoint_count` are two `u32` in a row,
    /// so they trade places without a word of complaint and a debugger is told
    /// it has the other kind of register. Nothing else held this down:
    /// `vdso_constants_template` builds the struct field by field by name, so
    /// its initialiser reads the same whatever the order is.
    #[test]
    fn the_feature_counts_are_breakpoints_then_watchpoints() {
        assert_eq!(size_of::<Features>(), 20);
        assert_eq!(align_of::<Features>(), 4);
        assert_eq!(offset_of!(Features, cpu), 0);
        assert_eq!(offset_of!(Features, hw_breakpoint_count), 4);
        assert_eq!(offset_of!(Features, hw_watchpoint_count), 8);
        assert_eq!(offset_of!(Features, address_tagging), 12);
        assert_eq!(offset_of!(Features, vm), 16);
    }

    /// The constants are read at fixed offsets as well, and `page_size` and
    /// `padding` are the pair that would go unnoticed: a vDSO reading the
    /// padding word as the page size gets a zero.
    #[test]
    fn the_constants_are_laid_out_as_the_vdso_reads_them() {
        assert_eq!(size_of::<VdsoConstants>(), 120);
        assert_eq!(align_of::<VdsoConstants>(), 8);
        assert_eq!(offset_of!(VdsoConstants, max_num_cpus), 0);
        assert_eq!(offset_of!(VdsoConstants, features), 4);
        assert_eq!(offset_of!(VdsoConstants, dcache_line_size), 24);
        assert_eq!(offset_of!(VdsoConstants, icache_line_size), 28);
        assert_eq!(offset_of!(VdsoConstants, page_size), 32);
        assert_eq!(offset_of!(VdsoConstants, padding), 36);
        assert_eq!(offset_of!(VdsoConstants, physmem), 40);
        assert_eq!(offset_of!(VdsoConstants, version_string_len), 48);
        assert_eq!(offset_of!(VdsoConstants, version_string), 56);
    }

    /// `as_str` is what `/proc/version` carries, so it has to stop where the
    /// name does. The field is a fixed sixty-four bytes and the git name is
    /// fifty, so handing back the whole field put fourteen NULs in the middle
    /// of the `/proc/version` line.
    #[test]
    fn the_version_is_the_name_without_the_padding() {
        let c = vdso_constants_template();
        let s = c.version_string.as_str();
        assert!(s.starts_with("git-"), "the git name, not the field");
        assert!(!s.contains('\0'), "the padding is not part of the name");
        assert_eq!(s.len() as u64, c.version_string_len);
    }

    /// A second call used to leave the tail of the first name standing behind
    /// the new one, because it wrote the name and nothing else:
    /// `version_string_len` said two and `as_str` returned eight characters.
    #[test]
    fn a_shorter_name_does_not_leave_the_longer_one_behind_it() {
        let mut c = vdso_constants_template();
        c.set_version_string("abcdefgh");
        assert_eq!(c.version_string.as_str(), "abcdefgh");
        c.set_version_string("ab");
        assert_eq!(c.version_string_len, 2);
        assert_eq!(c.version_string.as_str(), "ab");
    }

    /// A name longer than the field is cut on a character boundary. Cutting at
    /// the sixty-fourth byte whatever was there left part of a character in
    /// the field, and `as_str`'s `from_utf8` panicked on it: a kernel panic
    /// out of reading `/proc/version`.
    ///
    /// Both widths are here because only the three-byte one holds the walk
    /// back as a *loop*. A two-byte character is never more than one step from
    /// the boundary below it, so an `if` in place of the `while` cuts it just
    /// as well; a three-byte character cut at its last byte takes two steps,
    /// and one step leaves the `from_utf8` panic exactly where it was.
    #[test]
    fn a_name_too_long_for_the_field_is_cut_on_a_character_boundary() {
        let two = "a".repeat(63) + "\u{00e9}\u{00e9}";
        assert_eq!(two.len(), 67, "the 64th byte is the tail of a character");
        let mut c = vdso_constants_template();
        c.set_version_string(&two);
        assert_eq!(c.version_string_len, 63);
        assert_eq!(c.version_string.as_str(), "a".repeat(63));

        let three = "a".repeat(62) + "\u{20ac}\u{20ac}";
        assert_eq!(
            three.len(),
            68,
            "the 64th byte is mid-character, twice over"
        );
        let mut c = vdso_constants_template();
        c.set_version_string(&three);
        assert_eq!(c.version_string_len, 62, "two steps back, not one");
        assert_eq!(c.version_string.as_str(), "a".repeat(62));
    }

    /// A name that fills the field exactly keeps every byte of it: there is no
    /// room left for a NUL, so `as_str` has to read to the end of the field
    /// rather than look for one.
    #[test]
    fn a_name_that_fills_the_field_exactly_keeps_every_byte_of_it() {
        let mut c = vdso_constants_template();
        let exact = "a".repeat(64);
        c.set_version_string(&exact);
        assert_eq!(c.version_string_len, 64);
        assert_eq!(c.version_string.as_str(), exact);
    }
}
