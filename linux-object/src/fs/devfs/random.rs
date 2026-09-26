//! `/dev/random` and `/dev/urandom`.
//!
//! Both come from [`kernel_hal::rand::fill_random`], which is the only source
//! of random bytes in the tree and says so in its own module documentation:
//! "it feeds `/dev/random` and `/dev/urandom`". It did not. `/dev/urandom`
//! called it; `/dev/random` had a K&R linear congruential generator over a
//! `u32` seeded with the constant `1`, so **the stronger of the two devices
//! was the weak one**, and it was not weak by degrees: a constant seed and a
//! deterministic generator mean the first bytes read from `/dev/random` are
//! the same on every boot of every machine, and the whole stream is
//! recoverable from a handful of them.
//!
//! In Linux the two have been the same generator since 4.8; `/dev/random` is
//! not the *weaker* device in any version of it, which is the whole reason a
//! program that wants key material reaches for it: gnupg's key generation,
//! `dbus-uuidgen`, `ssh-keygen` on some builds, and anything that reads it as
//! a seed for its own generator.
//!
//! `secure` is kept for the one thing that really does differ between the two:
//! the device number (`1:8` for `random`, `1:9` for `urandom`).

use core::any::Any;

use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

/// random INode struct
#[derive(Clone)]
pub struct RandomINode {
    secure: bool,
    inode_id: usize,
}

impl RandomINode {
    /// create a random INode
    /// - urandom -> secure = true
    /// - random -> secure = false
    ///
    /// The flag chooses the device number, not the quality of the bytes: both
    /// read from the same source.
    pub fn new(secure: bool) -> RandomINode {
        RandomINode {
            secure,
            inode_id: DevFS::new_inode_id(),
        }
    }
}

impl INode for RandomINode {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        kernel_hal::rand::fill_random(buf);
        Ok(buf.len())
    }

    /// A write is accepted and contributes nothing.
    ///
    /// Linux takes a write on either device as a contribution to the entropy
    /// pool and answers with the byte count. There is no pool here to add to,
    /// but refusing the write is not the way to say so: `rngd` and `haveged`
    /// exist to feed this device and treat a failed write as a fatal error, so
    /// `NotSupported` stopped them at start-up instead of letting them run
    /// harmlessly. `dd of=/dev/urandom`, which init scripts use to restore a
    /// saved seed at boot, failed the same way.
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }

    /// Always readable, and now always writable: `poll` said `write: false`
    /// while a write was refused outright, and it has to keep agreeing with
    /// `write_at` or a program that waits for writability before writing waits
    /// for ever.
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o666,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(1, if self.secure { 9 } else { 8 }),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod random_device_tests {
    //! `/dev/random` was a K&R linear congruential generator over a `u32`
    //! seeded with `1`, so the device a program reaches for when it wants key
    //! material was the one that handed out a compile-time constant.

    use super::*;

    /// The bytes `/dev/random` used to hand out: the first sixteen of the K&R
    /// generator seeded with `1`, which is what every boot of every machine
    /// produced.
    fn the_old_constant_stream() -> [u8; 16] {
        let mut seed: u32 = 1;
        let mut out = [0u8; 16];
        for x in out.iter_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *x = (seed / 65536) as u8;
        }
        out
    }

    #[test]
    fn random_does_not_hand_out_the_constant_stream_it_used_to() {
        let dev = RandomINode::new(false);
        let mut buf = [0u8; 16];
        assert_eq!(dev.read_at(0, &mut buf).unwrap(), buf.len());
        assert_ne!(
            buf,
            the_old_constant_stream(),
            "/dev/random is still the K&R generator seeded with 1"
        );
    }

    #[test]
    fn the_two_devices_read_from_the_same_source() {
        // Not "give the same bytes" -- they must not -- but "neither is the
        // deterministic one". A fresh read of each, and neither may repeat.
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        RandomINode::new(false).read_at(0, &mut first).unwrap();
        RandomINode::new(false).read_at(0, &mut second).unwrap();
        assert_ne!(
            first, second,
            "two /dev/random inodes gave the same bytes, so the source is seeded per inode"
        );

        let mut u_first = [0u8; 32];
        let mut u_second = [0u8; 32];
        RandomINode::new(true).read_at(0, &mut u_first).unwrap();
        RandomINode::new(true).read_at(0, &mut u_second).unwrap();
        assert_ne!(u_first, u_second);
    }

    #[test]
    fn the_flag_only_chooses_the_device_number() {
        // 1:8 is `random` and 1:9 is `urandom`, and that is the only thing the
        // flag is allowed to decide now.
        assert_eq!(
            RandomINode::new(false).metadata().unwrap().rdev,
            make_rdev(1, 8)
        );
        assert_eq!(
            RandomINode::new(true).metadata().unwrap().rdev,
            make_rdev(1, 9)
        );
        for secure in [false, true] {
            let m = RandomINode::new(secure).metadata().unwrap();
            assert_eq!(m.type_, FileType::CharDevice);
            assert_eq!(m.mode, 0o666);
        }
    }

    #[test]
    fn a_write_is_accepted_and_poll_says_so() {
        // `rngd` and `haveged` exist to write here and treat a failed write as
        // fatal; `dd of=/dev/urandom` restores a saved seed the same way.
        for secure in [false, true] {
            let dev = RandomINode::new(secure);
            assert_eq!(dev.write_at(0, &[0xa5; 64]).unwrap(), 64);
            assert_eq!(dev.write_at(0, &[]).unwrap(), 0);
            let p = dev.poll().unwrap();
            assert!(p.read, "a random device is always readable");
            assert!(
                p.write,
                "poll must agree with write_at or a writer waits for ever"
            );
        }
    }
}
