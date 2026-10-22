//! Qué clase de dispositivo de entrada decide el driver que hay detrás de una
//! interfaz HID, y el barrido del puerto que llega hasta ahí.
//!
//! Casi todo lo que enchufa un usuario llega con `bInterfaceProtocol` 0, que
//! no dice nada: ni ratón, ni teclado, ni tableta. Lo que decide entonces es
//! el descriptor de informe, y equivocarse ahí no da un error, da un
//! dispositivo mudo: una tableta de más silencia el ratón PS/2 y el de
//! protocolo de arranque (el teclado sigue yendo, el puntero se muere), y un
//! ratón tomado por teclado no mueve nada.

use super::xhci_fake::{FakeDev, Hw};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
/// Velocidad de PORTSC: 3 = High Speed.
const HS: u8 = 3;

/// Un controlador ya arrancado y sin nada enchufado.
fn arrancado() -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    (hw, x)
}

/// Enumera `dev` en el puerto 1 y devuelve el papel de cada interfaz HID que
/// el driver se quedó, en orden.
fn papeles(dev: FakeDev) -> alloc::vec::Vec<u8> {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, dev);
    x.setup_device(DevTopo::root(1), HS).expect("enumera");
    x.hids.iter().map(|h| h.protocol).collect()
}

/// El descriptor de informe de un ratón de verdad: X e Y **relativos**, que
/// es lo que lo distingue de una tableta.
fn informe_de_raton() -> alloc::vec::Vec<u8> {
    FakeDev::mouse().report
}

/// El de un teclado de verdad: los modificadores y el array de teclas.
fn informe_de_teclado() -> alloc::vec::Vec<u8> {
    FakeDev::keyboard().report
}

/// Un puntero **absoluto**: el mismo diseño que un ratón pero con X e Y en
/// un rango físico de 0 a 32767, que es lo que hace de él una tableta.
fn informe_de_tableta() -> alloc::vec::Vec<u8> {
    alloc::vec![
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10,
        0x95, 0x02, 0x81, 0x02, 0xC0, 0xC0,
    ]
}

// --- lo que decide el papel de una interfaz --------------------------------

/// `bInterfaceProtocol` 1 es un teclado de protocolo de arranque y se decide
/// ahí mismo: su diseño es fijo, así que no hay descriptor que pedirle.
#[test]
fn un_teclado_de_protocolo_de_arranque_no_necesita_su_descriptor() {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, FakeDev::keyboard());
    x.setup_device(DevTopo::root(1), HS).expect("enumera");

    assert_eq!(
        x.hids
            .iter()
            .map(|h| h.protocol)
            .collect::<alloc::vec::Vec<_>>(),
        alloc::vec![HID_PROTO_KEY]
    );
    assert!(
        !hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x81 && s.breq == 0x06 && s.wvalue == 0x2200),
        "le ha pedido el descriptor de informe a un teclado de arranque"
    );
}

/// La tableta de una máquina virtual se reconoce por el fabricante con
/// protocolo 0, no por su descriptor: su diseño es conocido y pedírselo es
/// una petición de control de más en cada arranque.
#[test]
fn una_tableta_de_maquina_virtual_se_reconoce_por_su_fabricante() {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, FakeDev::tablet());
    x.setup_device(DevTopo::root(1), HS).expect("enumera");

    assert_eq!(
        x.hids
            .iter()
            .map(|h| h.protocol)
            .collect::<alloc::vec::Vec<_>>(),
        alloc::vec![HID_PROTO_TABLET]
    );
    assert!(
        !hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x81 && s.breq == 0x06 && s.wvalue == 0x2200),
        "le ha pedido el descriptor de informe a una tableta conocida"
    );
}

/// Un ratón de protocolo de arranque **sí** se lo pide: casi todos lo son, y
/// decidirlo por los dos campos de la interfaz dejaba el puntero sin rueda,
/// porque el informe del protocolo de arranque son tres bytes sin ella.
#[test]
fn a_un_raton_de_arranque_se_le_pide_el_descriptor_igual() {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS).expect("enumera");

    assert_eq!(
        x.hids
            .iter()
            .map(|h| h.protocol)
            .collect::<alloc::vec::Vec<_>>(),
        alloc::vec![HID_PROTO_MOUSE]
    );
    assert!(
        hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x81 && s.breq == 0x06 && s.wvalue == 0x2200),
        "no le ha pedido el descriptor de informe"
    );
}

/// Protocolo 0 con un descriptor de puntero relativo es un ratón.
#[test]
fn un_protocolo_cero_con_informe_de_puntero_es_un_raton() {
    assert_eq!(
        papeles(FakeDev::hid(0, 0, 0x1234, 0x5678, informe_de_raton())),
        alloc::vec![HID_PROTO_MOUSE]
    );
}

/// Protocolo 0 con un descriptor de teclado es un teclado, y eso no se puede
/// sacar de los campos de la interfaz: ahí los dos son iguales.
#[test]
fn un_protocolo_cero_con_informe_de_teclado_es_un_teclado() {
    assert_eq!(
        papeles(FakeDev::hid(0, 0, 0x1234, 0x5678, informe_de_teclado())),
        alloc::vec![HID_PROTO_KEY]
    );
}

/// Y con X e Y absolutos es una tableta, aunque el fabricante no sea de los
/// conocidos: un puntero absoluto tratado como relativo deja el cursor
/// pegado a una esquina.
#[test]
fn un_protocolo_cero_con_informe_absoluto_es_una_tableta() {
    assert_eq!(
        papeles(FakeDev::hid(0, 0, 0x1234, 0x5678, informe_de_tableta())),
        alloc::vec![HID_PROTO_TABLET]
    );
}

/// La subclase de arranque con protocolo 0 es un ratón por los campos de la
/// interfaz, pase lo que pase con el descriptor: ése es el caso que no se
/// puede dejar caer en «tableta».
#[test]
fn la_subclase_de_arranque_con_protocolo_cero_es_un_raton() {
    assert_eq!(
        papeles(FakeDev::hid(
            HID_SUBCLASS_BOOT,
            0,
            0x1234,
            0x5678,
            informe_de_tableta()
        )),
        alloc::vec![HID_PROTO_MOUSE],
        "un ratón de arranque se ha clasificado por su descriptor"
    );
}

/// Un dispositivo que declara tener descriptor de informe y lo rechaza no
/// aborta la enumeración, y lo que queda es un **ratón**, nunca una tableta:
/// la tableta silencia el ratón PS/2 y el de protocolo de arranque, así que
/// equivocarse hacia ese lado deja el escritorio sin puntero.
#[test]
fn un_descriptor_que_el_dispositivo_rechaza_sale_como_raton() {
    let mut dev = FakeDev::hid(0, 0, 0x1234, 0x5678, informe_de_raton());
    dev.stall.push((0x81, 0x06));
    assert_eq!(papeles(dev), alloc::vec![HID_PROTO_MOUSE]);
}

/// Y un descriptor que llega todo a ceros es lo mismo que no haber llegado:
/// de ahí no sale ningún diseño, y el papel que queda es ratón.
#[test]
fn un_descriptor_de_ceros_sale_como_raton() {
    let ceros = alloc::vec![0u8; 52];
    assert_eq!(
        papeles(FakeDev::hid(0, 0, 0x1234, 0x5678, ceros)),
        alloc::vec![HID_PROTO_MOUSE]
    );
}

/// Una interfaz de fabricante de más en un dispositivo que ya tiene su ratón
/// de arranque se descarta: quedarse las dos duplica cada movimiento, porque
/// las dos entregan el mismo informe.
#[test]
fn una_interfaz_de_puntero_de_mas_no_se_queda() {
    assert_eq!(
        papeles(FakeDev::boot_mouse_with_vendor_iface()),
        alloc::vec![HID_PROTO_MOUSE],
        "se ha quedado con las dos interfaces de puntero"
    );
}

// --- el barrido de un puerto ----------------------------------------------

/// Un puerto sin nada conectado no se toca: ni reset, ni enumeración.
#[test]
fn un_puerto_vacio_no_se_resetea() {
    let (hw, mut x) = arrancado();
    let antes = hw.portsc(1);
    let escrituras = hw.port_writes(1);

    x.try_port_hid(1).expect("el barrido no falla");

    assert_eq!(
        hw.port_writes(1),
        escrituras,
        "ha escrito en PORTSC de un puerto vacío"
    );
    assert_eq!(hw.portsc(1), antes, "ha cambiado PORTSC de un puerto vacío");
    assert!(x.devs.is_empty(), "ha enumerado algo que no estaba");
}

/// Un puerto con el dispositivo ya enumerado se limpia primero: dejar el
/// slot de antes y pedir otro es quedarse con dos slots para un aparato.
#[test]
fn un_puerto_que_ya_tiene_slot_se_limpia_antes_de_enumerar_otra_vez() {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, FakeDev::mouse());
    x.try_port_hid(1).expect("el primer barrido va");
    assert_eq!(x.devs.len(), 1, "el primer barrido no ha enumerado");

    x.try_port_hid(1).expect("el segundo barrido va");

    assert_eq!(x.devs.len(), 1, "ha dejado dos slots para un aparato");
    assert_eq!(x.hids.len(), 1, "ha dejado dos interfaces de puntero");
}

/// Un puerto que el controlador declara sin alimentación se enciende antes de
/// resetearlo: un reset sobre un puerto apagado no levanta el enlace y el
/// dispositivo no aparece.
#[test]
fn un_puerto_sin_alimentacion_se_enciende_antes_del_reset() {
    let (hw, mut x) = arrancado();
    hw.plug(1, HS, FakeDev::mouse());
    // PP es el bit 9. El fake lo deja puesto, como un controlador normal.
    hw.set_portsc(1, hw.portsc(1) & !(1 << 9));

    x.try_port_hid(1).expect("el barrido va");

    assert_ne!(hw.portsc(1) & (1 << 9), 0, "ha dejado el puerto apagado");
    assert_eq!(x.devs.len(), 1, "no ha enumerado con el puerto apagado");
}

/// Un dispositivo que no contesta las primeras peticiones se reintenta: el
/// primer intento falla, el puerto se limpia, se resetea otra vez y la
/// segunda vuelta enumera. Un pendrive que tarda en despertar depende de
/// esto.
#[test]
fn una_enumeracion_que_falla_se_reintenta_desde_el_reset() {
    let (hw, mut x) = arrancado();
    let mut dev = FakeDev::mouse();
    // Suficientes peticiones sin contestar para tumbar el primer intento,
    // pero no el segundo.
    dev.mute_first = 1;
    hw.plug(1, HS, dev);

    x.try_port_hid(1).expect("el barrido acaba bien");

    // El descriptor de dispositivo es la primera petición de cada intento:
    // pedido dos veces es el reintento, pedido una es que no hizo falta.
    let intentos = hw
        .requests(1)
        .iter()
        .filter(|s| s.bmrt == 0x80 && s.breq == 0x06 && s.wvalue == 0x0100)
        .count();
    assert!(intentos >= 2, "no ha reintentado: {} intento", intentos);
    // Un reintento es desde el principio: el puerto se resetea otra vez.
    // Volver a pedirle descriptores al mismo dispositivo sin resetearlo es
    // dejarlo en el estado en el que ya falló una vez.
    assert!(
        hw.port_resets(1) >= 2,
        "sólo ha reseteado el puerto {} vez",
        hw.port_resets(1)
    );
    // Y el slot del intento fallido se devuelve: 10 es Disable Slot.
    assert!(
        hw.commands().iter().any(|c| c.ty == 10),
        "no ha soltado el slot del intento que falló"
    );
    assert_eq!(x.devs.len(), 1, "no ha vuelto a intentarlo");
    assert_eq!(
        x.hids
            .iter()
            .map(|h| h.protocol)
            .collect::<alloc::vec::Vec<_>>(),
        alloc::vec![HID_PROTO_MOUSE]
    );
}
