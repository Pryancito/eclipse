//! Los tests de la conversión de muestras de `/dev/dsp`: `sample_bytes`,
//! las dos tablas de G.711, `decode_sample`, `silence_byte` y
//! `convert_frames`.
//!
//! Viven en su propio fichero y no dentro de `dsp.rs` porque lo que
//! prueban es aritmética pura, sin dispositivo: los tests que ya había
//! en `dsp.rs` entran todos por `io_control`/`write_pcm` y lo que miran
//! es el PCM que llega al anillo, así que un error de un bit en una de
//! estas tablas sólo se vería como «suena mal».
//!
//! Los valores de G.711 no son los que salen del código: son los de la
//! recomendación (±32124 para µ-law, ±32256 para A-law, y el ±8 de
//! A-law, que no tiene un cero exacto). Si alguien reescribe una tabla,
//! esto dice si sigue siendo G.711 o sólo si sigue siendo ella misma.

use super::*;

/// Cada formato de [`SUPPORTED_FORMATS`] mide lo que dice su nombre, y
/// uno que no esté en la lista no mide nada: es lo que hace que `SETFMT`
/// no lo instale.
#[test]
fn every_supported_format_has_a_size_and_nothing_else_does() {
    for (format, bytes) in [
        (AFMT_MU_LAW, 1),
        (AFMT_A_LAW, 1),
        (AFMT_U8, 1),
        (AFMT_S8, 1),
        (AFMT_S16_LE, 2),
        (AFMT_S16_BE, 2),
        (AFMT_U16_LE, 2),
        (AFMT_U16_BE, 2),
        (AFMT_S24_PACKED, 3),
        (AFMT_S24_LE, 4),
        (AFMT_S24_BE, 4),
        (AFMT_S32_LE, 4),
        (AFMT_S32_BE, 4),
        (AFMT_FLOAT, 4),
    ] {
        assert_eq!(
            sample_bytes(format),
            Some(bytes),
            "formato {format:#x} mide otra cosa"
        );
        assert_ne!(
            format & SUPPORTED_FORMATS,
            0,
            "{format:#x} tiene tamano y no esta en GETFMTS"
        );
    }
    // Y al revés: ningún bit de la máscara se queda sin tamaño, o
    // `GETFMTS` anunciaría un formato que `SETFMT` rechaza.
    for bit in 0..31 {
        let format = 1i32 << bit;
        if format & SUPPORTED_FORMATS != 0 {
            assert!(
                sample_bytes(format).is_some(),
                "GETFMTS anuncia {:#x} y no tiene tamano",
                format
            );
        }
    }
    assert_eq!(sample_bytes(AFMT_QUERY), None);
    assert_eq!(sample_bytes(0x0000_0400), None);
    assert_eq!(sample_bytes(-1), None);
}

/// µ-law, los cuatro puntos que fija G.711: el código 0 es el negativo
/// de fondo de escala, `0x80` el positivo, y los dos códigos de cero
/// (`0xff` y `0x7f`, que es el «menos cero») decodifican a cero exacto.
#[test]
fn mulaw_is_g711() {
    assert_eq!(mulaw_to_i16(0x00), -32124);
    assert_eq!(mulaw_to_i16(0x80), 32124);
    assert_eq!(mulaw_to_i16(0xff), 0);
    assert_eq!(mulaw_to_i16(0x7f), 0);
    // Un par de puntos intermedios de la tabla de la recomendación.
    assert_eq!(mulaw_to_i16(0xfe), 8);
    assert_eq!(mulaw_to_i16(0x7e), -8);
    assert_eq!(mulaw_to_i16(0xf0), 120);
    assert_eq!(mulaw_to_i16(0x70), -120);
}

/// A-law, lo mismo, con sus dos diferencias: el bit de signo marca el
/// semiplano POSITIVO (al contrario que en µ-law) y no existe un cero
/// exacto, el código de silencio vale ±8.
#[test]
fn alaw_is_g711_and_its_sign_bit_is_the_other_way_round() {
    assert_eq!(alaw_to_i16(0xaa), 32256);
    assert_eq!(alaw_to_i16(0x2a), -32256);
    assert_eq!(alaw_to_i16(0xd5), 8);
    assert_eq!(alaw_to_i16(0x55), -8);
    // El bit alto es el signo, y es el positivo: cada código y su
    // pareja con el bit puesto son el mismo valor con signo cambiado.
    for low in 0u8..0x80 {
        assert_eq!(
            alaw_to_i16(low | 0x80),
            -alaw_to_i16(low),
            "{:#04x} y su pareja no son simetricos",
            low
        );
    }
}

/// Las dos tablas son monótonas en la magnitud dentro de cada semiplano:
/// un código mayor es una muestra mayor. Un exponente o una mantisa
/// desplazados un bit rompen esto y no lo rompen en el fondo de escala.
#[test]
fn both_tables_are_monotonic_within_a_half_plane() {
    // µ-law: el código crece hacia el cero (el byte va invertido), así
    // que la magnitud DECRECE con el código dentro de cada semiplano.
    for code in 0u8..0x7f {
        assert!(
            mulaw_to_i16(code) < mulaw_to_i16(code + 1),
            "mu-law no es monotona en {:#04x}",
            code
        );
    }
    for code in 0x80u8..0xff {
        assert!(
            mulaw_to_i16(code) > mulaw_to_i16(code + 1),
            "mu-law no es monotona en {:#04x}",
            code
        );
    }
    // A-law: el `^ 0x55` deja los códigos entrelazados, así que la
    // monotonía se mira sobre el byte ya desentrelazado.
    for raw in 0u8..0x7f {
        let (a, b) = (alaw_to_i16(raw ^ 0x55), alaw_to_i16((raw + 1) ^ 0x55));
        assert!(
            a.abs() <= b.abs(),
            "a-law no crece en {:#04x}: {} -> {}",
            raw,
            a,
            b
        );
    }
}

/// El cero de cada formato lineal decodifica a cero exacto: un U8 o un
/// U16 son binario desplazado y su cero es el punto medio.
#[test]
fn the_zero_of_every_linear_format_is_zero() {
    for (format, bytes) in [
        (AFMT_U8, &[0x80u8, 0, 0, 0][..]),
        (AFMT_S8, &[0x00][..]),
        (AFMT_S16_LE, &[0x00, 0x00][..]),
        (AFMT_S16_BE, &[0x00, 0x00][..]),
        (AFMT_U16_LE, &[0x00, 0x80][..]),
        (AFMT_U16_BE, &[0x80, 0x00][..]),
        (AFMT_S24_PACKED, &[0, 0, 0][..]),
        (AFMT_S24_LE, &[0, 0, 0, 0][..]),
        (AFMT_S24_BE, &[0, 0, 0, 0][..]),
        (AFMT_S32_LE, &[0, 0, 0, 0][..]),
        (AFMT_S32_BE, &[0, 0, 0, 0][..]),
        (AFMT_FLOAT, &0f32.to_le_bytes()[..]),
    ] {
        assert_eq!(
            decode_sample(format, bytes),
            0,
            "el cero de {format:#x} no es cero"
        );
    }
}

/// El fondo de escala de cada formato lineal llega al fondo de escala
/// de S16, por arriba y por abajo. Es lo que dice que el formato se
/// está leyendo con el ancho y el signo que toca y no desplazado.
///
/// Los de ocho bits son la excepción por arriba y no por abajo: su
/// máximo es 127, que desplazado ocho bits son 32512 y no 32767. No es
/// un redondeo que se pueda arreglar sin dejar de ser un desplazamiento,
/// y es lo que hace Linux.
#[test]
fn full_scale_stays_full_scale_in_every_linear_format() {
    for (format, lo, hi, want_hi) in [
        (
            AFMT_U8,
            &[0x00u8, 0, 0, 0][..],
            &[0xff, 0, 0, 0][..],
            32512i16,
        ),
        (AFMT_S8, &[0x80][..], &[0x7f][..], 32512),
        (AFMT_S16_LE, &[0x00, 0x80][..], &[0xff, 0x7f][..], i16::MAX),
        (AFMT_S16_BE, &[0x80, 0x00][..], &[0x7f, 0xff][..], i16::MAX),
        (AFMT_U16_LE, &[0x00, 0x00][..], &[0xff, 0xff][..], i16::MAX),
        (AFMT_U16_BE, &[0x00, 0x00][..], &[0xff, 0xff][..], i16::MAX),
        (
            AFMT_S24_PACKED,
            &[0x00, 0x00, 0x80][..],
            &[0xff, 0xff, 0x7f][..],
            i16::MAX,
        ),
        (
            AFMT_S24_LE,
            &[0x00, 0x00, 0x80, 0x00][..],
            &[0xff, 0xff, 0x7f, 0x00][..],
            i16::MAX,
        ),
        (
            AFMT_S24_BE,
            &[0x00, 0x80, 0x00, 0x00][..],
            &[0x00, 0x7f, 0xff, 0xff][..],
            i16::MAX,
        ),
        (
            AFMT_S32_LE,
            &[0x00, 0x00, 0x00, 0x80][..],
            &[0xff, 0xff, 0xff, 0x7f][..],
            i16::MAX,
        ),
        (
            AFMT_S32_BE,
            &[0x80, 0x00, 0x00, 0x00][..],
            &[0x7f, 0xff, 0xff, 0xff][..],
            i16::MAX,
        ),
    ] {
        assert_eq!(
            decode_sample(format, lo),
            i16::MIN,
            "el minimo de {format:#x} no llega al fondo de escala"
        );
        assert_eq!(
            decode_sample(format, hi),
            want_hi,
            "el maximo de {format:#x} no llega al fondo de escala"
        );
    }
    // Y los de ocho bits sí llegan al fondo de escala por abajo: un
    // desplazamiento no puede perder el signo.
    assert_eq!(decode_sample(AFMT_U8, &[0x00]), i16::MIN);
    assert_eq!(decode_sample(AFMT_S8, &[0x80]), i16::MIN);
}

/// Cada pareja de endianismos es el mismo número escrito al revés, y las
/// dos decodifican igual. Un formato leído con el endianismo del otro da
/// ruido blanco a todo volumen, que es exactamente el fallo que esto
/// caza.
#[test]
fn each_endian_pair_decodes_the_same_number() {
    for v in [1i32, -1, 0x1234, -0x4321, 0x0033_4455, -0x0033_4455] {
        let s16 = (v as i16).to_le_bytes();
        assert_eq!(
            decode_sample(AFMT_S16_LE, &s16),
            decode_sample(AFMT_S16_BE, &[s16[1], s16[0]]),
        );
        let u16v = ((v as i16) as u16 ^ 0x8000).to_le_bytes();
        assert_eq!(
            decode_sample(AFMT_U16_LE, &u16v),
            decode_sample(AFMT_U16_BE, &[u16v[1], u16v[0]]),
        );
        let s32 = ((v as i64) << 8) as i32;
        let le = s32.to_le_bytes();
        assert_eq!(
            decode_sample(AFMT_S32_LE, &le),
            decode_sample(AFMT_S32_BE, &s32.to_be_bytes()),
        );
        // S24 en 32 bits: los tres bytes significativos y un relleno,
        // que en LE es el último y en BE el primero.
        let s24 = (v as i16) as i32;
        let packed = [0, s24 as u8, (s24 >> 8) as u8];
        assert_eq!(
            decode_sample(AFMT_S24_LE, &[packed[0], packed[1], packed[2], 0xa5]),
            decode_sample(AFMT_S24_BE, &[0xa5, packed[2], packed[1], packed[0]]),
        );
        // Y el empaquetado de 3 bytes lee lo mismo que el de 4.
        assert_eq!(
            decode_sample(AFMT_S24_PACKED, &packed),
            decode_sample(AFMT_S24_LE, &[packed[0], packed[1], packed[2], 0]),
        );
    }
}

/// El relleno del contenedor de 32 bits de un S24 no entra en la
/// muestra: cambiarlo no cambia nada.
#[test]
fn the_padding_byte_of_a_24_in_32_sample_is_ignored() {
    for pad in [0x00u8, 0x7f, 0xff] {
        assert_eq!(
            decode_sample(AFMT_S24_LE, &[0x11, 0x22, 0x33, pad]),
            decode_sample(AFMT_S24_LE, &[0x11, 0x22, 0x33, 0x00]),
        );
        assert_eq!(
            decode_sample(AFMT_S24_BE, &[pad, 0x33, 0x22, 0x11]),
            decode_sample(AFMT_S24_BE, &[0x00, 0x33, 0x22, 0x11]),
        );
    }
}

/// El float va en `-1.0..=1.0`, lo que se sale se recorta y un NaN es
/// silencio. Un NaN que pasara al anillo sale por el altavoz como un
/// chasquido a todo volumen.
#[test]
fn float_is_clamped_and_a_nan_is_silence() {
    let dec = |f: f32| decode_sample(AFMT_FLOAT, &f.to_le_bytes());
    assert_eq!(dec(0.0), 0);
    assert_eq!(dec(1.0), 32767);
    assert_eq!(dec(-1.0), -32767);
    assert_eq!(dec(0.5), 16383);
    assert_eq!(dec(-0.5), -16383);
    // Fuera de rango, recortado a fondo de escala y no envuelto.
    assert_eq!(dec(2.0), 32767);
    assert_eq!(dec(-2.0), -32767);
    assert_eq!(dec(1e30), 32767);
    assert_eq!(dec(f32::INFINITY), 32767);
    assert_eq!(dec(f32::NEG_INFINITY), -32767);
    assert_eq!(dec(f32::NAN), 0);
    assert_eq!(dec(-f32::NAN), 0);
}

/// Un formato que no existe decodifica a silencio en vez de leer fuera
/// del `slice`: `write_pcm` le da el tamaño que `sample_bytes` asume
/// cuando no conoce el formato (dos bytes), y el brazo por defecto no
/// puede mirar más allá.
#[test]
fn an_unknown_format_decodes_to_silence() {
    assert_eq!(decode_sample(0x0000_0400, &[0xff, 0xff]), 0);
    assert_eq!(decode_sample(AFMT_QUERY, &[0xff, 0xff]), 0);
}

/// El byte de silencio de cada formato es el silencio DE VERDAD de ese
/// formato, o sea el que decodifica a cero. A-law es la excepción de
/// G.711 (no tiene cero exacto) y su silencio es el ±8 de la tabla,
/// -90 dBFS: inaudible, pero no cero.
#[test]
fn the_silence_byte_of_a_format_decodes_to_silence() {
    for format in [AFMT_U8, AFMT_MU_LAW, AFMT_S8, AFMT_S16_LE, AFMT_S16_BE] {
        let b = silence_byte(format);
        let one = [b; 4];
        assert_eq!(
            decode_sample(format, &one),
            0,
            "el silencio de {format:#x} no es silencio"
        );
    }
    assert_eq!(silence_byte(AFMT_U8), 0x80);
    assert_eq!(silence_byte(AFMT_MU_LAW), 0xff);
    assert_eq!(silence_byte(AFMT_A_LAW), 0xd5);
    assert_eq!(decode_sample(AFMT_A_LAW, &[silence_byte(AFMT_A_LAW)]), 8);
    // Todo lo demás es binario con signo y su silencio es el cero.
    for format in [
        AFMT_S8,
        AFMT_S16_LE,
        AFMT_S16_BE,
        AFMT_S24_LE,
        AFMT_S32_LE,
        AFMT_FLOAT,
        0x0000_0400,
    ] {
        assert_eq!(silence_byte(format), 0, "{format:#x}");
    }
}

/// `convert_frames` saca S16LE estéreo: un mono se duplica en los dos
/// canales y un estéreo pasa canal a canal.
#[test]
fn mono_is_duplicated_and_stereo_is_kept() {
    let mut dst = Vec::new();
    convert_frames(AFMT_S16_LE, 1, &[0x34, 0x12, 0xcd, 0xab], 2, &mut dst);
    assert_eq!(dst, [0x34, 0x12, 0x34, 0x12, 0xcd, 0xab, 0xcd, 0xab]);

    let mut dst = Vec::new();
    convert_frames(AFMT_S16_LE, 2, &[0x34, 0x12, 0xcd, 0xab], 1, &mut dst);
    assert_eq!(dst, [0x34, 0x12, 0xcd, 0xab]);
}

/// Cada marco de salida son [`HW_FRAME`] bytes, los `frames` que se
/// piden y ni uno más, y se AÑADEN a lo que ya hubiera en `dst`: el
/// camino de escritura reutiliza el mismo `Vec` para el marco parcial
/// que arrastra de la escritura anterior.
#[test]
fn the_output_is_appended_and_is_exactly_the_frames_asked_for() {
    let mut dst = alloc::vec![0xaa, 0xbb];
    let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
    convert_frames(AFMT_S16_LE, 2, &src, 1, &mut dst);
    assert_eq!(dst.len(), 2 + HW_FRAME);
    assert_eq!(&dst[..2], [0xaa, 0xbb]);
    convert_frames(AFMT_S16_LE, 2, &src[4..], 1, &mut dst);
    assert_eq!(dst.len(), 2 + 2 * HW_FRAME);
    assert_eq!(&dst[2..], [1, 2, 3, 4, 5, 6, 7, 8]);

    // Cero marcos no toca el destino.
    let before = dst.clone();
    convert_frames(AFMT_S16_LE, 2, &src, 0, &mut dst);
    assert_eq!(dst, before);
}

/// Con más de dos canales se cogen los dos primeros y el marco avanza
/// por el ancho completo: un 5.1 suena por los altavoces de delante en
/// vez de deslizarse canal a canal.
#[test]
fn a_surround_frame_keeps_its_first_two_channels_and_its_stride() {
    let mut dst = Vec::new();
    // Dos marcos de 4 canales S8: (1,2,3,4) y (5,6,7,8).
    convert_frames(AFMT_S8, 4, &[1, 2, 3, 4, 5, 6, 7, 8], 2, &mut dst);
    let got: Vec<i16> = dst
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    assert_eq!(got, [1 << 8, 2 << 8, 5 << 8, 6 << 8]);
}

/// Un formato desconocido (que `SETFMT` no instala, pero que el brazo
/// por defecto tiene que sobrevivir) se trata como de dos bytes y sale
/// silencio, sin salirse del `slice` de entrada.
#[test]
fn an_unknown_format_converts_to_silence_without_reading_past_the_input() {
    let mut dst = Vec::new();
    // Exactamente 2 canales x 2 bytes x 2 marcos: si asumiera otro
    // tamaño, el índice se saldría y esto sería un panic.
    convert_frames(0x0000_0400, 2, &[1, 2, 3, 4, 5, 6, 7, 8], 2, &mut dst);
    assert_eq!(dst, [0; 8]);
}

/// Cada formato convierte un marco entero de µ-law/A-law/U8 por el
/// mismo camino que `decode_sample`: lo que `convert_frames` escribe es
/// exactamente la muestra decodificada, en los dos canales.
#[test]
fn convert_frames_writes_what_decode_sample_says() {
    for format in [
        AFMT_MU_LAW,
        AFMT_A_LAW,
        AFMT_U8,
        AFMT_S8,
        AFMT_S16_LE,
        AFMT_S16_BE,
        AFMT_U16_LE,
        AFMT_U16_BE,
        AFMT_S24_PACKED,
        AFMT_S24_LE,
        AFMT_S24_BE,
        AFMT_S32_LE,
        AFMT_S32_BE,
        AFMT_FLOAT,
    ] {
        let sb = sample_bytes(format).unwrap();
        let src: Vec<u8> = (0..sb as u8).map(|i| 0x31 + i).collect();
        let mut dst = Vec::new();
        convert_frames(format, 1, &src, 1, &mut dst);
        let want = decode_sample(format, &src).to_le_bytes();
        assert_eq!(dst, [want[0], want[1], want[0], want[1]], "{format:#x}");
    }
}
