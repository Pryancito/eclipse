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

    /// Un dispositivo que no acepta un cambio de volumen: el nodo tiene
    /// que contestar EIO y no dar el cambio por hecho.
    struct Deaf;

    impl zcore_drivers::scheme::Scheme for Deaf {
        fn name(&self) -> &str {
            "deaf"
        }
    }

    impl AudioScheme for Deaf {
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
        fn set_gain(&self, _l: u8, _r: u8, _ml: bool, _mr: bool) -> DeviceResult {
            Err(zcore_drivers::DeviceError::NotSupported)
        }
    }

    const READ_STEREODEVS: u32 = 0x8004_4dfb;
    const READ_CAPS: u32 = 0x8004_4dfc;
    const READ_RECMASK: u32 = 0x8004_4dfd;
    const WRITE_RECSRC: u32 = 0xc004_4dff;
    const READ_LINE: u32 = 0x8004_4d06;
    const WRITE_MIC: u32 = 0xc004_4d07;
    const READ_CHANNEL_24: u32 = 0x8004_4d18;
    const READ_CHANNEL_25: u32 = 0x8004_4d19;

    /// Las cuatro máscaras que `amixer`/`aumix` leen antes de dibujar
    /// nada: hay volumen y PCM, los dos estéreo, y no se graba. Un
    /// `RECMASK` o un `CAPS` distintos de cero harían que un mezclador
    /// ofreciera una entrada que no existe.
    #[test]
    fn the_four_masks_say_two_stereo_channels_and_no_recording() {
        let (_, dev) = mixer();
        assert_eq!(ioctl_int(&dev, READ_DEVMASK, 0).unwrap(), DEVMASK);
        assert_eq!(ioctl_int(&dev, READ_STEREODEVS, 0).unwrap(), DEVMASK);
        assert_eq!(ioctl_int(&dev, READ_RECMASK, 0).unwrap(), 0);
        assert_eq!(ioctl_int(&dev, READ_CAPS, 0).unwrap(), 0);
        // La máscara son exactamente los dos canales que este mezclador
        // tiene, y ninguno más.
        assert_eq!(
            DEVMASK,
            (1 << SOUND_MIXER_VOLUME) | (1 << SOUND_MIXER_PCM)
        );
        // `RECSRC` contesta cero se lea o se escriba: no hay fuente de
        // grabación que elegir.
        assert_eq!(ioctl_int(&dev, READ_RECSRC, 0).unwrap(), 0);
        assert_eq!(ioctl_int(&dev, WRITE_RECSRC, 1).unwrap(), 0);
    }

    /// Los canales que esta tarjeta no tiene son EINVAL, uno por uno —
    /// no el volumen por error. Un mezclador que leyera «micrófono» y
    /// recibiera el volumen maestro mostraría un control que al moverlo
    /// cambia el sonido de otro sitio.
    #[test]
    fn every_channel_this_card_does_not_have_is_einval() {
        let (_, dev) = mixer();
        for nr in 0..SOUND_MIXER_NRDEVICES {
            if nr == SOUND_MIXER_VOLUME || nr == SOUND_MIXER_PCM {
                continue;
            }
            let read = 0x8004_4d00 | nr;
            let write = 0xc004_4d00 | nr;
            assert!(
                matches!(ioctl_int(&dev, read, 0), Err(FsError::InvalidParam)),
                "leer el canal {} no es EINVAL",
                nr
            );
            assert!(
                matches!(ioctl_int(&dev, write, 50), Err(FsError::InvalidParam)),
                "escribir el canal {} no es EINVAL",
                nr
            );
        }
        // Y los de fuera del rango de canales también, que son los que
        // caen por el último brazo.
        assert!(matches!(
            ioctl_int(&dev, READ_CHANNEL_24, 0),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&dev, READ_CHANNEL_25, 0),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&dev, READ_LINE, 0),
            Err(FsError::InvalidParam)
        ));
        assert!(matches!(
            ioctl_int(&dev, WRITE_MIC, 0),
            Err(FsError::InvalidParam)
        ));
    }

    /// Un ioctl que no es de la familia 'M', o que es de 'M' pero no de
    /// tamaño `int`, no es de este nodo.
    #[test]
    fn an_ioctl_from_another_family_or_another_size_is_einval() {
        let (_, dev) = mixer();
        // Familia 'P' (el DSP).
        assert!(matches!(
            ioctl_int(&dev, 0xc004_5002, 0),
            Err(FsError::InvalidParam)
        ));
        // 'M' con el número del volumen pero con un tamaño que no es el
        // de un `int`: no se puede tratar como un entero.
        assert!(matches!(
            ioctl_int(&dev, 0x8008_4d00, 0),
            Err(FsError::InvalidParam)
        ));
        // Y un ioctl sin dirección.
        assert!(matches!(
            ioctl_int(&dev, 0x0000_4d00, 0),
            Err(FsError::InvalidParam)
        ));
    }

    /// `SOUND_MIXER_INFO` es lo que `aumix` y los mezcladores viejos
    /// leen para dar nombre a la tarjeta, en sus dos versiones (la de
    /// ahora y la `_old_mixer_info`, que es la misma sin los contadores).
    /// Las dos salen con el nombre del dispositivo y el id de la tarjeta.
    #[test]
    fn the_mixer_info_names_the_card_in_both_its_layouts() {
        let audio = Arc::new(FakeGain(Mutex::new((100, 100, false, false))));
        let dev = MixerDev::new(audio, 3);

        let mut info = MixerInfo {
            id: [b'z'; 16],
            name: [b'z'; 32],
            modify_counter: 7,
            fillers: [7; 10],
        };
        dev.io_control(SOUND_MIXER_INFO, &mut info as *mut MixerInfo as usize)
            .unwrap();
        assert_eq!(cstr(&info.id), "EclipseHDA3");
        assert_eq!(cstr(&info.name), "fake-audio");
        // Y lo que había detrás del nombre se ha limpiado.
        assert!(info.id[11..].iter().all(|&b| b == 0));
        assert!(info.name[10..].iter().all(|&b| b == 0));

        let mut old = OldMixerInfo {
            id: [b'z'; 16],
            name: [b'z'; 32],
        };
        dev.io_control(
            SOUND_OLD_MIXER_INFO,
            &mut old as *mut OldMixerInfo as usize,
        )
        .unwrap();
        assert_eq!(cstr(&old.id), "EclipseHDA3");
        assert_eq!(cstr(&old.name), "fake-audio");
    }

    fn cstr(buf: &[u8]) -> &str {
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        core::str::from_utf8(&buf[..n]).unwrap_or("")
    }

    /// `fill_cstr` corta lo que no cabe dejando el NUL, y NO borra la
    /// cola: el nodo pone a cero la estructura entera antes de
    /// rellenarla, que es de donde sale la limpieza que la prueba de
    /// arriba comprueba.
    #[test]
    fn a_name_that_does_not_fit_is_truncated_with_room_for_its_nul() {
        let mut buf = [0u8; 8];
        fill_cstr(&mut buf, "abc");
        assert_eq!(&buf, b"abc\0\0\0\0\0");
        fill_cstr(&mut buf, "abcdefghij");
        assert_eq!(&buf[..7], b"abcdefg");
        assert_eq!(buf[7], 0);
        // Un destino de un solo byte no escribe nada y no se sale.
        let mut one = [0u8; 1];
        fill_cstr(&mut one, "abc");
        assert_eq!(one, [0]);
        // Y uno vacío tampoco.
        let mut none: [u8; 0] = [];
        fill_cstr(&mut none, "abc");
    }

    /// Un volumen negativo o con basura en los bytes altos no se cuela:
    /// el nodo se queda con los dos bytes bajos y los recorta a 100.
    #[test]
    fn the_high_bytes_of_the_volume_word_are_not_part_of_it() {
        let (audio, dev) = mixer();
        // Sólo los dos bytes bajos cuentan.
        assert_eq!(
            ioctl_int(&dev, WRITE_VOLUME, 0x7fff_0000 | 40 | (60 << 8)).unwrap(),
            40 | (60 << 8)
        );
        assert_eq!(audio.gain(), (40, 60, false, false));
        // Un negativo tiene los bytes bajos a uno: 0xff recortado es 100.
        assert_eq!(ioctl_int(&dev, WRITE_VOLUME, -1).unwrap(), 100 | (100 << 8));
        assert_eq!(audio.gain(), (100, 100, false, false));
        // Y el cero de los dos lados silencia los dos.
        assert_eq!(ioctl_int(&dev, WRITE_PCM, 0).unwrap(), 0);
        assert_eq!(audio.gain(), (0, 0, true, true));
        // Un canal silenciado lee cero por mucho que su porcentaje no lo
        // sea: es lo que hace `snd_mixer_oss_get_volume1`.
        audio.set_gain(90, 90, true, false).unwrap();
        assert_eq!(ioctl_int(&dev, READ_VOLUME, 0).unwrap(), 90 << 8);
    }

    /// Un dispositivo que no deja tocar la ganancia contesta EIO, y la
    /// versión de OSS se contesta igual (no depende del dispositivo).
    #[test]
    fn a_device_that_refuses_the_gain_answers_eio() {
        let dev = MixerDev::new(Arc::new(Deaf), 0);
        assert!(matches!(
            ioctl_int(&dev, WRITE_VOLUME, 50),
            Err(FsError::DeviceError)
        ));
        assert!(matches!(
            ioctl_int(&dev, WRITE_PCM, 50),
            Err(FsError::DeviceError)
        ));
        // Leer sigue funcionando: la ganancia por defecto del trait.
        assert_eq!(ioctl_int(&dev, READ_VOLUME, 0).unwrap(), 100 | (100 << 8));
        assert_eq!(
            ioctl_int(&dev, OSS_GETVERSION, 0).unwrap(),
            SNDRV_OSS_VERSION
        );
    }

    /// Una dirección que no es de usuario es EFAULT y no un panic: todo
    /// lo que entra aquí viene de un `ioctl(2)`.
    #[test]
    fn a_bad_address_is_efault() {
        let (_, dev) = mixer();
        for cmd in [
            OSS_GETVERSION,
            SOUND_MIXER_INFO,
            SOUND_OLD_MIXER_INFO,
            READ_DEVMASK,
            READ_VOLUME,
            WRITE_VOLUME,
            READ_RECSRC,
        ] {
            assert!(
                matches!(dev.io_control(cmd, 0), Err(FsError::BadAddress)),
                "{:#x} no es EFAULT",
                cmd
            );
        }
    }

    /// El nodo no se lee ni se escribe, su `poll` no señala nada (no hay
    /// eventos de mezclador) y su número de dispositivo es el de OSS:
    /// mayor 14, menor `índice * 16`.
    #[test]
    fn the_node_is_ioctl_only_and_numbered_like_oss() {
        let (_, dev) = mixer();
        assert!(matches!(
            dev.read_at(0, &mut [0u8; 4]),
            Err(FsError::NotSupported)
        ));
        assert!(matches!(
            dev.write_at(0, &[0u8; 4]),
            Err(FsError::NotSupported)
        ));
        let p = dev.poll().unwrap();
        assert_eq!(
            (p.read, p.write, p.error, p.hangup),
            (false, false, false, false)
        );

        let m = dev.metadata().unwrap();
        assert_eq!(m.type_, FileType::CharDevice);
        assert_eq!(m.mode, 0o666);
        assert_eq!(m.rdev, make_rdev(14, 0));
        for index in [1usize, 2, 7] {
            let audio = Arc::new(FakeGain(Mutex::new((100, 100, false, false))));
            let dev = MixerDev::new(audio, index);
            assert_eq!(dev.metadata().unwrap().rdev, make_rdev(14, index * 16));
        }
        // Cada nodo tiene su propio inodo.
        let (_, other) = mixer();
        assert_ne!(
            dev.metadata().unwrap().inode,
            other.metadata().unwrap().inode
        );
        // Y se puede recuperar el tipo concreto por `as_any_ref`.
        assert!(dev.as_any_ref().downcast_ref::<MixerDev>().is_some());
    }
}
