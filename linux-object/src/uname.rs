//! System identification state shared by `uname(2)`, `sethostname(2)`,
//! `setdomainname(2)`, `/proc/version` and `/proc/sys/kernel/*`.
//!
//! Linux exposes the same six `utsname` strings through several interfaces and
//! real userspace mixes them freely (musl's `gethostname` reads `uname`,
//! `ps`/`procps` read `/proc/sys/kernel/osrelease`, glibc parses the release
//! at startup). Keeping the values here, in one place, is what makes those
//! views agree.

use alloc::string::{String, ToString};
use lock::Mutex;

/// `utsname.sysname` / `/proc/sys/kernel/ostype`. We are a Linux-compatible
/// kernel, and programs switch on this exact string.
pub const OS_TYPE: &str = "Eclipse";

/// `utsname.release` / `/proc/sys/kernel/osrelease` / `uname -r`.
///
/// The format must be Linux's `VERSION.PATCHLEVEL.SUBLEVEL-EXTRAVERSION`, and
/// the leading triple is not decoration. glibc parses it at startup in
/// `_dl_discover_osversion` and calls `__libc_fatal("FATAL: kernel too old")`
/// when it falls below the minimum it was configured with -- 3.2.0 for
/// Debian/Ubuntu builds. Go's runtime refuses pre-2.6.23 kernels, and
/// countless scripts do `uname -r | cut -d. -f1`.
///
/// This carried Eclipse's own product version ("0.5.3") for a while, which
/// parses as 0.5.3 and so is below every glibc minimum there has ever been:
/// no glibc-linked program could start at all, which is a large part of why
/// this tree only ever ran musl userspace. Eclipse's version belongs in the
/// EXTRAVERSION, exactly where a distribution puts its own ("6.6.0-18-generic"),
/// and [`ECLIPSE_VERSION`] keeps it available on its own.
///
/// 6.6 is the LTS whose ABI subset this tree implements: above every glibc
/// minimum in circulation, and consistent with the interfaces already here
/// that postdate 5.15 (`MFD_NOEXEC_SEAL` is 6.3).
pub const ECLIPSE_VERSION: &str = "0.5.5";
#[cfg(target_os = "none")]
pub const OS_RELEASE: &str = "0.5.5";
/// LibOS builds keep their distinguishing suffix, still in parseable form.
#[cfg(not(target_os = "none"))]
pub const OS_RELEASE: &str = "0.5.5-libos";

/// Maximum length of a host or domain name, per POSIX HOST_NAME_MAX on Linux
/// (`sethostname(2)` answers EINVAL above this).
pub const HOST_NAME_MAX: usize = 64;

static HOSTNAME: Mutex<Option<String>> = Mutex::new(None);
static DOMAINNAME: Mutex<Option<String>> = Mutex::new(None);

/// Default `utsname.nodename` until `sethostname` is called (Alpine's boot
/// scripts then install /etc/hostname over it).
const DEFAULT_HOSTNAME: &str = "Eclipse";
/// Default NIS `utsname.domainname`; Linux uses "(none)" when unset.
const DEFAULT_DOMAINNAME: &str = "(none)";

/// `utsname.version` / `/proc/sys/kernel/version`: free-form build banner.
pub fn os_version() -> String {
    kernel_hal::vdso::vdso_constants()
        .version_string
        .as_str()
        .to_string()
}

/// `__NEW_UTS_LEN + 1`: the width of each of the six `char[]` fields of
/// `struct utsname`, NUL included.
pub const UTS_FIELD_LEN: usize = HOST_NAME_MAX + 1;
/// `sizeof(struct utsname)`: six fields, 390 bytes.
pub const UTSNAME_LEN: usize = 6 * UTS_FIELD_LEN;

/// One field of `struct utsname`, as `uname(2)` writes it: at most
/// `__NEW_UTS_LEN` (64) bytes of the name, then NUL to the end of the
/// field. `sys_newuname` copies the whole 65-byte array, so a name that
/// does not fit is cut, never spilled into the next field.
pub fn utsname_field(name: &[u8]) -> [u8; UTS_FIELD_LEN] {
    let mut field = [0u8; UTS_FIELD_LEN];
    let n = name.len().min(HOST_NAME_MAX);
    field[..n].copy_from_slice(&name[..n]);
    field
}

/// The whole `struct utsname`, all 390 bytes, in field order: sysname,
/// nodename, release, version, machine, domainname.
pub fn utsname_bytes(fields: [&[u8]; 6]) -> [u8; UTSNAME_LEN] {
    let mut out = [0u8; UTSNAME_LEN];
    for (i, name) in fields.iter().enumerate() {
        out[i * UTS_FIELD_LEN..(i + 1) * UTS_FIELD_LEN].copy_from_slice(&utsname_field(name));
    }
    out
}

/// A host or domain name cut to `HOST_NAME_MAX` bytes, on a character
/// boundary. `sethostname` refuses a longer `len`, but the name it stores
/// is the UTF-8 rendering of the bytes it was given, and a byte that is
/// not UTF-8 renders as three (U+FFFD): 64 such bytes stored as 192, and
/// `uname` copied every one of them into a 65-byte field.
pub fn bounded_name(name: &str) -> &str {
    let mut n = name.len().min(HOST_NAME_MAX);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    &name[..n]
}

/// Current hostname (`utsname.nodename`, `/proc/sys/kernel/hostname`).
pub fn hostname() -> String {
    HOSTNAME
        .lock()
        .clone()
        .unwrap_or_else(|| DEFAULT_HOSTNAME.to_string())
}

/// Set the hostname from `sethostname(2)`. The caller has validated length.
pub fn set_hostname(name: &str) {
    *HOSTNAME.lock() = Some(bounded_name(name).to_string());
}

/// Current NIS domain name (`utsname.domainname`,
/// `/proc/sys/kernel/domainname`).
pub fn domainname() -> String {
    DOMAINNAME
        .lock()
        .clone()
        .unwrap_or_else(|| DEFAULT_DOMAINNAME.to_string())
}

/// Set the NIS domain name from `setdomainname(2)`.
pub fn set_domainname(name: &str) {
    *DOMAINNAME.lock() = Some(bounded_name(name).to_string());
}

#[cfg(test)]
mod utsname_tests {
    //! The six 65-byte fields of `struct utsname`, and the names that go in
    //! them. `uname` used to `write_cstring` each name at its field's
    //! offset, however long the name was.

    use super::*;

    /// A field is the name and then NUL to byte 65; 64 bytes fill it with
    /// the NUL last; the 65th byte and beyond are cut, not written past.
    #[test]
    fn a_field_is_at_most_64_bytes_then_nul_to_the_end() {
        let f = utsname_field(b"box");
        assert_eq!(&f[..4], b"box\0");
        assert!(f[4..].iter().all(|&b| b == 0));
        let f = utsname_field(&[b'x'; 64]);
        assert_eq!(f[63], b'x');
        assert_eq!(f[64], 0);
        let f = utsname_field(&[b'y'; 200]);
        assert_eq!(f[63], b'y');
        assert_eq!(f[64], 0);
        assert_eq!(f.len(), 65);
    }

    /// glibc reads `nodename` at 65 and `domainname` at 325, and the struct
    /// is 390 bytes: nothing written before, between or after.
    #[test]
    fn the_six_fields_sit_at_multiples_of_65() {
        let b = utsname_bytes([b"Eclipse", b"box", b"0.5.5", b"#1", b"x86_64", b"(none)"]);
        assert_eq!(b.len(), 390);
        assert_eq!(&b[0..8], b"Eclipse\0");
        assert_eq!(&b[65..69], b"box\0");
        assert_eq!(&b[130..136], b"0.5.5\0");
        assert_eq!(&b[195..198], b"#1\0");
        assert_eq!(&b[260..267], b"x86_64\0");
        assert_eq!(&b[325..332], b"(none)\0");
        assert_eq!(b[389], 0);
    }

    /// A stored name is at most 64 bytes, cut on a character boundary:
    /// 64 bytes that were not UTF-8 arrive as 64 U+FFFD, 192 bytes.
    #[test]
    fn a_stored_name_is_cut_to_64_bytes_on_a_character_boundary() {
        assert_eq!(bounded_name("box"), "box");
        let exact = "x".repeat(64);
        assert_eq!(bounded_name(&exact), exact);
        let long = "x".repeat(65);
        assert_eq!(bounded_name(&long).len(), 64);
        let lossy = String::from_utf8_lossy(&[0xe9u8; 64]).into_owned();
        assert_eq!(lossy.len(), 192);
        let cut = bounded_name(&lossy);
        assert_eq!(cut.len(), 63, "21 three-byte characters");
        assert!(cut.chars().all(|c| c == char::REPLACEMENT_CHARACTER));
    }
}

/// `/proc/version`, composed from the same values `uname(2)` reports so the
/// two never disagree (procps, neofetch and friends read this file).
pub fn proc_version() -> String {
    alloc::format!(
        "Eclipse version {} (eclipse@eclipse) {}\n",
        OS_RELEASE,
        os_version()
    )
}
