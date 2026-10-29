//! The loader's half of hunter's measured boot: a SHA-256 of every input rboot
//! hands the kernel, recorded in [`BootInfo::measurements`](crate::BootInfo).
//!
//! Measuring is not verifying. rboot does not know what the digests should be
//! and never refuses to boot over them: the golden values live in the root
//! filesystem (`/etc/hunter/boot`), and the kernel's `hunter::boot` is the one
//! that compares and decides. Keeping the decision out of the loader is the
//! whole design -- a loader that refused to boot on a mismatch would turn a
//! kernel update the operator forgot to re-pin into a machine that shows a
//! splash and stops, and rboot's one rule is that it never bricks boot.
//!
//! ## What `measure=` costs
//!
//! The kernel ELF, `rboot.conf` and the cmdline are a few tens of megabytes
//! at most, and hashing them is lost in the time the firmware takes to read
//! them. The initramfs is the whole live root -- hundreds of megabytes -- and
//! a software SHA-256 over it is seconds on real hardware and much longer
//! under QEMU without KVM. So it is opt-in:
//!
//! * `measure=kernel` (the default): kernel, config, cmdline;
//! * `measure=all`: those and the initramfs, hashed while it is read so the
//!   progress bar keeps moving;
//! * `measure=off`: nothing.
//!
//! Turning measurement down is not a way around it: an input the operator
//! pinned and rboot did not measure is a violation of its own on the kernel
//! side ("pinned but not measured"), and `rboot.conf` -- where `measure=`
//! lives -- is measured under every setting but `off`.

use crate::Measurement;
use sha2::{Digest as _, Sha256};

/// The `measure=` config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureSet {
    /// Nothing is measured.
    Off,
    /// Kernel ELF, `rboot.conf` and cmdline.
    Kernel,
    /// All of [`MeasureSet::Kernel`] plus the initramfs.
    All,
}

impl MeasureSet {
    /// The `measure=` value, or `None` for anything else (the caller warns
    /// and keeps the default; a config value never panics the loader).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            v if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") => Some(Self::Off),
            v if v.eq_ignore_ascii_case("kernel") => Some(Self::Kernel),
            v if v.eq_ignore_ascii_case("all") => Some(Self::All),
            _ => None,
        }
    }

    /// Whether the kernel ELF, config and cmdline are measured.
    pub fn kernel(self) -> bool {
        self != Self::Off
    }

    /// Whether the initramfs is measured.
    pub fn initramfs(self) -> bool {
        self == Self::All
    }
}

/// The measurement of `bytes`, all at once.
pub fn of(bytes: &[u8]) -> Measurement {
    let mut h = Sha256::new();
    h.update(bytes);
    finish(h, bytes.len())
}

/// A started hash for an input that is read in pieces.
pub fn start() -> Sha256 {
    Sha256::new()
}

/// The measurement of `len` bytes fed to `hasher`.
pub fn finish(hasher: Sha256, len: usize) -> Measurement {
    Measurement {
        measured: true,
        size: len as u64,
        digest: hasher.finalize().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_values_parse_and_never_guess() {
        assert_eq!(MeasureSet::parse("all"), Some(MeasureSet::All));
        assert_eq!(MeasureSet::parse(" Kernel "), Some(MeasureSet::Kernel));
        assert_eq!(MeasureSet::parse("OFF"), Some(MeasureSet::Off));
        assert_eq!(MeasureSet::parse("none"), Some(MeasureSet::Off));
        assert_eq!(MeasureSet::parse("yes"), None);
        assert_eq!(MeasureSet::parse(""), None);
        assert!(MeasureSet::All.initramfs() && MeasureSet::All.kernel());
        assert!(MeasureSet::Kernel.kernel() && !MeasureSet::Kernel.initramfs());
        assert!(!MeasureSet::Off.kernel());
    }

    #[test]
    fn a_measurement_is_the_sha256_of_the_bytes() {
        // SHA-256("abc"), FIPS 180-2 appendix B.1.
        let m = of(b"abc");
        assert!(m.measured);
        assert_eq!(m.size, 3);
        assert_eq!(
            m.digest[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "not SHA-256: {:02x?}",
            m.digest
        );
        // In pieces, the same digest.
        let mut h = start();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(finish(h, 3), m);
    }
}
