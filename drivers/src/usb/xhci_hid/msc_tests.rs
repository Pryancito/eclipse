//! Un pendrive: el transporte Bulk-Only y las órdenes SCSI que van encima.
//!
//! `disk_tests` prueba las funciones puras (armar un CBW, leer un CSW,
//! interpretar un INQUIRY o una capacidad); esto prueba el camino:
//! `setup_mass_storage` configurando la pareja de endpoints bulk,
//! `msc_identify` preguntándole quién es, y `msc_rw` leyendo y escribiendo
//! bloques de verdad contra un medio que el fake se guarda.
//!
//! El transporte es el de verdad (USB MSC BOT §5): por el endpoint OUT va un
//! CBW de 31 bytes, por el que toque la fase de datos, y por el IN un CSW de
//! 13 con el mismo `tag`.

use super::xhci_fake::{FakeDev, Hw};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// Bloques y tamaño de bloque de la unidad de los tests: 64 bloques de 512,
/// que es un pendrive de juguete pero con la aritmética de uno de verdad.
const BLOCKS: u64 = 64;
const BS: u32 = 512;

/// Un controlador arrancado con esa unidad ya enumerada en el puerto 1.
fn with_disk(dev: FakeDev) -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    // Las esperas de «no lista» son de milisegundos y no leen registros.
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, dev);
    x.setup_device(DevTopo::root(1), HS)
        .expect("la unidad enumera");
    (hw, x)
}

/// Los códigos de orden de las órdenes que recibió, en orden.
fn ops(hw: &Hw) -> Vec<u8> {
    hw.scsi_cdbs(1)
        .iter()
        .filter_map(|c| c.first().copied())
        .collect()
}

// --- la identificación ----------------------------------------------------

/// Lo que pasa entre «hay algo de clase 8 en el puerto 1» y «hay un disco de
/// tantos bloques»: tres órdenes SCSI en un orden que no es negociable, y
/// ninguna de ellas se puede mandar sin los dos endpoints bulk configurados.
#[test]
fn un_pendrive_enchufado_se_identifica_entero() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS));

    assert_eq!(
        x.mscs.len(),
        1,
        "la interfaz de almacenamiento no se apuntó"
    );
    let m = &x.mscs[0];
    // EP2 IN es el DCI 5 y EP1 OUT el 2 (2*ep + dirección).
    assert_eq!((m.dci_in, m.dci_out), (5, 2), "los DCI bulk salen mal");

    assert_eq!(
        ops(&hw),
        alloc::vec![0x00, 0x12, 0x25],
        "el orden es TEST UNIT READY, INQUIRY y READ CAPACITY(10)"
    );

    // Y lo que el driver se queda de todo eso.
    assert_eq!(
        scsi_text(&m.inquiry.vendor),
        "Eclipse",
        "el fabricante no se ha leído del INQUIRY"
    );
    assert!(m.inquiry.removable, "un pendrive es extraíble");
    let cap = m.capacity.expect("la capacidad");
    assert_eq!(
        (cap.last_lba, cap.block_size),
        (BLOCKS - 1, BS),
        "la capacidad no es la que contestó la unidad"
    );
}

/// `GET_MAX_LUN` es una petición de clase, y su respuesta son las unidades
/// lógicas menos una: un lector de tarjetas con cuatro ranuras contesta 3.
#[test]
fn las_unidades_logicas_salen_de_get_max_lun() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).with_luns(3));
    assert_eq!(x.mscs[0].max_lun, 3, "no se ha leído GET_MAX_LUN");
    assert!(
        hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0xa1 && s.breq == 0xfe && s.wlen == 1),
        "no se ha pedido GET_MAX_LUN: {:?}",
        hw.requests(1)
    );
}

/// Y es opcional: un STALL quiere decir «una sola unidad lógica», no «este
/// dispositivo no vale». Un driver que lo tratase como error dejaría sin
/// montar cualquier pendrive de los que no lo implementan.
#[test]
fn un_stall_en_get_max_lun_es_una_sola_unidad() {
    let (_hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).stalling(0xa1, 0xfe));
    assert_eq!(x.mscs.len(), 1, "la unidad no se ha apuntado");
    assert_eq!(
        x.mscs[0].max_lun, 0,
        "un STALL no son cero unidades lógicas"
    );
    assert!(
        x.mscs[0].capacity.is_some(),
        "la identificación se ha parado en GET_MAX_LUN"
    );
}

/// Y si no contesta de ninguna manera, tampoco: una petición que se queda sin
/// respuesta es un plazo agotado, y de ahí sólo se puede salir suponiendo UNA
/// unidad lógica. Suponer dieciséis sería preguntarle por quince unidades que
/// no existen en cada identificación.
#[test]
fn un_get_max_lun_sin_respuesta_es_una_sola_unidad() {
    let (_hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).muting(0xa1, 0xfe));
    assert_eq!(x.mscs.len(), 1, "la unidad no se ha apuntado");
    assert_eq!(
        x.mscs[0].max_lun, 0,
        "un plazo agotado no son dieciséis unidades lógicas"
    );
    assert!(
        x.mscs[0].capacity.is_some(),
        "la identificación se ha parado en GET_MAX_LUN"
    );
}

/// Un pendrive recién enchufado contesta «no lista» un rato. Pedirle el
/// sentido es lo que limpia esa condición de atención: sin eso, los hay que
/// repiten el mismo fallo para siempre y nunca se montan.
#[test]
fn una_unidad_que_arranca_lenta_se_reintenta_pidiendole_el_sentido() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).with_not_ready(2));

    assert_eq!(
        ops(&hw),
        alloc::vec![0x00, 0x03, 0x00, 0x03, 0x00, 0x12, 0x25],
        "el reintento no pide el sentido entre vuelta y vuelta"
    );
    assert!(
        x.mscs[0].capacity.is_some(),
        "la unidad acabó lista y aun así no se le preguntó la capacidad"
    );
}

/// Una unidad que nunca está lista se queda apuntada —sale en `/proc/usbhid`,
/// que es lo que contesta «¿ve el sistema mi pendrive?»— pero sin capacidad:
/// sin medio dentro no hay nada que leer.
#[test]
fn una_unidad_que_nunca_esta_lista_se_apunta_sin_capacidad() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).with_not_ready(u32::MAX));

    assert_eq!(x.mscs.len(), 1, "la unidad no se ha apuntado");
    assert!(
        x.mscs[0].capacity.is_none(),
        "se ha leído una capacidad de una unidad sin medio"
    );
    // El INQUIRY sí se le hace: el nombre se sabe sin medio dentro.
    assert!(ops(&hw).contains(&0x12), "no se le ha preguntado quién es");
    assert!(
        !ops(&hw).contains(&0x25),
        "se le ha pedido la capacidad sin estar lista"
    );
}

/// Un disco de más de 2 TiB no cabe en los cuatro bytes de `READ
/// CAPACITY(10)`, y la forma que tiene de decirlo es contestar el último LBA
/// saturado: entonces hay que preguntar otra vez con la de 16 bytes.
#[test]
fn una_unidad_demasiado_grande_se_pregunta_con_la_de_16() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).needing_capacity16());

    assert_eq!(
        ops(&hw),
        alloc::vec![0x00, 0x12, 0x25, 0x9e],
        "no se ha repreguntado con READ CAPACITY(16)"
    );
    let cap = x.mscs[0].capacity.expect("la capacidad de 16");
    assert_eq!(
        cap.last_lba,
        BLOCKS - 1,
        "la capacidad buena es la de la orden de 16"
    );
}

/// El `tag` es lo único que ata un CSW a su CBW. Creerse el de otra orden es
/// leer el resultado de una pregunta que no se hizo, así que no es que la
/// orden falle: es que el transporte ya no es de fiar.
#[test]
fn un_csw_de_otra_orden_no_se_cree() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).with_wrong_csw_tag());

    assert_eq!(x.mscs.len(), 1, "la unidad se apunta igual");
    assert!(
        x.mscs[0].capacity.is_none(),
        "se ha aceptado la respuesta de un CSW con otro tag"
    );
    // Se para en la primera orden: el transporte falló, no la unidad.
    assert_eq!(
        ops(&hw),
        alloc::vec![0x00],
        "siguió preguntando tras el CSW malo"
    );
}

/// Una orden que la unidad rechaza (CSW status distinto de cero) y un
/// transporte que falla son dos cosas muy distintas, y mezclarlas convierte
/// «no hay tarjeta metida» en «el dispositivo no funciona».
#[test]
fn una_orden_rechazada_no_es_un_fallo_de_transporte() {
    let (hw, x) = with_disk(FakeDev::disk(BLOCKS, BS).failing_scsi(0x25));

    // El INQUIRY entró, así que el transporte está bien; lo que falló es la
    // capacidad, y eso deja la unidad apuntada y sin tamaño.
    assert_eq!(scsi_text(&x.mscs[0].inquiry.vendor), "Eclipse");
    assert!(
        x.mscs[0].capacity.is_none(),
        "se ha inventado una capacidad que la unidad rechazó"
    );
    assert_eq!(ops(&hw), alloc::vec![0x00, 0x12, 0x25]);
}

// --- leer y escribir ------------------------------------------------------

/// Una lectura de punta a punta: CBW con READ(10), fase de datos por el
/// endpoint IN y CSW. Lo que llega al buffer tiene que ser lo que hay en el
/// bloque, no lo que había en otro.
#[test]
fn una_lectura_trae_lo_que_hay_en_ese_bloque() {
    let marca: Vec<u8> = (0..BS as usize).map(|i| (i % 251) as u8).collect();
    let (hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS).with_block(5, &marca));
    let id = x.mscs[0].disk_id;

    let mut buf = alloc::vec![0u8; BS as usize];
    x.msc_rw(id, 5, MscData::Read(&mut buf))
        .expect("la lectura del bloque 5");
    assert_eq!(buf, marca, "la lectura no ha traído el bloque 5");

    let leidas: Vec<Vec<u8>> = hw
        .scsi_cdbs(1)
        .into_iter()
        .filter(|c| c.first() == Some(&0x28))
        .collect();
    assert_eq!(leidas.len(), 1, "una lectura, una orden READ(10)");
    assert_eq!(
        &leidas[0][2..6],
        &[0, 0, 0, 5],
        "el LBA del READ(10) va en big endian"
    );

    // Y un bloque que nadie escribió lee ceros, no lo del vecino.
    let mut otro = alloc::vec![0xffu8; BS as usize];
    x.msc_rw(id, 6, MscData::Read(&mut otro))
        .expect("la lectura del bloque 6");
    assert_eq!(otro, alloc::vec![0u8; BS as usize]);
}

/// Y una escritura: el CBW anuncia los bytes, la fase de datos va por el
/// endpoint OUT y el CSW llega después de ella. El medio del fake es lo que
/// dice si de verdad llegaron.
#[test]
fn una_escritura_llega_al_medio_de_la_unidad() {
    let (hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS));
    let id = x.mscs[0].disk_id;

    let datos: Vec<u8> = (0..BS as usize).map(|i| (i % 97) as u8).collect();
    x.msc_rw(id, 9, MscData::Write(&datos))
        .expect("la escritura del bloque 9");

    assert_eq!(
        hw.media_block(1, 9),
        datos,
        "lo escrito no está en el bloque 9"
    );
    assert_eq!(
        hw.media_block(1, 8),
        alloc::vec![0u8; BS as usize],
        "la escritura ha tocado el bloque de al lado"
    );
    assert!(
        ops(&hw).contains(&0x2a),
        "no se ha mandado un WRITE(10): {:?}",
        ops(&hw)
    );
}

/// Una lectura más allá del medio se para ANTES de mandarla: el final de la
/// tanda tiene que caber en el disco. Dejarla salir la convierte en un LBA
/// fuera de rango, y entonces lo que vuelve es el error de la unidad, que
/// arriba se lee como disco roto en vez de como petición mal hecha.
#[test]
fn una_lectura_fuera_del_medio_no_llega_a_mandarse() {
    let (hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS));
    let id = x.mscs[0].disk_id;
    let antes = hw.scsi_cdbs(1).len();

    let mut buf = alloc::vec![0u8; BS as usize];
    assert_eq!(
        x.msc_rw(id, BLOCKS, MscData::Read(&mut buf)),
        Err(DeviceError::InvalidParam),
        "una lectura de después del último bloque no es un fallo de la unidad"
    );
    assert_eq!(
        hw.scsi_cdbs(1).len(),
        antes,
        "la orden fuera de rango se ha mandado a la unidad"
    );

    // Y el último bloque, que sí cabe, se lee.
    assert!(x.msc_rw(id, BLOCKS - 1, MscData::Read(&mut buf)).is_ok());
}

/// Una tanda más larga de lo que cabe en un TRB se parte en vueltas, y cada
/// vuelta tiene que pedir LOS BLOQUES QUE LE TOCAN: una que vuelva a pedir el
/// principio devuelve los mismos datos dos veces, y eso en un sistema de
/// archivos es corrupción silenciosa.
#[test]
fn una_tanda_mas_larga_que_un_trb_se_parte_en_vueltas_que_avanzan() {
    // 127 bloques es lo que cabe en un TRB de 65 535 bytes, así que 200 son
    // dos vueltas: 127 y 73.
    const GRANDE: u64 = 300;
    const TANDA: usize = 200;
    let dev = (0..TANDA as u64).fold(FakeDev::disk(GRANDE, BS), |d, b| {
        d.with_block(b + 10, &alloc::vec![(b % 251) as u8; BS as usize])
    });
    let (hw, mut x) = with_disk(dev);
    let id = x.mscs[0].disk_id;

    let mut buf = alloc::vec![0u8; TANDA * BS as usize];
    x.msc_rw(id, 10, MscData::Read(&mut buf))
        .expect("la lectura de doscientos bloques");

    for b in 0..TANDA {
        assert_eq!(
            buf[b * BS as usize],
            (b % 251) as u8,
            "el bloque {} de la tanda ha venido de otro sitio",
            b
        );
    }

    // Y los LBA de las dos vueltas, que es lo que lo dice sin ambigüedad.
    let lbas: Vec<u32> = hw
        .scsi_cdbs(1)
        .into_iter()
        .filter(|c| c.first() == Some(&0x28))
        .map(|c| u32::from_be_bytes([c[2], c[3], c[4], c[5]]))
        .collect();
    assert_eq!(
        lbas,
        alloc::vec![10, 137],
        "las vueltas no avanzan por el disco"
    );
}

/// Una unidad que entrega menos datos de los que la orden pidió y aun así dice
/// que todo fue bien. Darlo por bueno es entregar al sistema de archivos un
/// sector medio lleno de ceros creyéndose datos leídos.
#[test]
fn una_lectura_a_medias_no_se_da_por_buena() {
    let (_hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS).with_short_data(8));
    let id = x.mscs[0].disk_id;

    let mut buf = alloc::vec![0u8; BS as usize];
    assert!(
        x.msc_rw(id, 0, MscData::Read(&mut buf)).is_err(),
        "una lectura que trajo ocho bytes de menos ha salido bien"
    );
}

/// Un `flush` es una orden SCSI como otra, y la que hay que mandar es
/// `SYNCHRONIZE CACHE(10)`: sin ella, lo que el pendrive tenga en su caché se
/// pierde al desenchufarlo.
#[test]
fn un_flush_manda_synchronize_cache() {
    let (hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS));
    let id = x.mscs[0].disk_id;

    x.msc_flush(id).expect("el flush");
    assert!(
        ops(&hw).contains(&0x35),
        "no se ha mandado SYNCHRONIZE CACHE(10): {:?}",
        ops(&hw)
    );
}

/// Y si la unidad se fue, lo que había abierto encima tiene que enterarse:
/// una lectura de una unidad que ya no está es un error, no un buffer de
/// ceros que pasa por datos.
#[test]
fn una_unidad_que_ya_no_esta_no_contesta_lecturas() {
    let (_hw, mut x) = with_disk(FakeDev::disk(BLOCKS, BS));
    let id = x.mscs[0].disk_id;

    let mut buf = alloc::vec![0u8; BS as usize];
    assert!(x.msc_rw(id, 0, MscData::Read(&mut buf)).is_ok());

    assert!(
        x.msc_rw(id + 1, 0, MscData::Read(&mut buf)).is_err(),
        "ha contestado por una unidad que no existe"
    );
}
