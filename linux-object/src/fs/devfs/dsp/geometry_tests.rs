//! Los tests de la geometría de fragmentos de `/dev/dsp` y de la
//! aritmética de bytes de cliente a bytes de anillo: `geometry`,
//! `client_frame`/`client_bytes`/`hw_bytes`, `drain_secs`, `odelay` y
//! `room`.
//!
//! Lo que ya había en `dsp.rs` prueba la geometría POR SU RESULTADO en
//! el caso por defecto y en una petición de SDL (4 KiB x 2). Esto prueba
//! el algoritmo: un barrido por tamaños de anillo, formatos, canales,
//! frecuencias y las tres peticiones del cliente, exigiendo las
//! invariantes de las que dependen `GETBLKSIZE`, `GETOSPACE` y el límite
//! de encolado, más los extremos que ningún cliente real pide y que son
//! los que desbordan o dividen por cero.

use super::*;
use zcore_drivers::DeviceResult;

/// Un dispositivo del que sólo importa el tamaño del anillo y la
/// frecuencia que dice tener: la geometría no mira nada más.
struct Ring {
    cap: usize,
    rate: u32,
    channels: u8,
    queued: Mutex<usize>,
    delay: Mutex<usize>,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Ring {
            cap,
            rate: 48_000,
            channels: 2,
            queued: Mutex::new(0),
            delay: Mutex::new(0),
        }
    }

    fn at(cap: usize, rate: u32, channels: u8) -> Self {
        Ring {
            rate,
            channels,
            ..Ring::new(cap)
        }
    }
}

impl zcore_drivers::scheme::Scheme for Ring {
    fn name(&self) -> &str {
        "ring"
    }
}

impl AudioScheme for Ring {
    fn set_params(&self, _rate: u32, _channels: u8) -> DeviceResult<(u32, u8)> {
        Ok((self.rate, self.channels))
    }
    fn params(&self) -> (u32, u8) {
        (self.rate, self.channels)
    }
    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        *self.queued.lock() += pcm.len();
        Ok(pcm.len())
    }
    fn free_bytes(&self) -> usize {
        self.cap - *self.queued.lock()
    }
    fn buffer_bytes(&self) -> usize {
        self.cap
    }
    fn queued_bytes(&self) -> usize {
        *self.queued.lock()
    }
    fn delay_bytes(&self) -> usize {
        (*self.delay.lock()).max(*self.queued.lock())
    }
    fn reset(&self) -> DeviceResult {
        *self.queued.lock() = 0;
        Ok(())
    }
}

fn dev(audio: Ring) -> DspDev {
    DspDev::new(Arc::new(audio), 0)
}

fn params(format: i32, channels: usize) -> OssParams {
    OssParams {
        format,
        channels,
        rate: 48_000,
        fragshift: 0,
        maxfrags: 0,
        subdivision: 0,
    }
}

/// El anillo de la HDA, el que ven los tests de `dsp.rs`.
const HDA_RING: usize = 65536 - 16384;

/// Las invariantes de las que dependen `GETBLKSIZE`, `GETOSPACE` y el
/// tope de encolado, sobre todo el espacio de parámetros que un cliente
/// puede pedir:
///
/// * el fragmento es un número entero de marcos de cliente, o
///   `GETBLKSIZE` daría un tamaño que ninguna escritura puede cumplir
///   exactamente y el cliente se quedaría con un marco a medias en cada
///   fragmento;
/// * hay al menos dos fragmentos, que es lo que hace que haya un
///   fragmento que llenar mientras suena el otro;
/// * el búfer entero cabe en el anillo, o `GETOSPACE` prometería un
///   sitio que `write` luego no tiene (es el fallo que en ALSA aborta el
///   sumidero de PulseAudio);
/// * y si hay más de dos fragmentos, el búfer llega al mínimo del
///   esclavo, los 512 marcos que el `hw_params` nativo también exige.
#[test]
fn the_geometry_is_whole_frames_at_least_two_fragments_and_fits_the_ring() {
    for ring in [4096, 8192, 16384, 32768, HDA_RING, 65536, 1 << 20] {
        for format in [
            AFMT_MU_LAW,
            AFMT_U8,
            AFMT_S16_LE,
            AFMT_S24_PACKED,
            AFMT_S32_LE,
        ] {
            for channels in [1usize, 2, 4, 6] {
                for rate in [8_000u32, 22_050, 48_000, 192_000] {
                    for fragshift in [0u32, 4, 8, 10, 12, 16, 24, 30] {
                        for maxfrags in [0usize, 2, 3, 8, 255] {
                            for subdivision in [0usize, 1, 2, 4, 16] {
                                let d = dev(Ring::new(ring));
                                let p = OssParams {
                                    fragshift,
                                    maxfrags,
                                    subdivision,
                                    ..params(format, channels)
                                };
                                let (period, periods) = d.geometry(&p, rate);
                                let cf = sample_bytes(format).unwrap() * channels;
                                let what = alloc::format!(
                                    "anillo {} fmt {:#x} {}ch {} Hz frag {} max {} sd {} -> {} x {}",
                                    ring, format, channels, rate, fragshift, maxfrags,
                                    subdivision, period, periods
                                );
                                assert_eq!(period % cf, 0, "fragmento a medio marco: {}", what);
                                assert!(period >= 16, "fragmento ridiculo: {}", what);
                                assert!(periods >= 2, "un solo fragmento: {}", what);
                                let buffer_frames = period * periods / cf;
                                assert!(
                                    buffer_frames <= ring / HW_FRAME,
                                    "el buffer no cabe en el anillo: {}",
                                    what
                                );
                                if periods > 2 {
                                    assert!(
                                        buffer_frames >= MIN_BUFFER_FRAMES,
                                        "buffer por debajo del minimo del esclavo: {}",
                                        what
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Un anillo que todavía no sabe su tamaño (un dispositivo sin
/// configurar contesta 0) no divide por cero ni sale con un fragmento de
/// cero bytes, que convertiría el primer `GETOSPACE` en una división por
/// cero en el cliente.
#[test]
fn a_ring_of_no_bytes_still_gives_a_usable_geometry() {
    for channels in [1usize, 2, 6] {
        let d = dev(Ring::new(0));
        let (period, periods) = d.geometry(&params(AFMT_S16_LE, channels), 48_000);
        assert!(period >= 16, "{} canales: {}", channels, period);
        assert_eq!(period % (2 * channels), 0);
        assert!(periods >= 2);
    }
}

/// `SETFRAGMENT` manda: el fragmento es exactamente la potencia de dos
/// que se pide, mientras quepa en la mitad del búfer y no baje del
/// mínimo del esclavo.
#[test]
fn setfragment_is_honoured_exactly_between_the_two_bounds() {
    let d = dev(Ring::new(HDA_RING));
    // S16LE estéreo: el búfer son 32 KiB, así que la mitad son 16 KiB y
    // el mínimo 128 marcos x 4 B = 512 B.
    for shift in 9..=14u32 {
        let p = OssParams {
            fragshift: shift,
            ..params(AFMT_S16_LE, 2)
        };
        let (period, _) = d.geometry(&p, 48_000);
        assert_eq!(period, 1 << shift, "shift {}", shift);
    }
    // Por debajo del mínimo sube al mínimo.
    for shift in 1..9u32 {
        let p = OssParams {
            fragshift: shift,
            ..params(AFMT_S16_LE, 2)
        };
        assert_eq!(d.geometry(&p, 48_000).0, 512, "shift {}", shift);
    }
    // Y por encima de la mitad del búfer se queda en la mitad del
    // búfer, también cuando el desplazamiento es absurdo: `1 << 30`
    // desbordaría un `usize` de 32 bits, así que el tope de 24 que pone
    // el código tiene que aguantar cualquier número.
    for shift in [15u32, 16, 20, 24, 25, 31, u32::MAX] {
        let p = OssParams {
            fragshift: shift,
            ..params(AFMT_S16_LE, 2)
        };
        assert_eq!(d.geometry(&p, 48_000).0, 16384, "shift {}", shift);
    }
}

/// El número de fragmentos que pide `SETFRAGMENT` en su palabra alta es
/// un TOPE, y el mínimo de búfer del esclavo le pasa por encima: con
/// fragmentos pequeños salen más fragmentos de los que se pidieron, que
/// es lo contrario de prometer menos latencia de la que se puede dar.
#[test]
fn maxfrags_caps_the_count_unless_the_slave_minimum_needs_more() {
    let d = dev(Ring::new(HDA_RING));
    let with = |maxfrags, fragshift| {
        d.geometry(
            &OssParams {
                maxfrags,
                fragshift,
                ..params(AFMT_S16_LE, 2)
            },
            48_000,
        )
    };
    // Sin tope, el búfer entero: 32 KiB en fragmentos de 4 KiB.
    assert_eq!(with(0, 0), (4096, 8));
    // Con tope, el tope.
    assert_eq!(with(2, 0), (4096, 2));
    assert_eq!(with(4, 0), (4096, 4));
    // Un tope de uno no existe: siempre hay dos.
    assert_eq!(with(1, 0), (4096, 2));
    // Y con el fragmento en el mínimo del esclavo (512 B = 128 marcos),
    // tres fragmentos no llegan a los 512 marcos de búfer, así que salen
    // cuatro aunque se pidieran tres.
    assert_eq!(with(3, 9), (512, 4));
    assert_eq!(with(3, 0).1, 3);
}

/// `SUBDIVIDE` divide el fragmento que el algoritmo habría elegido, y
/// sigue sin bajar del mínimo del esclavo.
#[test]
fn subdivide_divides_the_period_down_to_the_slave_minimum() {
    let d = dev(Ring::new(HDA_RING));
    let with = |subdivision| {
        d.geometry(
            &OssParams {
                subdivision,
                ..params(AFMT_S16_LE, 2)
            },
            48_000,
        )
        .0
    };
    // Sin pedir nada, el algoritmo de Linux elige 4 KiB (16 KiB bajo un
    // segundo de audio, y la heurística del cuarto lo deja ahí).
    assert_eq!(with(0), 4096);
    assert_eq!(with(1), 16384);
    assert_eq!(with(2), 8192);
    assert_eq!(with(4), 4096);
    assert_eq!(with(8), 2048);
    assert_eq!(with(32), 512);
    // El mínimo del esclavo es el suelo, por mucho que se subdivida.
    assert_eq!(with(64), 512);
    assert_eq!(with(4096), 512);
    assert_eq!(with(usize::MAX), 512);
}

/// El fragmento se elige contra la frecuencia: el algoritmo lo baja
/// mientras pase de un segundo de audio, así que a 8 kHz mono sale un
/// fragmento más corto en bytes que a 192 kHz 6 canales, y nunca más de
/// la mitad del búfer.
#[test]
fn the_period_follows_the_rate_and_never_passes_half_the_buffer() {
    let d = dev(Ring::new(1 << 20));
    let mut last = 0;
    for rate in [8_000u32, 22_050, 48_000, 96_000, 192_000] {
        let (period, periods) = d.geometry(&params(AFMT_S16_LE, 2), rate);
        assert!(period >= last, "{} Hz dio {} tras {}", rate, period, last);
        last = period;
        assert!(period * periods <= 1 << 20);
        assert!(period <= (1 << 20) / 2);
    }
}

/// La aritmética de bytes de cliente a bytes de anillo redondea SIEMPRE
/// hacia abajo a un marco entero, en los dos sentidos: medio marco de
/// cliente no es medio marco de anillo, es ninguno.
#[test]
fn the_byte_arithmetic_floors_to_whole_frames_both_ways() {
    for (format, channels, cf) in [
        (AFMT_MU_LAW, 1, 1),
        (AFMT_U8, 2, 2),
        (AFMT_S16_LE, 1, 2),
        (AFMT_S16_LE, 2, 4),
        (AFMT_S24_PACKED, 2, 6),
        (AFMT_S32_LE, 6, 24),
    ] {
        let rt = OssRuntime::new(params(format, channels));
        assert_eq!(rt.client_frame(), cf, "{:#x} {}ch", format, channels);
        for frames in [0usize, 1, 3, 97] {
            assert_eq!(rt.client_bytes(frames * HW_FRAME), frames * cf);
            assert_eq!(rt.hw_bytes(frames * cf), frames * HW_FRAME);
            // Un byte de más no es un marco de más.
            assert_eq!(rt.client_bytes(frames * HW_FRAME + 1), frames * cf);
            if cf > 1 {
                assert_eq!(rt.hw_bytes(frames * cf + 1), frames * HW_FRAME);
            }
        }
        // Y la vuelta nunca promete más de lo que había.
        for bytes in [0usize, 1, 5, 17, 4096, 4097] {
            assert!(rt.client_bytes(rt.hw_bytes(bytes)) <= bytes);
        }
    }
    // El formato desconocido que `SETFMT` no instala se cuenta como de
    // dos bytes, igual que en la conversión.
    let rt = OssRuntime::new(params(0x0000_0400, 2));
    assert_eq!(rt.client_frame(), 4);
}

/// `buffer_bytes` es el búfer negociado, y un runtime recién abierto
/// todavía no tiene ninguno: la geometría se calcula en el primer
/// `make_ready`, no al abrir.
#[test]
fn a_fresh_runtime_has_no_buffer_until_it_is_made_ready() {
    let rt = OssRuntime::new(OssDefaults::Dsp.params());
    assert_eq!(rt.buffer_bytes(), 0);
    assert!(rt.params_dirty);
    assert!(rt.prepare);
    assert!(rt.trigger);
    assert!(!rt.nonblock);
    assert_eq!(rt.partial_len, 0);
    assert_eq!(rt.bytes, 0);
    // Y el de `/dev/audio` arranca con las convenciones de Sun.
    let sun = OssRuntime::new(OssDefaults::Audio.params());
    assert_eq!(sun.params.format, AFMT_MU_LAW);
    assert_eq!(sun.params.channels, 1);
    assert_eq!(sun.params.rate, 8_000);
    assert_eq!(sun.client_frame(), 1);
    assert_eq!(sun.hw_rate, 8_000);
}

/// La cota de espera de un vaciado es el tiempo que el dispositivo tarda
/// en sonar lo que tiene, redondeado hacia arriba y con un segundo de
/// más: una cota que redondeara hacia abajo cortaría el final del
/// audio, que es lo que esto protege.
#[test]
fn the_drain_bound_rounds_up_and_adds_a_second() {
    let d = dev(Ring::at(HDA_RING, 48_000, 2));
    // 48 kHz estéreo S16 son 192.000 B/s.
    assert_eq!(d.drain_secs(0), 1);
    assert_eq!(d.drain_secs(1), 2);
    assert_eq!(d.drain_secs(192_000), 2);
    assert_eq!(d.drain_secs(192_001), 3);
    assert_eq!(d.drain_secs(10 * 192_000), 11);
    // A 8 kHz mono el mismo número de bytes tarda doce veces más.
    let slow = dev(Ring::at(HDA_RING, 8_000, 1));
    assert_eq!(slow.drain_secs(16_000), 2);
    assert_eq!(slow.drain_secs(192_000), 13);
    // Un dispositivo que todavía no tiene frecuencia no divide por cero.
    let unset = dev(Ring::at(HDA_RING, 0, 0));
    assert_eq!(unset.drain_secs(0), 1);
    assert!(unset.drain_secs(1024) >= 2);
}

/// `GETODELAY` cuenta lo que el enlace tiene todavía por sonar MÁS el
/// marco a medias que el nodo arrastra: un cliente que sincroniza vídeo
/// con eso no puede quedarse con un marco de desfase permanente.
#[test]
fn the_odelay_counts_the_link_delay_and_the_carried_partial() {
    let audio = Arc::new(Ring::new(HDA_RING));
    let d = DspDev::new(audio.clone(), 0);
    let mut rt = OssRuntime::new(OssDefaults::Dsp.params());
    assert_eq!(d.odelay(&rt), 0);
    // 100 marcos en el anillo son 100 marcos de cliente (S16LE estéreo).
    *audio.queued.lock() = 100 * HW_FRAME;
    assert_eq!(d.odelay(&rt), 100 * 4);
    // El silencio que el dispositivo metió por delante cuenta también:
    // es lo que `snd_pcm_oss_get_odelay` suma.
    *audio.delay.lock() = 150 * HW_FRAME;
    assert_eq!(d.odelay(&rt), 150 * 4);
    // Y el marco a medias, en bytes de cliente.
    rt.partial_len = 3;
    assert_eq!(d.odelay(&rt), 150 * 4 + 3);
    // A µ-law mono los mismos bytes de anillo son la cuarta parte de
    // bytes de cliente.
    let mut sun = OssRuntime::new(OssDefaults::Audio.params());
    sun.partial_len = 0;
    assert_eq!(d.odelay(&sun), 150);
}

/// El sitio que `GETOSPACE` y una escritura no bloqueante ven es el
/// mínimo entre lo que queda del búfer negociado y lo que queda del
/// anillo, en marcos enteros: nunca más de lo que el dispositivo puede
/// tomar, nunca más del búfer que el cliente pidió.
#[test]
fn the_room_is_the_smaller_of_the_negotiated_buffer_and_the_ring() {
    let audio = Arc::new(Ring::new(HDA_RING));
    let d = DspDev::new(audio.clone(), 0);
    let mut rt = OssRuntime::new(OssDefaults::Dsp.params());
    // Un búfer negociado de 8 KiB de cliente sobre un anillo de 48 KiB.
    rt.period_bytes = 4096;
    rt.periods = 2;
    assert_eq!(rt.buffer_bytes(), 8192);
    assert_eq!(d.room(&rt), 8192);
    // Lo encolado se descuenta del búfer, no del anillo.
    *audio.queued.lock() = 4096;
    assert_eq!(d.room(&rt), 4096);
    *audio.queued.lock() = 8192;
    assert_eq!(d.room(&rt), 0);
    // Pasado el búfer no hay sitio negativo.
    *audio.queued.lock() = 20_000;
    assert_eq!(d.room(&rt), 0);
    // Y con un búfer más grande que el anillo manda el anillo.
    rt.period_bytes = HDA_RING;
    rt.periods = 4;
    *audio.queued.lock() = HDA_RING - 100;
    assert_eq!(d.room(&rt), 100 / HW_FRAME * 4);
}
