//! The numbers in this module that userspace or a device file chooses, and
//! that nothing inside the kernel can check.
//!
//! The `memfd_create` flags and the file seals next to them are already
//! pinned by `memfd_args_tests` and `memfd_seal_tests`. What was not, until
//! these tests, is the dirfd that means "the working directory", the symlink
//! depth, and the major and minor the input event nodes live at: measured by
//! mutation over the whole suite, each of those could be changed without a
//! single test noticing.
//!
//! They have nothing in common except the shape of the hole. Each is a
//! number some party outside this file already decided --- `<fcntl.h>`,
//! Linux's own `MAXSYMLINKS`, `devices.txt` --- and each is compared here
//! against a literal written out from that source and against nothing in
//! the tree.

use super::*;

#[test]
fn the_dirfd_that_means_the_working_directory_is_the_one_fcntl_h_gives() {
    // `AT_FDCWD`, which every `*at(2)` caller passes to mean "resolve this
    // path from the working directory". It is a word userspace chooses, and
    // this kernel only recognises it: a different number here and
    // `openat(AT_FDCWD, "x", ...)` looks for a real open fd numbered -100,
    // fails to find one, and every relative path in the system is EBADF.
    assert_eq!(i32::from(FileDesc::CWD), -100);
    // Negative, so it can never collide with a descriptor the fd table
    // handed out, and not -1, which is the "no fd" a great deal of other
    // code uses and which an `*at` call must NOT accept as the cwd.
    assert!(i32::from(FileDesc::CWD) < 0);
    assert_ne!(i32::from(FileDesc::CWD), -1);
    // And it really is the same descriptor the lookup path compares against.
    assert_eq!(FileDesc::CWD, FileDesc(-100));
}

#[test]
fn the_symlink_depth_is_the_one_linux_follows() {
    // Linux's `MAXSYMLINKS` (`include/linux/namei.h`) and POSIX's
    // `SYMLOOP_MAX` floor. The number is load-bearing in both directions:
    // lower, and a legitimate chain of symlinks --- which a distribution's
    // `/usr/bin` or `/etc/alternatives` tree can easily be --- gets ELOOP
    // where Linux resolves it; higher, and a loop costs more work before
    // this kernel notices it is one.
    assert_eq!(FOLLOW_MAX_DEPTH, 40);
}

#[test]
fn the_event_nodes_are_at_the_major_and_minor_devices_txt_gives() {
    // `Documentation/admin-guide/devices.txt`, character major 13: the
    // input core. It gives each device type 32 minors, so the base is not a
    // free choice.
    assert_eq!(EVDEV_MAJOR, 13, "the input core");
    assert_eq!(EVDEV_EVENT_MINOR_BASE, 64, "/dev/input/event0");
    // The three ranges below it, each named, because the failure of a base
    // that drifted down is not an error: it is an event node that is really
    // a mouse node, and a compositor reading the wrong device.
    const JS_FIRST: usize = 0;
    const MOUSE_FIRST: usize = 32;
    const MICE: usize = 63;
    for (name, minor) in [("jsN", JS_FIRST), ("mouseN", MOUSE_FIRST), ("mice", MICE)] {
        assert!(
            minor < EVDEV_EVENT_MINOR_BASE,
            "{} is below the event queues",
            name
        );
    }
    // One minor lower is `mice`, exactly.
    assert_eq!(EVDEV_EVENT_MINOR_BASE - 1, MICE);
    // And 32 minors per type is what puts the base where it is.
    assert_eq!(EVDEV_EVENT_MINOR_BASE, MOUSE_FIRST * 2);
}

#[test]
fn the_elf_cache_can_hold_the_largest_file_it_accepts() {
    // A file over `ELF_VMO_CACHE_FILE_MAX` is read uncached, so the total
    // has to be able to hold one of that size: were the ceiling the
    // smaller of the two, a file that passed the per-file check could never
    // fit, and every insert would evict the whole cache and then fail to
    // store anything.
    assert!(ELF_VMO_CACHE_FILE_MAX <= ELF_VMO_CACHE_MAX_BYTES);
    // And room for more than one of them, or a second exec would always
    // throw out the first and a warm cache would never stay warm.
    assert!(ELF_VMO_CACHE_MAX_BYTES >= ELF_VMO_CACHE_FILE_MAX * 2);
    // Both are whole pages: the cache accounts committed bytes, which are
    // only ever allocated a page at a time.
    for bytes in [ELF_VMO_CACHE_FILE_MAX, ELF_VMO_CACHE_MAX_BYTES] {
        assert_eq!(bytes % 4096, 0);
    }
}

#[test]
fn the_memfd_registry_never_prunes_on_every_create() {
    // `MEMFD_PRUNE_AT` starts here and is re-armed to twice what survived,
    // floored at this. At zero the floor would stop flooring: a desktop
    // that creates a memfd per window resize would walk and re-arm the
    // registry on every single one, under its lock, which is the cost this
    // threshold exists to avoid.
    assert!(MEMFD_PRUNE_MIN > 0);
    // Doubling what survived has to be able to exceed the floor, or the
    // threshold would never grow with the real load.
    assert!(MEMFD_PRUNE_MIN < usize::MAX / 2);
}
