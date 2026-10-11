//! Los tests del refine de `hw_params` del PCM nativo: los intervalos
//! (`iv_normalize`, `iv_empty`, `iv_clamp`, `iv_bounds`, `iv_name`), el
//! barrido de propagación, y las dos etapas de recuperación (`reopen`,
//! `relax_derived_params`, `relax_size_params`).
//!
//! `snd.rs` sólo tenía pruebas de los dos casos que rompieron
//! PulseAudio en hardware (la capacidad del anillo para la frecuencia
//! que se va a poner). El resto de la negociación — las nueve
//! relaciones propagadas en los dos sentidos hasta el punto fijo — no
//! tenía ninguna, y es donde viven los fallos que el propio fichero
//! documenta: un redondeo asimétrico vacía el intervalo y el
//! `hw_params` entero sale con EINVAL.
//!
//! Así que cada caso concreto que los comentarios de `propagate`
//! nombran como «esto es lo que rechazaba X» tiene aquí su prueba, más
//! las invariantes del barrido: que sólo estreche, que llegue a un
//! punto fijo antes de las ocho vueltas, y que lo que acepta contenga
//! lo que el cliente pidió.

use super::*;
use zcore_drivers::DeviceResult;

/// Un anillo de capacidad fija, en marcos.
struct Ring(usize);

impl zcore_drivers::scheme::Scheme for Ring {
    fn name(&self) -> &str {
        "ring"
    }
}

impl AudioScheme for Ring {
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
        self.0 * BYTES_PER_FRAME as usize
    }
    fn buffer_bytes(&self) -> usize {
        self.0 * BYTES_PER_FRAME as usize
    }
    fn queued_bytes(&self) -> usize {
        0
    }
    fn reset(&self) -> DeviceResult {
        Ok(())
    }
}

/// El anillo de la HDA a 48 kHz: 683 ms.
const RING_FRAMES: usize = 32768;

fn pcm() -> PcmDev {
    PcmDev::new(Arc::new(Ring(RING_FRAMES)), 0)
}

/// Una petición que no pide nada: todo abierto, todas las máscaras a
/// uno. Es con lo que alsa-lib arranca su `snd_pcm_hw_params_any`.
fn any() -> SndPcmHwParams {
    let mut p: SndPcmHwParams = unsafe { core::mem::zeroed() };
    for m in p.masks.iter_mut() {
        m.bits = [!0; 8];
    }
    for iv in p.intervals.iter_mut() {
        iv.min = 0;
        iv.max = u32::MAX;
    }
    p.rmask = !0;
    p
}

fn pin(p: &mut SndPcmHwParams, idx: usize, value: u32) {
    p.intervals[idx] = SndInterval {
        min: value,
        max: value,
        flags: 0,
    };
}

fn iv(p: &SndPcmHwParams, idx: usize) -> (u32, u32) {
    (p.intervals[idx].min, p.intervals[idx].max)
}

// ── Los intervalos ──────────────────────────────────────────────────

/// Todo lo que se negocia aquí es entero, así que un límite abierto se
/// cierra al entero de dentro antes de propagar nada: es la
/// normalización que hace `snd_interval_refine` con un intervalo
/// INTEGER, y sin ella un `period_size > 128` y un `period_size >= 129`
/// darían resultados distintos.
#[test]
fn an_open_bound_is_folded_into_the_integer_inside_it() {
    let mut i = SndInterval {
        min: 128,
        max: 1024,
        flags: INTERVAL_OPENMIN | INTERVAL_OPENMAX,
    };
    PcmDev::iv_normalize(&mut i);
    assert_eq!((i.min, i.max), (129, 1023));
    assert_eq!(i.flags & (INTERVAL_OPENMIN | INTERVAL_OPENMAX), 0);
    assert_ne!(i.flags & INTERVAL_INTEGER, 0);
    assert!(!PcmDev::iv_empty(&i));

    // Un intervalo cerrado no se mueve, y de todos modos sale marcado
    // como entero.
    let mut i = SndInterval {
        min: 7,
        max: 7,
        flags: 0,
    };
    PcmDev::iv_normalize(&mut i);
    assert_eq!((i.min, i.max), (7, 7));
    assert_ne!(i.flags & INTERVAL_INTEGER, 0);

    // Un intervalo de un solo valor con los dos límites abiertos no
    // tiene ningún entero dentro: queda vacío, no del revés.
    let mut i = SndInterval {
        min: 5,
        max: 5,
        flags: INTERVAL_OPENMIN | INTERVAL_OPENMAX,
    };
    PcmDev::iv_normalize(&mut i);
    assert!(PcmDev::iv_empty(&i));
    assert_ne!(i.flags & INTERVAL_EMPTY, 0);

    // Y los extremos SATURAN en vez de envolver: un `> u32::MAX` o un
    // `< 0` se queda en el valor del borde, no en el otro extremo. Un
    // `wrapping_add` dejaria `min = 0`, o sea el intervalo abierto de
    // par en par, que es exactamente lo contrario de lo que se pidio;
    // por eso esto no sale vacio, sale pegado al borde.
    let mut i = SndInterval {
        min: u32::MAX,
        max: u32::MAX,
        flags: INTERVAL_OPENMIN,
    };
    PcmDev::iv_normalize(&mut i);
    assert_eq!((i.min, i.max), (u32::MAX, u32::MAX));
    let mut i = SndInterval {
        min: 0,
        max: 0,
        flags: INTERVAL_OPENMAX,
    };
    PcmDev::iv_normalize(&mut i);
    assert_eq!((i.min, i.max), (0, 0));
}

/// Un intervalo está vacío por la bandera o por los números, y las dos
/// cosas cuentan: `refine` decide el EINVAL con esto.
#[test]
fn an_interval_is_empty_by_its_flag_or_by_its_numbers() {
    let full = SndInterval {
        min: 0,
        max: u32::MAX,
        flags: 0,
    };
    assert!(!PcmDev::iv_empty(&full));
    assert!(PcmDev::iv_empty(&SndInterval {
        flags: INTERVAL_EMPTY,
        ..full
    }));
    assert!(PcmDev::iv_empty(&SndInterval {
        min: 10,
        max: 9,
        flags: 0
    }));
    assert!(!PcmDev::iv_empty(&SndInterval {
        min: 9,
        max: 9,
        flags: 0
    }));
}

/// `iv_clamp` intersecta y dice si algo se estrechó — que es lo que le
/// permite al barrido saber que ha llegado al punto fijo — y nunca
/// ensancha.
#[test]
fn a_clamp_only_narrows_and_says_whether_it_did() {
    let mut i = SndInterval {
        min: 100,
        max: 200,
        flags: 0,
    };
    // Un tope más ancho por los dos lados no cambia nada.
    assert!(!PcmDev::iv_clamp(&mut i, 0, 1000));
    assert_eq!((i.min, i.max), (100, 200));
    // Estrechar por abajo, por arriba, y por los dos.
    assert!(PcmDev::iv_clamp(&mut i, 150, 1000));
    assert_eq!((i.min, i.max), (150, 200));
    assert!(PcmDev::iv_clamp(&mut i, 0, 180));
    assert_eq!((i.min, i.max), (150, 180));
    assert!(!PcmDev::iv_clamp(&mut i, 150, 180));
    // Una intersección vacía marca la bandera.
    assert!(PcmDev::iv_clamp(&mut i, 190, 1000));
    assert!(PcmDev::iv_empty(&i));
}

/// Los límites de las relaciones se calculan en 64 bits (un
/// `buffer_size` de `u32::MAX` por cuatro no cabe en 32), así que el
/// tope se recorta a `u32::MAX` en vez de envolver.
#[test]
fn a_clamp_past_the_32_bit_range_saturates_instead_of_wrapping() {
    let mut i = SndInterval {
        min: 0,
        max: u32::MAX,
        flags: 0,
    };
    assert!(!PcmDev::iv_clamp(&mut i, 0, u64::MAX));
    assert_eq!((i.min, i.max), (0, u32::MAX));
    assert!(!PcmDev::iv_clamp(&mut i, 0, u32::MAX as u64 + 1000));
    assert_eq!(i.max, u32::MAX);
    // Y un suelo imposible deja el intervalo vacío, no en el cero.
    let mut i = SndInterval {
        min: 0,
        max: 1000,
        flags: 0,
    };
    assert!(PcmDev::iv_clamp(&mut i, u64::MAX, u64::MAX));
    assert_eq!(i.min, u32::MAX);
    assert!(PcmDev::iv_empty(&i));
    assert_eq!(PcmDev::iv_bounds(&i), (u32::MAX as u64, 1000));
}

/// Cada índice tiene su nombre: es lo único que ve quien lee el
/// `klog` de un `hw_params` rechazado, y un nombre desplazado señalaría
/// al parámetro de al lado.
#[test]
fn every_interval_index_has_its_own_name() {
    let names = [
        (PcmDev::IV_SAMPLE_BITS, "sample_bits"),
        (PcmDev::IV_FRAME_BITS, "frame_bits"),
        (PcmDev::IV_CHANNELS, "channels"),
        (PcmDev::IV_RATE, "rate"),
        (PcmDev::IV_PERIOD_TIME, "period_time"),
        (PcmDev::IV_PERIOD_SIZE, "period_size"),
        (PcmDev::IV_PERIOD_BYTES, "period_bytes"),
        (PcmDev::IV_PERIODS, "periods"),
        (PcmDev::IV_BUFFER_TIME, "buffer_time"),
        (PcmDev::IV_BUFFER_SIZE, "buffer_size"),
        (PcmDev::IV_BUFFER_BYTES, "buffer_bytes"),
        (PcmDev::IV_TICK_TIME, "tick_time"),
    ];
    // Los doce índices del array, cada uno una vez.
    assert_eq!(names.len(), 12);
    for (i, (idx, name)) in names.iter().enumerate() {
        assert_eq!(*idx, i, "{} no esta en su sitio", name);
        assert_eq!(PcmDev::iv_name(*idx), *name);
    }
    assert_eq!(PcmDev::iv_name(12), "?");
    assert_eq!(PcmDev::iv_name(usize::MAX), "?");
}

// ── El barrido ──────────────────────────────────────────────────────

/// Lo que este camino no negocia: S16LE estéreo y nada más. Un cliente
/// que pida mono o S24 tiene que llevarse un EINVAL, porque contestarle
/// `Ok` y luego tocar sus bytes como estéreo S16LE es audio equivocado
/// dado por bueno.
#[test]
fn the_format_is_s16le_stereo_and_anything_else_is_refused() {
    let d = pcm();
    let mut p = any();
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_SAMPLE_BITS), (16, 16));
    assert_eq!(iv(&p, PcmDev::IV_FRAME_BITS), (32, 32));
    assert_eq!(iv(&p, PcmDev::IV_CHANNELS), (2, 2));
    // Las máscaras salen con un bit cada una, y los huecos de más allá
    // de dos palabras a cero (un cliente los deja a uno con `any`).
    assert_eq!(p.masks[PAR_ACCESS].bits[0], 1 << ACCESS_RW_INTERLEAVED);
    assert_eq!(p.masks[PAR_FORMAT].bits[0], 1 << FORMAT_S16_LE);
    assert_eq!(p.masks[PAR_FORMAT].bits[1], 0);
    assert_eq!(p.masks[PAR_SUBFORMAT].bits[0], 1 << SUBFORMAT_STD);
    for m in &p.masks {
        assert!(m.bits[2..].iter().all(|&w| w == 0));
    }
    // Y lo que el cliente lee de vuelta.
    assert_eq!(p.info, INFO_PAUSE | INFO_INTERLEAVED | INFO_BLOCK_TRANSFER);
    assert_eq!(p.msbits, 16);
    assert_eq!((p.rate_num, p.rate_den), (0, 1));
    assert_eq!(p.fifo_size, 0);
    assert_eq!(p.cmask, 0x000f_ff07);

    // Mono.
    let mut p = any();
    pin(&mut p, PcmDev::IV_CHANNELS, 1);
    assert!(!d.refine(&mut p));
    // Seis canales.
    let mut p = any();
    pin(&mut p, PcmDev::IV_CHANNELS, 6);
    assert!(!d.refine(&mut p));
    // Un formato que no es S16LE.
    let mut p = any();
    p.masks[PAR_FORMAT].bits[0] = 1 << 10;
    assert!(!d.refine(&mut p));
    // Acceso mmap.
    let mut p = any();
    p.masks[PAR_ACCESS].bits[0] = 1;
    assert!(!d.refine(&mut p));
    // Y 24 bits por muestra.
    let mut p = any();
    pin(&mut p, PcmDev::IV_SAMPLE_BITS, 24);
    assert!(!d.refine(&mut p));
}

/// La frecuencia se pega al conjunto discreto que el formato de flujo
/// de la HDA codifica: el suelo sube a la primera soportada y el techo
/// baja a la última.
#[test]
fn the_rate_snaps_to_the_set_the_hardware_encodes() {
    let d = pcm();
    for &rate in RATES.iter() {
        let mut p = any();
        pin(&mut p, PcmDev::IV_RATE, rate);
        assert!(d.refine(&mut p), "{} Hz rechazada", rate);
        assert_eq!(iv(&p, PcmDev::IV_RATE), (rate, rate));
    }
    // Un intervalo que cubre el conjunto se pega a sus extremos.
    let mut p = any();
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_RATE), (8_000, 192_000));

    // Uno que empieza y acaba entre dos soportadas se pega hacia
    // dentro: 12000..40000 son 16000..32000.
    let mut p = any();
    p.intervals[PcmDev::IV_RATE] = SndInterval {
        min: 12_000,
        max: 40_000,
        flags: 0,
    };
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_RATE), (16_000, 32_000));

    // Y uno que cae ENTERO en un hueco no tiene ninguna frecuencia
    // dentro: EINVAL, que es por lo que los `*_near` de alsa-lib van
    // probando.
    for (lo, hi) in [(8_001u32, 11_024u32), (48_001, 88_199), (192_001, 300_000)] {
        let mut p = any();
        p.intervals[PcmDev::IV_RATE] = SndInterval {
            min: lo,
            max: hi,
            flags: 0,
        };
        assert!(!d.refine(&mut p), "{}..{} aceptada", lo, hi);
    }
    // Por debajo de la más baja, sube a la más baja.
    let mut p = any();
    p.intervals[PcmDev::IV_RATE] = SndInterval {
        min: 1,
        max: 9_000,
        flags: 0,
    };
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_RATE), (8_000, 8_000));
}

/// Los topes duros del anillo: el fragmento no pasa de la mitad del
/// anillo (para que quepan dos) y el búfer no pasa del anillo. El suelo
/// de 128 marcos deja pasar los 5 ms a 48 kHz que un cliente de baja
/// latencia pide de verdad (240 marcos), que es por lo que no es 256.
#[test]
fn the_ring_bounds_the_period_and_the_buffer() {
    let d = pcm();
    let mut p = any();
    assert!(d.refine(&mut p));
    assert_eq!(p.intervals[PcmDev::IV_PERIOD_SIZE].min, 128);
    assert_eq!(
        p.intervals[PcmDev::IV_PERIOD_SIZE].max,
        (RING_FRAMES / 2) as u32
    );
    assert_eq!(p.intervals[PcmDev::IV_BUFFER_SIZE].max, RING_FRAMES as u32);
    assert_eq!(p.intervals[PcmDev::IV_BUFFER_SIZE].min, 512);
    // El tope de 1024 fragmentos es el duro, pero la propagacion lo
    // aprieta hasta el que de verdad cabe: el anillo entero en
    // fragmentos del minimo (32768 / 128).
    assert!(p.intervals[PcmDev::IV_PERIODS].max <= 1024);
    assert_eq!(
        p.intervals[PcmDev::IV_PERIODS].max,
        (RING_FRAMES / 128) as u32
    );
    assert!(p.intervals[PcmDev::IV_PERIODS].min >= 1);

    // 5 ms a 48 kHz son 240 marcos, y tienen que pasar.
    let mut p = any();
    pin(&mut p, PcmDev::IV_RATE, 48_000);
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, 240);
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_SIZE), (240, 240));

    // Un fragmento más grande que medio anillo, no.
    let mut p = any();
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, (RING_FRAMES / 2 + 1) as u32);
    assert!(!d.refine(&mut p));
    // Un búfer más grande que el anillo, tampoco.
    let mut p = any();
    pin(&mut p, PcmDev::IV_BUFFER_SIZE, (RING_FRAMES + 1) as u32);
    assert!(!d.refine(&mut p));
    // Ni un fragmento por debajo del suelo.
    let mut p = any();
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, 127);
    assert!(!d.refine(&mut p));

    // Un anillo pequeño no deja el tope del fragmento por debajo de su
    // propio suelo (un anillo de 200 marcos daría 100 < 128), que
    // invertiría el intervalo. Se mira sobre el barrido y no sobre
    // `refine`, porque un anillo así no tiene NINGUNA configuración: el
    // búfer mínimo son 512 marcos y no caben.
    let small = PcmDev::new(Arc::new(Ring(200)), 0);
    let mut p = any();
    for i in p.intervals.iter_mut() {
        PcmDev::iv_normalize(i);
    }
    small.propagate(&mut p.intervals);
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_SIZE), (128, 128));
    let mut p = any();
    assert!(!small.refine(&mut p));
    assert!(PcmDev::iv_empty(&p.intervals[PcmDev::IV_BUFFER_SIZE]));
}

/// Bytes y marcos son el mismo número por cuatro en los dos sentidos, y
/// la vuelta redondea hacia arriba: el que redondeara hacia abajo se
/// comería el valor legal cuando los bytes no son múltiplo de un marco.
#[test]
fn bytes_and_frames_are_the_same_bound_times_four() {
    let d = pcm();
    let mut p = any();
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, 1024);
    pin(&mut p, PcmDev::IV_BUFFER_SIZE, 4096);
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_BYTES), (4096, 4096));
    assert_eq!(iv(&p, PcmDev::IV_BUFFER_BYTES), (16384, 16384));

    // Y al revés: pedir los bytes fija los marcos.
    let mut p = any();
    pin(&mut p, PcmDev::IV_PERIOD_BYTES, 4096);
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_SIZE), (1024, 1024));

    // Unos bytes que no son un número entero de marcos no vacían el
    // intervalo: el tope de marcos redondea hacia arriba y es `install`
    // quien se queda con el número coherente.
    let mut p = any();
    p.intervals[PcmDev::IV_PERIOD_BYTES] = SndInterval {
        min: 4094,
        max: 4094,
        flags: 0,
    };
    assert!(d.refine(&mut p));
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_SIZE), (1023, 1024));
}

/// Las tres peticiones que los comentarios de `propagate` nombran como
/// las que SE RECHAZABAN: medio segundo a 44,1 kHz con fragmentos de
/// 125 ms (22050 marcos no son cuatro fragmentos de 5512,5), dos
/// fragmentos sobre el búfer mínimo (512 entre 2 no da 170 ni 256 sin
/// contradecirse), y medio segundo a 11,025 kHz (500000 µs contra 5513
/// marcos). Las tres tienen que pasar.
#[test]
fn the_three_requests_that_used_to_be_rejected_are_accepted() {
    let d = pcm();

    // 0,5 s a 44,1 kHz, fragmentos de 125 ms: lo que pide mpg123.
    let mut p = any();
    pin(&mut p, PcmDev::IV_RATE, 44_100);
    pin(&mut p, PcmDev::IV_BUFFER_SIZE, 22_050);
    pin(&mut p, PcmDev::IV_PERIOD_TIME, 125_000);
    assert!(d.refine(&mut p), "0,5 s a 44,1 kHz rechazado");

    // Dos fragmentos y el búfer mínimo.
    let mut p = any();
    pin(&mut p, PcmDev::IV_PERIODS, 2);
    pin(&mut p, PcmDev::IV_BUFFER_SIZE, 512);
    assert!(d.refine(&mut p), "dos fragmentos rechazados");
    assert_eq!(iv(&p, PcmDev::IV_PERIODS), (2, 2));
    let (ps_lo, ps_hi) = iv(&p, PcmDev::IV_PERIOD_SIZE);
    assert!(ps_lo <= 256 && 256 <= ps_hi, "256 fuera de {}..{}", ps_lo, ps_hi);

    // 0,5 s a 11,025 kHz por tiempo de búfer.
    let mut p = any();
    pin(&mut p, PcmDev::IV_RATE, 11_025);
    pin(&mut p, PcmDev::IV_BUFFER_TIME, 500_000);
    assert!(d.refine(&mut p), "0,5 s a 11,025 kHz rechazado");
    let (bs_lo, bs_hi) = iv(&p, PcmDev::IV_BUFFER_SIZE);
    assert!(bs_lo <= 5512 && 5512 <= bs_hi, "{}..{}", bs_lo, bs_hi);
}

/// Un cliente que pide un tiempo se lleva el tamaño que ese tiempo
/// contiene, y pedir el tamaño le devuelve un tiempo que CONTIENE el
/// que pidió: las dos direcciones describen la misma celda semiabierta
/// y no se pelean.
#[test]
fn a_time_and_a_size_describe_the_same_half_open_cell() {
    let d = pcm();
    for &rate in RATES.iter() {
        for &us in [5_000u64, 10_000, 23_220, 125_000, 500_000].iter() {
            let frames = (us * rate as u64 / 1_000_000) as u32;
            if !(128..=(RING_FRAMES / 2) as u32).contains(&frames) {
                continue;
            }
            // Por tiempo: el tamaño que sale contiene el marco exacto.
            let mut p = any();
            pin(&mut p, PcmDev::IV_RATE, rate);
            p.intervals[PcmDev::IV_PERIOD_TIME] = SndInterval {
                min: us as u32,
                max: us as u32,
                flags: 0,
            };
            assert!(d.refine(&mut p), "{} us a {} Hz", us, rate);
            let (lo, hi) = iv(&p, PcmDev::IV_PERIOD_SIZE);
            assert!(
                lo <= frames && frames <= hi,
                "{} us a {} Hz: {} fuera de {}..{}",
                us,
                rate,
                frames,
                lo,
                hi
            );
            // Y por tamaño: el tiempo que sale contiene el que se pidió.
            let mut p = any();
            pin(&mut p, PcmDev::IV_RATE, rate);
            pin(&mut p, PcmDev::IV_PERIOD_SIZE, frames);
            assert!(d.refine(&mut p));
            let (t_lo, t_hi) = iv(&p, PcmDev::IV_PERIOD_TIME);
            assert!(
                (t_lo as u64) <= us && us <= t_hi as u64,
                "{} marcos a {} Hz: {} us fuera de {}..{}",
                frames,
                rate,
                us,
                t_lo,
                t_hi
            );
        }
    }
}

/// El barrido sólo puede ESTRECHAR, así que llega a un punto fijo; y
/// para una petición que tiene solución llega dentro de las ocho
/// vueltas que `refine` le da, que es lo que hace que ese tope sea una
/// red y no el que decide el resultado.
#[test]
fn the_sweep_only_narrows_and_reaches_its_fixed_point_within_the_cap() {
    let d = pcm();
    for (rate, ps, bs, n) in [
        (0u32, 0u32, 0u32, 0u32),
        (48_000, 0, 0, 0),
        (8_000, 0, 0, 0),
        (192_000, 0, 0, 0),
        (44_100, 0, 22_050, 0),
        (48_000, 1024, 0, 4),
        (48_000, 128, 512, 4),
        (192_000, 4096, 16_384, 0),
        (96_000, 0, 0, 256),
    ] {
        let mut p = any();
        if rate != 0 {
            pin(&mut p, PcmDev::IV_RATE, rate);
        }
        if ps != 0 {
            pin(&mut p, PcmDev::IV_PERIOD_SIZE, ps);
        }
        if bs != 0 {
            pin(&mut p, PcmDev::IV_BUFFER_SIZE, bs);
        }
        if n != 0 {
            pin(&mut p, PcmDev::IV_PERIODS, n);
        }
        for i in p.intervals.iter_mut() {
            PcmDev::iv_normalize(i);
        }
        let mut sweeps = 0;
        loop {
            let before = p.intervals;
            if !d.propagate(&mut p.intervals) {
                break;
            }
            sweeps += 1;
            for (idx, (a, b)) in before.iter().zip(p.intervals.iter()).enumerate() {
                assert!(
                    b.min >= a.min && b.max <= a.max,
                    "{} se ensancho: {}..{} -> {}..{}",
                    PcmDev::iv_name(idx),
                    a.min,
                    a.max,
                    b.min,
                    b.max
                );
            }
            assert!(
                sweeps < 8,
                "no llega al punto fijo en ocho vueltas: {:?}",
                (rate, ps, bs, n)
            );
        }
        assert!(
            !p.intervals.iter().any(PcmDev::iv_empty),
            "{:?} salio vacio y tenia solucion",
            (rate, ps, bs, n)
        );
    }
}

/// Una petición que NO tiene solución sale vacía, y sale vacía para
/// siempre: un intervalo vacío no vuelve a llenarse por mucho que se
/// propague, que es lo que deja que el tope de ocho vueltas de `refine`
/// corte el barrido sin cambiar la respuesta.
///
/// Las dos de aquí son contradicciones de verdad y no fallos del
/// barrido: 512 marcos de búfer en fragmentos de 128 son cuatro
/// fragmentos y no dos, y 1024 fragmentos del mínimo son 131072 marcos,
/// cuatro veces el anillo.
#[test]
fn a_request_with_no_solution_comes_out_empty_and_stays_empty() {
    let d = pcm();
    for (rate, ps, bs, n) in [(8_000u32, 128u32, 512u32, 2u32), (96_000, 0, 0, 1024)] {
        let mut p = any();
        pin(&mut p, PcmDev::IV_RATE, rate);
        if ps != 0 {
            pin(&mut p, PcmDev::IV_PERIOD_SIZE, ps);
        }
        if bs != 0 {
            pin(&mut p, PcmDev::IV_BUFFER_SIZE, bs);
        }
        if n != 0 {
            pin(&mut p, PcmDev::IV_PERIODS, n);
        }
        assert!(!d.refine(&mut p), "{:?} aceptada", (rate, ps, bs, n));
        let empty: Vec<&str> = p
            .intervals
            .iter()
            .enumerate()
            .filter(|(_, v)| PcmDev::iv_empty(v))
            .map(|(i, _)| PcmDev::iv_name(i))
            .collect();
        assert!(!empty.is_empty());
        // Veinte barridos más no rescatan ninguno.
        for _ in 0..20 {
            d.propagate(&mut p.intervals);
        }
        for &name in empty.iter() {
            let idx = (0..12).find(|&i| PcmDev::iv_name(i) == name).unwrap();
            assert!(
                PcmDev::iv_empty(&p.intervals[idx]),
                "{} se lleno despues de estar vacio",
                name
            );
        }
        // Y `install` lo recupera soltando lo que puede soltar, o
        // contesta EINVAL; lo que no puede es dar por bueno el triple
        // contradictorio.
        let mut q = any();
        pin(&mut q, PcmDev::IV_RATE, rate);
        if ps != 0 {
            pin(&mut q, PcmDev::IV_PERIOD_SIZE, ps);
        }
        if bs != 0 {
            pin(&mut q, PcmDev::IV_BUFFER_SIZE, bs);
        }
        if n != 0 {
            pin(&mut q, PcmDev::IV_PERIODS, n);
        }
        if d.install(&mut q).is_ok() {
            assert!(!q.intervals.iter().any(PcmDev::iv_empty));
            assert_eq!(q.intervals[PcmDev::IV_RATE].min, rate);
        }
    }
}

/// `refine` es idempotente: pasarlo dos veces no estrecha más. Un
/// cliente de alsa-lib llama a HW_REFINE después de cada
/// `set_*`/`set_*_near`, decenas de veces sobre la misma petición, y si
/// cada pasada mordiera un marco la configuración que acabara
/// instalando no sería la que pidió.
#[test]
fn refining_twice_changes_nothing() {
    let d = pcm();
    for (rate, ps, bs) in [
        (0u32, 0u32, 0u32),
        (48_000, 0, 0),
        (44_100, 0, 22_050),
        (48_000, 1024, 4096),
        (11_025, 0, 5512),
        (192_000, 240, 0),
    ] {
        let mut p = any();
        if rate != 0 {
            pin(&mut p, PcmDev::IV_RATE, rate);
        }
        if ps != 0 {
            pin(&mut p, PcmDev::IV_PERIOD_SIZE, ps);
        }
        if bs != 0 {
            pin(&mut p, PcmDev::IV_BUFFER_SIZE, bs);
        }
        assert!(d.refine(&mut p), "{:?} rechazado", (rate, ps, bs));
        let once = p.intervals;
        assert!(d.refine(&mut p), "{:?} rechazado en la segunda", (rate, ps, bs));
        for (idx, (a, b)) in once.iter().zip(p.intervals.iter()).enumerate() {
            assert_eq!(
                (a.min, a.max),
                (b.min, b.max),
                "{} cambio en la segunda pasada de {:?}",
                PcmDev::iv_name(idx),
                (rate, ps, bs)
            );
        }
    }
}

/// Un anillo que todavía no tiene tamaño (un dispositivo sin configurar
/// contesta cero bytes) no divide por cero ni acepta una configuración
/// que no existe.
#[test]
fn a_ring_of_no_frames_does_not_divide_by_zero() {
    let d = PcmDev::new(Arc::new(Ring(0)), 0);
    let mut p = any();
    // Con el búfer mínimo de 512 marcos por encima del anillo, no hay
    // configuración: lo que importa es que lo diga y no que reviente.
    assert!(!d.refine(&mut p));
}

// ── Las dos etapas de recuperación ──────────────────────────────────

/// `reopen` abre de par en par exactamente los parámetros que se le
/// nombran y no toca ningún otro.
#[test]
fn reopen_opens_exactly_what_it_is_told_to() {
    let mut p = any();
    for (i, iv) in p.intervals.iter_mut().enumerate() {
        iv.min = 100 + i as u32;
        iv.max = 200 + i as u32;
        iv.flags = INTERVAL_EMPTY;
    }
    PcmDev::reopen(&mut p, &[PcmDev::IV_PERIODS, PcmDev::IV_TICK_TIME]);
    for (i, iv) in p.intervals.iter().enumerate() {
        if i == PcmDev::IV_PERIODS || i == PcmDev::IV_TICK_TIME {
            assert_eq!((iv.min, iv.max, iv.flags), (0, u32::MAX, 0), "{}", i);
        } else {
            assert_eq!((iv.min, iv.max), (100 + i as u32, 200 + i as u32), "{}", i);
            assert_ne!(iv.flags, 0, "{}", i);
        }
    }
}

/// La primera etapa suelta sólo lo que alsa-lib DERIVA y se queda con
/// los dos tamaños que el cliente eligió: son los que fijan su
/// latencia, y tirarlos contestaría a una petición de 10 ms con el
/// anillo entero de 683 ms. La segunda suelta también los tamaños. Y
/// ninguna de las dos toca la frecuencia ni los canales en ninguna
/// etapa.
#[test]
fn the_two_recovery_stages_let_go_of_exactly_their_own_parameters() {
    let derived = [
        PcmDev::IV_PERIOD_TIME,
        PcmDev::IV_PERIOD_BYTES,
        PcmDev::IV_PERIODS,
        PcmDev::IV_BUFFER_TIME,
        PcmDev::IV_BUFFER_BYTES,
        PcmDev::IV_TICK_TIME,
    ];
    let sizes = [PcmDev::IV_PERIOD_SIZE, PcmDev::IV_BUFFER_SIZE];
    let kept = [
        PcmDev::IV_SAMPLE_BITS,
        PcmDev::IV_FRAME_BITS,
        PcmDev::IV_CHANNELS,
        PcmDev::IV_RATE,
    ];

    let pinned = |idx: usize| 1000 + idx as u32;
    let mut p = any();
    for (i, iv) in p.intervals.iter_mut().enumerate() {
        iv.min = pinned(i);
        iv.max = pinned(i);
    }
    PcmDev::relax_derived_params(&mut p);
    for &i in derived.iter() {
        assert_eq!(p.intervals[i].max, u32::MAX, "{}", PcmDev::iv_name(i));
    }
    for &i in sizes.iter().chain(kept.iter()) {
        assert_eq!(
            (p.intervals[i].min, p.intervals[i].max),
            (pinned(i), pinned(i)),
            "{} se solto en la primera etapa",
            PcmDev::iv_name(i)
        );
    }

    let mut p = any();
    for (i, iv) in p.intervals.iter_mut().enumerate() {
        iv.min = pinned(i);
        iv.max = pinned(i);
    }
    PcmDev::relax_size_params(&mut p);
    for &i in derived.iter().chain(sizes.iter()) {
        assert_eq!(p.intervals[i].max, u32::MAX, "{}", PcmDev::iv_name(i));
    }
    for &i in kept.iter() {
        assert_eq!(
            (p.intervals[i].min, p.intervals[i].max),
            (pinned(i), pinned(i)),
            "{} se solto en la segunda etapa",
            PcmDev::iv_name(i)
        );
    }
}

/// Y las dos etapas juntas: la petición de mpg123 que `refine` sí
/// rechaza (un tiempo de fragmento y un número de marcos que se
/// contradicen en un marco) sale adelante por `install`, y la que es
/// imposible de verdad (mono) sigue siendo EINVAL en las tres pasadas.
#[test]
fn the_recovery_saves_an_inconsistent_request_and_not_an_impossible_one() {
    let d = pcm();

    // period_size y period_time que no pueden ser los dos: 1024 marcos
    // a 48 kHz son 21333 us, no 10000.
    let mut p = any();
    pin(&mut p, PcmDev::IV_RATE, 48_000);
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, 1024);
    pin(&mut p, PcmDev::IV_PERIOD_TIME, 10_000);
    assert!(!d.refine(&mut p), "la contradiccion paso el refine");

    let mut p = any();
    pin(&mut p, PcmDev::IV_RATE, 48_000);
    pin(&mut p, PcmDev::IV_PERIOD_SIZE, 1024);
    pin(&mut p, PcmDev::IV_PERIOD_TIME, 10_000);
    // La primera etapa suelta el tiempo y se queda con los 1024 marcos
    // que el cliente eligió.
    d.install(&mut p).expect("install no recupero");
    assert_eq!(iv(&p, PcmDev::IV_PERIOD_SIZE), (1024, 1024));

    // Mono no se arregla soltando tamaños: la frecuencia, los canales y
    // las máscaras no se tocan en ninguna etapa.
    let mut p = any();
    pin(&mut p, PcmDev::IV_CHANNELS, 1);
    assert!(matches!(d.install(&mut p), Err(FsError::InvalidParam)));
}
