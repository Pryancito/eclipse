//! `/dev/mixer` — the OSS mixer over an [`AudioScheme`] device's gain,
//! modelled on Linux's `snd-mixer-oss` (`sound/core/oss/mixer_oss.c`).
//!
//! Linux maps the card's `Master` control onto `SOUND_MIXER_VOLUME` and its
//! `PCM` control onto `SOUND_MIXER_PCM`. This card has one gain (the HDA
//! driver scales S16LE on the way into the ring; HDMI/DP pins have no analog
//! volume), so both OSS channels read and write that one control: a player
//! that adjusts `pcm` (mpv's default mixer channel) and a `aumix -v` on
//! `vol` both land on it, which is what a user of either expects to hear.
//! Setting a channel to 0 mutes it (`Master Playback Switch` off) and any
//! other level unmutes it, the way Linux's `snd_mixer_oss_put_volume1` does.
//! There is no recording, so `RECSRC`/`RECMASK` are 0 and `CAPS` is 0.
//! Anything else is `EINVAL`, as on Linux.

use alloc::sync::Arc;
use core::any::Any;

use kernel_hal::drivers::scheme::AudioScheme;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

use super::dsp::SNDRV_OSS_VERSION;

const IOC_TYPE_M: u32 = b'M' as u32;
const IOC_READ: u32 = 2 << 30;
const IOC_WRITE: u32 = 1 << 30;

const OSS_GETVERSION: u32 = 0x8004_4d76; // _IOR('M', 118, int)
const SOUND_MIXER_INFO: u32 = 0x805c_4d65; // _IOR('M', 101, mixer_info)
const SOUND_OLD_MIXER_INFO: u32 = 0x8030_4d65; // _IOR('M', 101, _old_mixer_info)

const SOUND_MIXER_VOLUME: u32 = 0;
const SOUND_MIXER_PCM: u32 = 4;
const SOUND_MIXER_NRDEVICES: u32 = 25;
const SOUND_MIXER_RECSRC: u32 = 0xff;
const SOUND_MIXER_DEVMASK: u32 = 0xfe;
const SOUND_MIXER_RECMASK: u32 = 0xfd;
const SOUND_MIXER_CAPS: u32 = 0xfc;
const SOUND_MIXER_STEREODEVS: u32 = 0xfb;

const DEVMASK: i32 = (1 << SOUND_MIXER_VOLUME) | (1 << SOUND_MIXER_PCM);

/// `mixer_info`.
#[derive(Clone, Copy)]
#[repr(C)]
struct MixerInfo {
    id: [u8; 16],
    name: [u8; 32],
    modify_counter: i32,
    fillers: [i32; 10],
}

/// `_old_mixer_info`.
#[derive(Clone, Copy)]
#[repr(C)]
struct OldMixerInfo {
    id: [u8; 16],
    name: [u8; 32],
}

fn uread<T: Copy>(addr: usize) -> Result<T> {
    if !kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>())
        || !addr.is_multiple_of(core::mem::align_of::<T>())
    {
        return Err(FsError::BadAddress);
    }
    Ok(unsafe { core::ptr::read_unaligned(addr as *const T) })
}

fn uwrite<T: Copy>(addr: usize, val: T) -> Result<()> {
    if !kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>())
        || !addr.is_multiple_of(core::mem::align_of::<T>())
    {
        return Err(FsError::BadAddress);
    }
    unsafe { core::ptr::write_unaligned(addr as *mut T, val) };
    Ok(())
}

fn fill_cstr(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

pub struct MixerDev {
    audio: Arc<dyn AudioScheme>,
    index: usize,
    inode_id: usize,
}

impl MixerDev {
    pub fn new(audio: Arc<dyn AudioScheme>, index: usize) -> Self {
        MixerDev {
            audio,
            index,
            inode_id: DevFS::new_inode_id(),
        }
    }

    /// The OSS volume word: left in the low byte, right in the next, both
    /// percentages; a muted channel reads as 0.
    fn read_volume(&self) -> i32 {
        let (l, r, mute_l, mute_r) = self.audio.gain();
        let l = if mute_l { 0 } else { l as i32 };
        let r = if mute_r { 0 } else { r as i32 };
        l | (r << 8)
    }

    fn write_volume(&self, val: i32) -> Result<i32> {
        let l = (val & 0xff).min(100) as u8;
        let r = ((val >> 8) & 0xff).min(100) as u8;
        self.audio
            .set_gain(l, r, l == 0, r == 0)
            .map_err(|_| FsError::DeviceError)?;
        Ok(self.read_volume())
    }
}

impl INode for MixerDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: false,
            error: false,
            hangup: false,
        })
    }

    #[allow(unsafe_code)]
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        match cmd {
            OSS_GETVERSION => uwrite::<i32>(data, SNDRV_OSS_VERSION)?,
            SOUND_MIXER_INFO => {
                let mut info = MixerInfo {
                    id: [0; 16],
                    name: [0; 32],
                    modify_counter: 0,
                    fillers: [0; 10],
                };
                fill_cstr(&mut info.id, &alloc::format!("EclipseHDA{}", self.index));
                fill_cstr(&mut info.name, self.audio.name());
                uwrite::<MixerInfo>(data, info)?;
            }
            SOUND_OLD_MIXER_INFO => {
                let mut info = OldMixerInfo {
                    id: [0; 16],
                    name: [0; 32],
                };
                fill_cstr(&mut info.id, &alloc::format!("EclipseHDA{}", self.index));
                fill_cstr(&mut info.name, self.audio.name());
                uwrite::<OldMixerInfo>(data, info)?;
            }
            // SOUND_MIXER_READ_* (_IOR('M', n, int)) and SOUND_MIXER_WRITE_*
            // (_IOWR('M', n, int)): an int-sized 'M' ioctl.
            _ if (cmd >> 8) & 0xff == IOC_TYPE_M && (cmd >> 16) & 0x3fff == 4 => {
                let nr = cmd & 0xff;
                let dir = cmd & (IOC_READ | IOC_WRITE);
                match (nr, dir) {
                    (SOUND_MIXER_DEVMASK | SOUND_MIXER_STEREODEVS, IOC_READ) => {
                        uwrite::<i32>(data, DEVMASK)?
                    }
                    (SOUND_MIXER_RECMASK | SOUND_MIXER_CAPS, IOC_READ) => uwrite::<i32>(data, 0)?,
                    (SOUND_MIXER_RECSRC, _) => uwrite::<i32>(data, 0)?,
                    (SOUND_MIXER_VOLUME | SOUND_MIXER_PCM, IOC_READ) => {
                        uwrite::<i32>(data, self.read_volume())?
                    }
                    (SOUND_MIXER_VOLUME | SOUND_MIXER_PCM, _) if dir == IOC_READ | IOC_WRITE => {
                        let val = uread::<i32>(data)?;
                        let now = self.write_volume(val)?;
                        uwrite::<i32>(data, now)?;
                    }
                    (n, _) if n < SOUND_MIXER_NRDEVICES => {
                        // A channel this mixer does not have.
                        return Err(FsError::InvalidParam);
                    }
                    _ => return Err(FsError::InvalidParam),
                }
            }
            _ => {
                debug!("[mixer{}] unknown ioctl {:#x}", self.index, cmd);
                return Err(FsError::InvalidParam);
            }
        }
        Ok(0)
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
            // OSS numbering: /dev/mixer is (14, 0), /dev/mixer1 is (14, 16), …
            rdev: make_rdev(14, self.index * 16),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lock::Mutex;
    use zcore_drivers::DeviceResult;

    struct FakeGain(Mutex<(u8, u8, bool, bool)>);

    impl zcore_drivers::scheme::Scheme for FakeGain {
        fn name(&self) -> &str {
            "fake-audio"
        }
    }

    impl AudioScheme for FakeGain {
        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            Ok((rate, channels))
        }
        fn params(&self) -> (u32, u8) {
            (48_000, 2)
        }
        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            Ok(pcm.len())
        }
        fn free_bytes(&self) -> usize {
            0
        }
        fn buffer_bytes(&self) -> usize {
            0
        }
        fn queued_bytes(&self) -> usize {
            0
        }
        fn reset(&self) -> DeviceResult {
            Ok(())
        }
        fn set_gain(&self, l: u8, r: u8, ml: bool, mr: bool) -> DeviceResult {
            *self.0.lock() = (l, r, ml, mr);
            Ok(())
        }
        fn gain(&self) -> (u8, u8, bool, bool) {
            *self.0.lock()
        }
    }

    fn mixer() -> (Arc<FakeGain>, MixerDev) {
        let audio = Arc::new(FakeGain(Mutex::new((100, 100, false, false))));
        (audio.clone(), MixerDev::new(audio, 0))
    }

    fn ioctl_int(dev: &MixerDev, cmd: u32, val: i32) -> Result<i32> {
        let mut v = val;
        dev.io_control(cmd, &mut v as *mut i32 as usize)?;
        Ok(v)
    }

    const READ_DEVMASK: u32 = 0x8004_4dfe;
    const READ_RECSRC: u32 = 0x8004_4dff;
    const READ_VOLUME: u32 = 0x8004_4d00;
    const WRITE_VOLUME: u32 = 0xc004_4d00;
    const READ_PCM: u32 = 0x8004_4d04;
    const WRITE_PCM: u32 = 0xc004_4d04;
    const READ_MIC: u32 = 0x8004_4d07;

    #[test]
    fn the_mixer_has_volume_and_pcm_and_nothing_records() {
        let (_, dev) = mixer();
        assert_eq!(ioctl_int(&dev, READ_DEVMASK, 0).unwrap(), DEVMASK);
        assert_eq!(ioctl_int(&dev, READ_RECSRC, 0).unwrap(), 0);
        assert_eq!(
            ioctl_int(&dev, OSS_GETVERSION, 0).unwrap(),
            SNDRV_OSS_VERSION
        );
        assert!(matches!(
            ioctl_int(&dev, READ_MIC, 0),
            Err(FsError::InvalidParam)
        ));
    }

    #[test]
    fn volume_and_pcm_are_the_one_gain_and_zero_mutes() {
        let (audio, dev) = mixer();
        assert_eq!(ioctl_int(&dev, READ_VOLUME, 0).unwrap(), 100 | (100 << 8));
        // Right 50, left 80; the ioctl hands the applied value back.
        assert_eq!(
            ioctl_int(&dev, WRITE_PCM, 80 | (50 << 8)).unwrap(),
            80 | (50 << 8)
        );
        assert_eq!(audio.gain(), (80, 50, false, false));
        assert_eq!(ioctl_int(&dev, READ_VOLUME, 0).unwrap(), 80 | (50 << 8));
        // Over 100 is clamped, as amixer would.
        assert_eq!(
            ioctl_int(&dev, WRITE_VOLUME, 200 | (30 << 8)).unwrap(),
            100 | (30 << 8)
        );
        // Zero on one side mutes that side only.
        assert_eq!(ioctl_int(&dev, WRITE_VOLUME, 60).unwrap(), 60);
        assert_eq!(audio.gain(), (60, 0, false, true));
        assert_eq!(ioctl_int(&dev, READ_PCM, 0).unwrap(), 60);
    }
}
