//! La recuperación del transporte Bulk-Only y los reintentos por sentido.
//!
//! `msc_tests` prueba el camino cuando todo va bien: el CBW sale, la fase de
//! datos va, el CSW vuelve. Esto prueba el camino cuando NO va, que es el que
//! un pendrive de verdad toma a la primera y el que ninguna unidad emulada
//! toma nunca:
//!
//! - La primera orden tras un enchufe se rechaza con `UNIT ATTENTION`. Es el
//!   caso normal, no un disco roto, y llegaba al sistema de archivos como un
//!   error de E/S: el «I/O error» al montar nacía ahí.
//! - El endpoint IN se para antes de entregar el CSW. Linux pide el CSW dos
//!   veces por esto (`usb_stor_Bulk_transport`); pedirlo una sola vez
//!   convierte un tropiezo en un fallo permanente.
//! - Una orden que se queda sin CSW, o cuyo CSW no casa el tag, deja a la
//!   unidad con una respuesta pendiente. Sin el `Bulk-Only Mass Storage
//!   Reset` la orden siguiente se la encuentra y falla, y la de después
//!   igual, hasta desenchufar.

use super::xhci_fake::{FakeDev, Hw};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;
const BLOCKS: u64 = 64;
const BS: u32 = 512;

/// `bmRequestType` y `bRequest` del Bulk-Only Mass Storage Reset: del host al
/// dispositivo, de clase, a la interfaz (BOT §3.1).
const RESET_BMRT: u8 = 0x21;
const RESET_BREQ: u8 = 0xff;

/// Un controlador arrancado con una unidad ya enumerada en el puerto 1.
fn with_disk() -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    // Las esperas de «no lista» son de milisegundos y no leen registros.
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::disk(BLOCKS, BS));
    x.setup_device(DevTopo::root(1), HS)
        .expect("la unidad enumera");
    (hw, x)
}

fn disk_id(x: &XhciInner) -> u64 {
    x.mscs.first().expect("la unidad").disk_id
}

/// Los códigos de orden que recibió la unidad, en orden.
fn ops(hw: &Hw) -> Vec<u8> {
    hw.scsi_cdbs(1)
        .iter()
        .filter_map(|c| c.first().copied())
        .collect()
}

/// Cuántas veces se pidió el reset del transporte.
fn resets(hw: &Hw) -> usize {
    hw.requests(1)
        .iter()
        .filter(|s| s.bmrt == RESET_BMRT && s.breq == RESET_BREQ)
        .count()
}

// --- los reintentos por sentido -------------------------------------------

/// El caso del encargo. Un pendrive declara su condición de atención a la
/// primera orden que le llega después de enchufarlo: la descarta, contesta
/// «ha fallado», y la siguiente ya va. La condición se limpia al preguntar el
/// sentido, que es justo lo que no se preguntaba.
///
/// Sin el reintento esto era un `IoError` que subía hasta el sistema de
/// archivos como EIO, con la unidad perfectamente sana detrás.
#[test]
fn una_unit_attention_no_es_un_error_de_lectura() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    let before = ops(&hw).len();
    hw.arm_unit_attention(1, 1);

    let mut buf = [0u8; BS as usize];
    x.msc_rw(id, 0, MscData::Read(&mut buf))
        .expect("la lectura tiene que salir adelante al segundo intento");

    // Y el camino que tomó: la lectura rechazada, el sentido que limpia la
    // condición, y la lectura otra vez. Ese orden es la prueba de que el
    // reintento no es un reintento a ciegas.
    assert_eq!(
        &ops(&hw)[before..],
        &[SCSI_READ_10, SCSI_REQUEST_SENSE, SCSI_READ_10],
        "falta el REQUEST SENSE entre las dos lecturas, o falta la segunda"
    );
    // Una condición de atención no es un fallo del transporte: resetearlo
    // costaría una vuelta entera de la unidad por algo que ya está resuelto.
    assert_eq!(resets(&hw), 0, "no hacía falta resetear el transporte");
}

/// Lo mismo en una escritura: lo que no puede pasar es que la orden se dé por
/// hecha. Un `Ok` con la condición de atención sin resolver dejaría el sector
/// sin escribir y al sistema de archivos creyendo que está en el medio.
#[test]
fn una_unit_attention_tampoco_pierde_una_escritura() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    hw.arm_unit_attention(1, 1);

    let payload = alloc::vec![0x5a; BS as usize];
    x.msc_rw(id, 3, MscData::Write(&payload))
        .expect("la escritura tiene que salir adelante");
    assert_eq!(
        hw.media_block(1, 3),
        payload,
        "el bloque no se escribió: la orden rechazada se dio por hecha"
    );
}

/// El reintento tiene tope. Una unidad que repite la condición no puede dejar
/// la lectura girando para siempre dentro del cerrojo del controlador.
#[test]
fn una_unit_attention_que_no_se_va_acaba_en_error() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    hw.arm_unit_attention(1, 16);

    let mut buf = [0u8; BS as usize];
    assert_eq!(
        x.msc_rw(id, 0, MscData::Read(&mut buf)),
        Err(DeviceError::IoError),
        "una condición que no se va tiene que acabar en error"
    );
    let reads = ops(&hw).iter().filter(|&&o| o == SCSI_READ_10).count();
    assert_eq!(
        reads,
        (MSC_RW_SENSE_RETRIES as usize) + 1,
        "el número de intentos no es el que dice MSC_RW_SENSE_RETRIES"
    );
}

/// Y el sentido decide. Un error de medio no se arregla preguntando otra vez:
/// repetirlo sólo retrasa el error que el llamante tiene que ver.
#[test]
fn un_error_de_medio_no_se_reintenta() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    let before = ops(&hw).len();
    hw.arm_medium_error(1, 16);

    let mut buf = [0u8; BS as usize];
    assert_eq!(
        x.msc_rw(id, 0, MscData::Read(&mut buf)),
        Err(DeviceError::IoError),
        "un error de medio es un error"
    );
    assert_eq!(
        &ops(&hw)[before..],
        &[SCSI_READ_10, SCSI_REQUEST_SENSE],
        "un error de medio no se vuelve a pedir"
    );
}

// --- el CSW que no llega a la primera -------------------------------------

/// El reintento que hace Linux. Una unidad que para su endpoint IN antes de
/// entregar el CSW contesta bien al segundo intento, y el CSW de verdad sigue
/// esperando detrás del STALL.
#[test]
fn un_csw_parado_se_vuelve_a_pedir() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    hw.arm_csw_stall(1, 1, 1);

    let mut buf = [0u8; BS as usize];
    x.msc_rw(id, 1, MscData::Read(&mut buf))
        .expect("el CSW está detrás del STALL; hay que volver a pedirlo");
    // Al segundo intento la orden salió adelante, así que el transporte no
    // estaba desincronizado y resetearlo habría sido tirar una vuelta.
    assert_eq!(resets(&hw), 0, "no hacía falta resetear el transporte");
}

/// Un CSW que no llega en ningún intento sí deja a la unidad a media orden:
/// tiene una respuesta pendiente que la orden siguiente se encontraría. Eso es
/// lo que arregla el reset del transporte, y es la diferencia entre una
/// lectura fallida y un disco que no vuelve hasta desenchufarlo.
#[test]
fn un_csw_que_nunca_llega_resetea_el_transporte() {
    let (hw, mut x) = with_disk();
    let id = disk_id(&x);
    hw.arm_csw_stall(1, MSC_CSW_ATTEMPTS as u32, 1);

    let mut buf = [0u8; BS as usize];
    assert_eq!(
        x.msc_rw(id, 0, MscData::Read(&mut buf)),
        Err(DeviceError::IoError),
        "sin CSW la lectura no se puede dar por buena"
    );
    assert_eq!(
        resets(&hw),
        1,
        "la orden se quedó sin CSW y nadie reseteó el transporte"
    );
}

/// Un CSW con el tag de otra orden es la firma de un transporte desincronizado:
/// lo que se está leyendo es la respuesta de la orden anterior. Creérselo sería
/// leer el resultado equivocado; quedarse quieto deja el desfase puesto para
/// siempre.
#[test]
fn un_csw_de_otro_tag_resetea_el_transporte() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::disk(BLOCKS, BS).with_wrong_csw_tag());
    // La identificación ya falla, y es ahí donde se ve: cada orden que no casa
    // el tag tiene que dejar el transporte reseteado detrás.
    let _ = x.setup_device(DevTopo::root(1), HS);

    assert!(
        resets(&hw) > 0,
        "ningún CSW casó el tag y el transporte no se reseteó ni una vez"
    );
}

// --- lo que el reset hace, exactamente ------------------------------------

/// La secuencia del BOT §5.3.4, en orden y entera: la petición de clase a la
/// interfaz y después los dos `CLEAR_FEATURE(ENDPOINT_HALT)`.
///
/// Los dos halts hacen falta aunque el fallo fuese de uno solo: el reset de la
/// unidad le devuelve los endpoints a su estado inicial, y el host tiene que
/// hacer lo propio en los dos o el que quede parado se lleva la orden
/// siguiente.
#[test]
fn el_reset_del_transporte_manda_la_peticion_y_los_dos_halts() {
    let (hw, mut x) = with_disk();
    let (dci_in, dci_out) = {
        let m = x.mscs.first().expect("la unidad");
        (m.dci_in, m.dci_out)
    };
    let before = hw.requests(1).len();
    x.msc_reset_recovery(0);

    let after: Vec<(u8, u8, u16)> = hw
        .requests(1)
        .into_iter()
        .skip(before)
        .map(|s| (s.bmrt, s.breq, s.windex))
        .collect();
    assert_eq!(
        after.first(),
        Some(&(RESET_BMRT, RESET_BREQ, 0)),
        "la primera tiene que ser el reset de clase a la interfaz 0"
    );
    // CLEAR_FEATURE(ENDPOINT_HALT) es bmRequestType 0x02, bRequest 0x01, y
    // wIndex la DIRECCIÓN del endpoint, no su DCI.
    let halted: Vec<u16> = after
        .iter()
        .filter(|(bmrt, breq, _)| *bmrt == 0x02 && *breq == 0x01)
        .map(|(_, _, windex)| *windex)
        .collect();
    assert_eq!(after.len(), 3, "sobran peticiones: {after:?}");
    let want: Vec<u16> = [dci_in, dci_out]
        .iter()
        .map(|dci| ep_addr_from_dci(*dci))
        .collect();
    assert_eq!(
        halted, want,
        "faltan halts por quitar, o van en otro orden que el del BOT"
    );
}

/// Y una unidad que ya no está no se resetea: el índice que traía la orden
/// puede apuntar a una ranura que se vació al desenchufar, y eso tiene que ser
/// un no-op y no un pánico.
#[test]
fn resetear_una_unidad_que_ya_no_esta_no_hace_nada() {
    let (hw, mut x) = with_disk();
    let before = hw.requests(1).len();
    x.msc_reset_recovery(7);
    assert_eq!(
        hw.requests(1).len(),
        before,
        "no hay unidad 7: no se le puede haber pedido nada"
    );
}
