//! Los tests del nodo de control de `/dev/snd/controlC<N>`: lo que
//! `amixer`, `alsamixer` y el sumidero de PulseAudio preguntan antes de
//! tocar una sola muestra.
//!
//! `CtlDev` no tenía ninguna prueba. Es el nodo que decide si una
//! tarjeta EXISTE para ALSA: `alsamixer` aborta si `SUBSCRIBE_EVENTS`
//! falla, alsa-lib descubre que no hay captura porque `PCM_INFO` con
//! `stream = 1` contesta ENOENT, y el mezclador simple compone «Master»
//! a partir de los dos nombres exactos de los elementos. Todo eso son
//! contratos con userspace que no se ven desde ningún test del PCM.

use super::*;
use zcore_drivers::DeviceResult;

struct FakeGain {
    gain: Mutex<(u8, u8, bool, bool)>,
}

impl FakeGain {
    fn new() -> Arc<Self> {
        Arc::new(FakeGain {
            gain: Mutex::new((100, 100, false, false)),
        })
    }
}

impl zcore_drivers::scheme::Scheme for FakeGain {
    fn name(&self) -> &str {
        "HDMI/DP pin 3"
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
        4096
    }
    fn queued_bytes(&self) -> usize {
        0
    }
    fn reset(&self) -> DeviceResult {
        Ok(())
    }
    fn set_gain(&self, l: u8, r: u8, ml: bool, mr: bool) -> DeviceResult {
        *self.gain.lock() = (l, r, ml, mr);
        Ok(())
    }
    fn gain(&self) -> (u8, u8, bool, bool) {
        *self.gain.lock()
    }
}

/// Un dispositivo que no deja tocar el volumen: el nodo tiene que
/// contestar EIO y no dar el cambio por hecho.
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

const CTL_VERSION: u32 = 0x0000_5500;
const CTL_CARD_INFO: u32 = 0x0000_5501;
const CTL_ELEM_LIST: u32 = 0x0000_5510;
const CTL_ELEM_INFO: u32 = 0x0000_5511;
const CTL_ELEM_READ: u32 = 0x0000_5512;
const CTL_ELEM_WRITE: u32 = 0x0000_5513;
const CTL_ELEM_LOCK: u32 = 0x0000_5514;
const CTL_ELEM_UNLOCK: u32 = 0x0000_5515;
const CTL_SUBSCRIBE_EVENTS: u32 = 0x0000_5516;
const CTL_TLV_READ: u32 = 0x0000_551a;
const CTL_HWDEP_NEXT: u32 = 0x0000_5520;
const CTL_PCM_NEXT_DEVICE: u32 = 0x0000_5530;
const CTL_PCM_INFO: u32 = 0x0000_5531;
const CTL_PCM_PREFER_SUBDEVICE: u32 = 0x0000_5532;
const CTL_RAWMIDI_NEXT: u32 = 0x0000_5540;
const CTL_POWER: u32 = 0x0000_55d0;
const CTL_POWER_STATE: u32 = 0x0000_55d1;

fn ctl() -> (Arc<FakeGain>, CtlDev) {
    let audio = FakeGain::new();
    (audio.clone(), CtlDev::new(audio.clone(), 0))
}

fn ioctl_int(dev: &CtlDev, cmd: u32, val: i32) -> Result<i32> {
    let mut v = val;
    dev.io_control(cmd, &mut v as *mut i32 as usize)?;
    Ok(v)
}

fn id_of(elem: MixerElem) -> SndCtlElemId {
    let mut id: SndCtlElemId = unsafe { core::mem::zeroed() };
    fill_elem_id(&mut id, elem);
    id
}

fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

// ── Los ayudantes de nombres e ids ──────────────────────────────────

/// Un nombre de elemento se lee hasta el NUL, y uno que llena el búfer
/// entero sin NUL se lee entero en vez de salirse. Unos bytes que no
/// son UTF-8 son un nombre vacío, no un panic: vienen de userspace.
#[test]
fn a_name_is_read_up_to_its_nul_and_never_past_the_buffer() {
    let mut buf = [0u8; 44];
    buf[..6].copy_from_slice(b"Master");
    assert_eq!(cstr_name(&buf), "Master");
    // Basura después del NUL: no es parte del nombre.
    buf[7] = b'X';
    assert_eq!(cstr_name(&buf), "Master");
    // Sin NUL en ninguna parte: el búfer entero.
    let full = [b'a'; 44];
    assert_eq!(cstr_name(&full), core::str::from_utf8(&full).unwrap());
    // Vacío.
    assert_eq!(cstr_name(&[0u8; 44]), "");
    // No UTF-8.
    let mut bad = [0u8; 44];
    bad[0] = 0xff;
    assert_eq!(cstr_name(&bad), "");
}

/// `fill_cstr` siempre deja el final en NUL — también cuando lo que
/// escribe no cabe — y borra lo que hubiera detrás: estos búferes se
/// rellenan sobre memoria del cliente, y el resto de un nombre anterior
/// convertiría «Master» en «Masterx».
#[test]
fn filling_a_name_truncates_with_a_nul_and_wipes_the_tail() {
    let mut buf = [b'x'; 8];
    fill_cstr(&mut buf, "abc");
    assert_eq!(&buf, b"abc\0\0\0\0\0");
    // Justo lo que cabe menos el NUL.
    let mut buf = [b'x'; 8];
    fill_cstr(&mut buf, "abcdefg");
    assert_eq!(&buf, b"abcdefg\0");
    // Más de lo que cabe: se corta y el NUL sigue estando.
    let mut buf = [b'x'; 8];
    fill_cstr(&mut buf, "abcdefghij");
    assert_eq!(&buf, b"abcdefg\0");
    // Y un búfer con un nombre más largo dentro queda limpio.
    let mut buf = *b"MasterPlay";
    fill_cstr(&mut buf, "PCM");
    assert_eq!(&buf, b"PCM\0\0\0\0\0\0\0");
}

/// Los dos elementos se identifican por numid Y por nombre, con los
/// nombres EXACTOS que el mezclador simple de alsa-lib compone en
/// «Master»; cualquier otro es ENOENT y no el primero de la lista.
#[test]
fn an_element_is_found_by_its_numid_or_by_its_exact_name() {
    assert!(matches!(mixer_elem_at(0), Some(MixerElem::Volume)));
    assert!(matches!(mixer_elem_at(1), Some(MixerElem::Switch)));
    assert!(mixer_elem_at(2).is_none());
    assert!(mixer_elem_at(u32::MAX).is_none());

    let vol = id_of(MixerElem::Volume);
    assert_eq!(vol.numid, NUMID_VOLUME);
    assert_eq!(vol.iface, IFACE_MIXER);
    assert_eq!(cstr_name(&vol.name), NAME_VOLUME);
    assert_eq!((vol.device, vol.subdevice, vol.index), (0, 0, 0));
    let sw = id_of(MixerElem::Switch);
    assert_eq!(sw.numid, NUMID_SWITCH);
    assert_eq!(cstr_name(&sw.name), NAME_SWITCH);

    // Ida y vuelta por numid.
    assert!(matches!(elem_from_id(&vol), Some(MixerElem::Volume)));
    assert!(matches!(elem_from_id(&sw), Some(MixerElem::Switch)));

    // Y por nombre, sin numid: es como pregunta `amixer sset Master`.
    let by_name = |name: &str| {
        let mut id: SndCtlElemId = zeroed();
        fill_cstr(&mut id.name, name);
        elem_from_id(&id)
    };
    assert!(matches!(by_name(NAME_VOLUME), Some(MixerElem::Volume)));
    assert!(matches!(by_name(NAME_SWITCH), Some(MixerElem::Switch)));
    // Un nombre parecido no vale.
    assert!(by_name("Master Playback Volum").is_none());
    assert!(by_name("master playback volume").is_none());
    assert!(by_name("PCM Playback Volume").is_none());
    assert!(by_name("").is_none());
    // Y un numid que no existe tampoco, por mucho que el nombre esté.
    let mut odd = id_of(MixerElem::Volume);
    odd.numid = 99;
    assert!(matches!(elem_from_id(&odd), Some(MixerElem::Volume)));
    odd.name = [0; 44];
    assert!(elem_from_id(&odd).is_none());
}

/// Rellenar un id lo deja LIMPIO: el cliente manda la estructura con lo
/// que tenía dentro y lo que lee de vuelta no puede llevar su propio
/// `index` ni un `device` de otra tarjeta.
#[test]
fn filling_an_id_wipes_what_the_client_sent() {
    let mut id = SndCtlElemId {
        numid: 7,
        iface: 99,
        device: 3,
        subdevice: 4,
        name: [b'z'; 44],
        index: 5,
    };
    fill_elem_id(&mut id, MixerElem::Switch);
    assert_eq!(id.numid, NUMID_SWITCH);
    assert_eq!(id.iface, IFACE_MIXER);
    assert_eq!((id.device, id.subdevice, id.index), (0, 0, 0));
    assert_eq!(cstr_name(&id.name), NAME_SWITCH);
}

// ── El nodo ─────────────────────────────────────────────────────────

/// Sólo los ioctl de la familia 'U' son de este nodo; el resto no
/// existe aquí.
#[test]
fn only_the_u_family_belongs_to_this_node() {
    let (_, dev) = ctl();
    assert_eq!(ioctl_int(&dev, CTL_VERSION, 0).unwrap(), SNDRV_CTL_VERSION);
    // Un ioctl del PCM ('A') en el nodo de control.
    assert!(matches!(
        ioctl_int(&dev, 0x0000_4110, 0),
        Err(FsError::NotSupported)
    ));
    // Y un número de la familia 'U' que no está implementado.
    assert!(matches!(
        ioctl_int(&dev, 0x0000_5577, 0),
        Err(FsError::NotSupported)
    ));
    // Una dirección que no es de usuario es EFAULT, no un panic.
    assert!(matches!(
        dev.io_control(CTL_VERSION, 0),
        Err(FsError::BadAddress)
    ));
}

/// `CARD_INFO` es lo que `aplay -l` imprime, y sale LIMPIO: el nodo
/// pone a cero la estructura antes de rellenarla, así que lo que el
/// cliente tuviera dentro no se cuela en un nombre.
#[test]
fn the_card_info_identifies_the_card_and_comes_out_clean() {
    let audio = FakeGain::new();
    let dev = CtlDev::new(audio, 2);
    let mut info: SndCtlCardInfo = zeroed();
    // Basura del cliente en todos los campos.
    info.card = -7;
    info.id = [b'z'; 16];
    info.name = [b'z'; 32];
    info.components = [b'z'; 128];
    dev.io_control(CTL_CARD_INFO, &mut info as *mut SndCtlCardInfo as usize)
        .unwrap();
    assert_eq!(info.card, 2);
    assert_eq!(cstr_name_of(&info.id), "EclipseHDA2");
    assert_eq!(cstr_name_of(&info.driver), "eclipse-hda");
    assert_eq!(cstr_name_of(&info.name), "HDMI/DP pin 3");
    assert_eq!(cstr_name_of(&info.longname), "HDMI/DP pin 3");
    assert_eq!(cstr_name_of(&info.mixername), "Eclipse Mixer");
    assert_eq!(cstr_name_of(&info.components), "");
    // Y sin basura detrás de ningún NUL.
    assert!(info.components.iter().all(|&b| b == 0));
    assert!(info.id[11..].iter().all(|&b| b == 0));
}

fn cstr_name_of(buf: &[u8]) -> &str {
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// `ELEM_LIST` anuncia los dos elementos de la tarjeta y escribe
/// exactamente los que caben desde el desplazamiento que se le pide:
/// `alsamixer` lo llama primero con `space = 0` para saber cuántos hay
/// y luego con el sitio justo.
#[test]
fn the_element_list_counts_two_and_writes_what_fits_from_the_offset() {
    let (_, dev) = ctl();

    fn list(
        dev: &CtlDev,
        ids: &mut [SndCtlElemId],
        offset: u32,
        space: u32,
        with_buffer: bool,
    ) -> (u32, u32) {
        let mut l: SndCtlElemList = zeroed();
        l.offset = offset;
        l.space = space;
        l.pids = if with_buffer {
            ids.as_mut_ptr() as u64
        } else {
            0
        };
        dev.io_control(CTL_ELEM_LIST, &mut l as *mut SndCtlElemList as usize)
            .unwrap();
        (l.count, l.used)
    }

    let mut ids: [SndCtlElemId; 4] = zeroed();
    // Sólo contar: dos elementos, ninguno escrito.
    assert_eq!(list(&dev, &mut ids, 0, 0, true), (2, 0));
    assert_eq!(list(&dev, &mut ids, 0, 0, false), (2, 0));
    assert!(ids.iter().all(|i| i.numid == 0));

    // Sitio para los dos: los dos.
    assert_eq!(list(&dev, &mut ids, 0, 4, true), (2, 2));
    assert_eq!(ids[0].numid, NUMID_VOLUME);
    assert_eq!(cstr_name(&ids[0].name), NAME_VOLUME);
    assert_eq!(ids[1].numid, NUMID_SWITCH);
    assert_eq!(cstr_name(&ids[1].name), NAME_SWITCH);
    // Y nada más allá de los dos que dijo haber escrito.
    assert_eq!(ids[2].numid, 0);

    // Sitio para uno: el primero.
    let mut one: [SndCtlElemId; 4] = zeroed();
    assert_eq!(list(&dev, &mut one, 0, 1, true), (2, 1));
    assert_eq!(one[0].numid, NUMID_VOLUME);
    assert_eq!(one[1].numid, 0);

    // Desde el segundo: el segundo y nada más.
    let mut from1: [SndCtlElemId; 4] = zeroed();
    assert_eq!(list(&dev, &mut from1, 1, 4, true), (2, 1));
    assert_eq!(from1[0].numid, NUMID_SWITCH);
    assert_eq!(from1[1].numid, 0);

    // Pasado el final: cero escritos, y sigue diciendo que hay dos.
    let mut past: [SndCtlElemId; 4] = zeroed();
    assert_eq!(list(&dev, &mut past, 2, 4, true), (2, 0));
    assert_eq!(list(&dev, &mut past, 9999, 4, true), (2, 0));
    // Un desplazamiento que desbordaría al sumarle el índice no envuelve.
    assert_eq!(list(&dev, &mut past, u32::MAX, 4, true), (2, 0));
    assert!(past.iter().all(|i| i.numid == 0));
}

/// El `pids` de `ELEM_LIST` es un SEGUNDO puntero de usuario que viaja
/// DENTRO de la estructura, así que validar la estructura no dice nada
/// de él: tiene que comprobarse aparte o el nodo escribe ids de
/// elemento donde le digan, direcciones del kernel incluidas.
///
/// Lo que se ve desde aquí es su consecuencia observable: un puntero
/// nulo con sitio declarado es EFAULT (la mitad de usuario del `access_ok`
/// sólo acota sobre hierro). Y sólo se exige el sitio que de verdad se va
/// a escribir, así que un cliente que ofrece un hueco enorme —- alsa-lib
/// lo dimensiona por `count` —- no se lleva un EFAULT por el sitio que no
/// usa.
#[test]
fn the_second_user_pointer_inside_the_list_is_checked_on_its_own() {
    let (_, dev) = ctl();
    let mut l: SndCtlElemList = zeroed();
    l.space = 2;
    l.pids = 0;
    assert!(matches!(
        dev.io_control(CTL_ELEM_LIST, &mut l as *mut SndCtlElemList as usize),
        Err(FsError::BadAddress)
    ));
    // Sin sitio declarado no hay nada que escribir y el puntero no
    // importa: es la primera llamada de alsa-lib, la de contar.
    let mut l: SndCtlElemList = zeroed();
    l.space = 0;
    l.pids = 0;
    dev.io_control(CTL_ELEM_LIST, &mut l as *mut SndCtlElemList as usize)
        .unwrap();
    assert_eq!((l.count, l.used), (2, 0));
    // Lo mismo desde un desplazamiento pasado el final: no se escribe
    // nada, así que tampoco se exige el búfer.
    let mut l: SndCtlElemList = zeroed();
    l.space = 4;
    l.offset = 2;
    l.pids = 0;
    dev.io_control(CTL_ELEM_LIST, &mut l as *mut SndCtlElemList as usize)
        .unwrap();
    assert_eq!((l.count, l.used), (2, 0));
    // Y un hueco enorme con un búfer de verdad pasa.
    let mut ids: [SndCtlElemId; 2] = zeroed();
    let mut l: SndCtlElemList = zeroed();
    l.space = 1024;
    l.pids = ids.as_mut_ptr() as u64;
    dev.io_control(CTL_ELEM_LIST, &mut l as *mut SndCtlElemList as usize)
        .unwrap();
    assert_eq!(l.used, 2);
    assert_eq!(ids[1].numid, NUMID_SWITCH);
}

/// `ELEM_INFO` describe el volumen como un entero de 0 a 100 y el
/// interruptor como un booleano, los dos de dos canales y
/// lectura/escritura: es lo que hace que `alsamixer` dibuje una barra
/// estéreo con una M debajo.
#[test]
fn the_element_info_is_a_stereo_integer_and_a_stereo_boolean() {
    let (_, dev) = ctl();
    let info_of = |id: SndCtlElemId| {
        let mut info: SndCtlElemInfo = zeroed();
        info.id = id;
        // Basura del cliente, que tiene que desaparecer.
        info.count = 77;
        info.owner = 5;
        dev.io_control(CTL_ELEM_INFO, &mut info as *mut SndCtlElemInfo as usize)
            .map(|_| info)
    };

    let vol = info_of(id_of(MixerElem::Volume)).unwrap();
    assert_eq!(vol.type_, ELEM_INTEGER);
    assert_eq!((vol.min, vol.max, vol.step), (0, 100, 1));
    assert_eq!(vol.count, 2);
    assert_eq!(vol.access, ACCESS_READ_WRITE);
    assert_eq!(vol.owner, -1);
    assert_eq!(cstr_name(&vol.id.name), NAME_VOLUME);

    let sw = info_of(id_of(MixerElem::Switch)).unwrap();
    assert_eq!(sw.type_, ELEM_BOOLEAN);
    assert_eq!((sw.min, sw.max, sw.step), (0, 1, 1));
    assert_eq!(sw.count, 2);
    assert_eq!(cstr_name(&sw.id.name), NAME_SWITCH);

    // Por nombre, sin numid.
    let mut by_name: SndCtlElemId = zeroed();
    fill_cstr(&mut by_name.name, NAME_VOLUME);
    assert_eq!(info_of(by_name).unwrap().type_, ELEM_INTEGER);

    // Un elemento que no existe es ENOENT.
    let mut nope: SndCtlElemId = zeroed();
    nope.numid = 42;
    fill_cstr(&mut nope.name, "Capture Volume");
    assert!(matches!(info_of(nope), Err(FsError::EntryNotFound)));
}

/// `ELEM_READ` lee la ganancia del dispositivo, y el interruptor de
/// ALSA es «1 = suena»: al revés dejaría a `amixer` mostrando `[off]`
/// con el volumen a tope.
#[test]
fn reading_the_elements_gives_the_gain_and_an_unmuted_switch_is_one() {
    let (audio, dev) = ctl();
    let read = |id: SndCtlElemId| {
        let mut v: SndCtlElemValue = zeroed();
        v.id = id;
        dev.io_control(CTL_ELEM_READ, &mut v as *mut SndCtlElemValue as usize)
            .map(|_| v)
    };

    let v = read(id_of(MixerElem::Volume)).unwrap();
    assert_eq!((v.values[0], v.values[1]), (100, 100));
    // El id sale rellenado, que es lo que alsa-lib vuelve a leer.
    assert_eq!(cstr_name(&v.id.name), NAME_VOLUME);
    let v = read(id_of(MixerElem::Switch)).unwrap();
    assert_eq!((v.values[0], v.values[1]), (1, 1));

    // Medio silenciado y a volúmenes distintos.
    audio.set_gain(80, 30, false, true).unwrap();
    let v = read(id_of(MixerElem::Volume)).unwrap();
    assert_eq!((v.values[0], v.values[1]), (80, 30));
    let v = read(id_of(MixerElem::Switch)).unwrap();
    assert_eq!((v.values[0], v.values[1]), (1, 0));

    let mut nope: SndCtlElemId = zeroed();
    nope.numid = 42;
    assert!(matches!(read(nope), Err(FsError::EntryNotFound)));
}

/// `ELEM_WRITE` recorta a 0..100 y, sobre todo, NO se lleva por delante
/// lo que no escribe: escribir el volumen conserva el silenciado y
/// escribir el interruptor conserva el volumen. `amixer set Master
/// unmute` hace exactamente eso, y perder el volumen ahí es subir el
/// sonido a tope sin que nadie lo pida.
#[test]
fn writing_one_element_leaves_the_other_alone_and_clamps_the_range() {
    let (audio, dev) = ctl();
    let write = |elem: MixerElem, a: i64, b: i64| {
        let mut v: SndCtlElemValue = zeroed();
        v.id = id_of(elem);
        v.values = [a, b];
        dev.io_control(CTL_ELEM_WRITE, &mut v as *mut SndCtlElemValue as usize)
    };

    write(MixerElem::Volume, 70, 40).unwrap();
    assert_eq!(audio.gain(), (70, 40, false, false));
    // Silenciar no toca el volumen.
    write(MixerElem::Switch, 0, 0).unwrap();
    assert_eq!(audio.gain(), (70, 40, true, true));
    // Y volver a sonar tampoco.
    write(MixerElem::Switch, 1, 1).unwrap();
    assert_eq!(audio.gain(), (70, 40, false, false));
    // Un canal sí y el otro no.
    write(MixerElem::Switch, 1, 0).unwrap();
    assert_eq!(audio.gain(), (70, 40, false, true));
    // Cambiar el volumen conserva ese silenciado.
    write(MixerElem::Volume, 55, 55).unwrap();
    assert_eq!(audio.gain(), (55, 55, false, true));
    // Cualquier valor distinto de cero es «suena».
    write(MixerElem::Switch, 7, -1).unwrap();
    assert_eq!(audio.gain(), (55, 55, false, false));

    // Recorte: por encima de 100 y por debajo de 0.
    write(MixerElem::Volume, 1000, -1000).unwrap();
    assert_eq!(audio.gain(), (100, 0, false, false));
    write(MixerElem::Volume, i64::MAX, i64::MIN).unwrap();
    assert_eq!(audio.gain(), (100, 0, false, false));

    let mut v: SndCtlElemValue = zeroed();
    v.id.numid = 42;
    assert!(matches!(
        dev.io_control(CTL_ELEM_WRITE, &mut v as *mut SndCtlElemValue as usize),
        Err(FsError::EntryNotFound)
    ));

    // Un dispositivo que no acepta el cambio es EIO y no un `Ok`.
    let deaf = CtlDev::new(Arc::new(Deaf), 0);
    let mut v: SndCtlElemValue = zeroed();
    v.id = id_of(MixerElem::Volume);
    v.values = [50, 50];
    assert!(matches!(
        deaf.io_control(CTL_ELEM_WRITE, &mut v as *mut SndCtlElemValue as usize),
        Err(FsError::DeviceError)
    ));
}

/// El descubrimiento de dispositivos: un solo PCM de reproducción, el
/// 0. Que la captura y cualquier otro número contesten ENOENT es cómo
/// alsa-lib y PulseAudio descubren que esta tarjeta no graba — no es un
/// fallo.
#[test]
fn there_is_one_playback_pcm_and_no_capture() {
    let (_, dev) = ctl();
    // NEXT_DEVICE: el primero es el 0, y después del 0 no hay más.
    assert_eq!(ioctl_int(&dev, CTL_PCM_NEXT_DEVICE, -1).unwrap(), 0);
    assert_eq!(ioctl_int(&dev, CTL_PCM_NEXT_DEVICE, 0).unwrap(), -1);
    assert_eq!(ioctl_int(&dev, CTL_PCM_NEXT_DEVICE, 5).unwrap(), -1);
    // Ni hwdep ni MIDI.
    assert_eq!(ioctl_int(&dev, CTL_HWDEP_NEXT, -1).unwrap(), -1);
    assert_eq!(ioctl_int(&dev, CTL_RAWMIDI_NEXT, -1).unwrap(), -1);

    let info_for = |device: u32, stream: i32| {
        let mut info: SndPcmInfo = zeroed();
        info.device = device;
        info.stream = stream;
        info.subdevice = 0;
        info.card = -9;
        dev.io_control(CTL_PCM_INFO, &mut info as *mut SndPcmInfo as usize)
            .map(|_| info)
    };

    let info = info_for(0, 0).unwrap();
    assert_eq!(info.card, 0);
    assert_eq!(info.device, 0);
    assert_eq!(info.stream, 0);
    assert_eq!(cstr_name_of(&info.id), "Eclipse HDA");
    assert_eq!(cstr_name_of(&info.name), "HDMI/DP pin 3");
    assert_eq!(cstr_name_of(&info.subname), "subdevice #0");
    assert_eq!((info.subdevices_count, info.subdevices_avail), (1, 1));

    // Captura: ENOENT.
    assert!(matches!(info_for(0, 1), Err(FsError::EntryNotFound)));
    // Otro dispositivo: ENOENT.
    assert!(matches!(info_for(1, 0), Err(FsError::EntryNotFound)));
    assert!(matches!(info_for(99, 0), Err(FsError::EntryNotFound)));
}

/// Los ioctl que no hacen nada y que, si fallaran, tumbarían al
/// cliente: `alsamixer` aborta si `SUBSCRIBE_EVENTS` devuelve error, y
/// `TLV_READ` tiene que decir ENXIO (ningún control de aquí lleva
/// escala en dB) para que dibuje porcentajes en vez de decibelios.
#[test]
fn the_no_op_ioctls_answer_what_their_clients_need() {
    let (_, dev) = ctl();
    // Un negativo se normaliza a cero, y un positivo se queda.
    assert_eq!(ioctl_int(&dev, CTL_SUBSCRIBE_EVENTS, -1).unwrap(), 0);
    assert_eq!(ioctl_int(&dev, CTL_SUBSCRIBE_EVENTS, 1).unwrap(), 1);
    // Sin escala en dB.
    assert!(matches!(
        ioctl_int(&dev, CTL_TLV_READ, 0),
        Err(FsError::NoSuchDeviceOrAddress)
    ));
    // Cerrojos de elemento y preferencia de subdispositivo: aceptados.
    assert!(ioctl_int(&dev, CTL_ELEM_LOCK, 0).is_ok());
    assert!(ioctl_int(&dev, CTL_ELEM_UNLOCK, 0).is_ok());
    assert!(ioctl_int(&dev, CTL_PCM_PREFER_SUBDEVICE, 0).is_ok());
    assert!(ioctl_int(&dev, CTL_POWER, 0).is_ok());
    // Siempre encendida (D0).
    assert_eq!(ioctl_int(&dev, CTL_POWER_STATE, 7).unwrap(), 0);
}

/// El nodo de control no se lee ni se escribe, su `poll` no señala
/// nada (no hay cola de eventos todavía) y su número de dispositivo es
/// el de ALSA: mayor 116, menor `tarjeta * 32`.
#[test]
fn the_node_is_ioctl_only_and_numbered_like_alsa() {
    let (_, dev) = ctl();
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
    assert_eq!(m.rdev, make_rdev(116, 0));
    for card in [1usize, 2, 7] {
        let dev = CtlDev::new(FakeGain::new(), card);
        assert_eq!(dev.metadata().unwrap().rdev, make_rdev(116, card * 32));
    }
    // Y cada nodo tiene su propio inodo.
    let (_, other) = ctl();
    assert_ne!(
        dev.metadata().unwrap().inode,
        other.metadata().unwrap().inode
    );
}
