//! El puntero **absoluto**: las tabletas de máquina virtual, que es con lo
//! que se mueve el ratón del escritorio cuando se arranca con `make qemu`.
//!
//! Un informe absoluto no se parece en nada a uno relativo: trae la posición
//! (0 a 32767 en los dos ejes) en vez de un desplazamiento, y los dos
//! emuladores que importan la colocan en sitios distintos del informe. QEMU
//! manda `[botones, X16, Y16, rueda, pan]` y VirtualBox
//! `[botones, dz, dw, relleno, X16, Y16]`; tomar uno por el otro deja el
//! puntero clavado en una esquina, que es el síntoma que los distingue.
//!
//! Y encima de eso está la bandera global que dice que hay un puntero
//! absoluto: lo que apaga el ratón PS/2 y los informes relativos de HID, que
//! si no pelearían con la tableta por el mismo cursor (saltos, movimiento
//! doble). Estos tests toman turno en `AbsPointerTurn` por eso.

use super::xhci_fake::{FakeDev, Hw};
use super::*;
use alloc::boxed::Box;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// Un controlador arrancado con ese aparato en el puerto 1.
fn with(dev: FakeDev) -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, dev);
    x.setup_device(DevTopo::root(1), HS)
        .expect("la tableta enumera");
    (hw, x)
}

fn listener() -> (EventListener<InputEvent>, Arc<Mutex<Vec<(u16, u16, i32)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let lis = EventListener::<InputEvent>::new();
    let sink = seen.clone();
    lis.subscribe(
        Box::new(move |e: &InputEvent| sink.lock().push((e.event_type as u16, e.code, e.value))),
        false,
    );
    (lis, seen)
}

/// Los ejes absolutos que salieron, en orden.
fn axes(seen: &Arc<Mutex<Vec<(u16, u16, i32)>>>) -> Vec<(u16, i32)> {
    seen.lock()
        .iter()
        .filter(|(t, _, _)| *t == EV_ABS)
        .map(|(_, c, v)| (*c, *v))
        .collect()
}

/// Las teclas y botones que salieron, en orden.
fn keys(seen: &Arc<Mutex<Vec<(u16, u16, i32)>>>) -> Vec<(u16, i32)> {
    seen.lock()
        .iter()
        .filter(|(t, _, _)| *t == EV_KEY)
        .map(|(_, c, v)| (*c, *v))
        .collect()
}

/// Entrega un informe por el endpoint de la tableta y lo procesa.
fn report(hw: &Hw, x: &mut XhciInner, lis: &EventListener<InputEvent>, data: &[u8]) {
    let dci = x.hids[0].ep_dci;
    hw.send_in(1, dci, data);
    x.process_irq_events(Some(lis), false);
}

// --- la identificación ----------------------------------------------------

/// Una tableta de máquina virtual se reconoce por el fabricante y su
/// `bInterfaceProtocol` 0, **sin pedirle el descriptor de informe**: su
/// diseño es conocido, y pedírselo a QEMU no aporta nada.
#[test]
fn una_tableta_de_maquina_virtual_se_reconoce_sin_pedirle_el_descriptor() {
    let _abs = AbsPointerTurn::new();
    let (hw, x) = with(FakeDev::tablet());

    assert_eq!(x.hids.len(), 1, "la tableta no se ha armado");
    assert_eq!(
        x.hids[0].protocol, HID_PROTO_TABLET,
        "no se ha clasificado como puntero absoluto"
    );
    assert!(
        !hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x81 && s.breq == 0x06 && (s.wvalue >> 8) == 0x22),
        "se le ha pedido el descriptor de informe: {:?}",
        hw.requests(1)
    );
}

/// Y levanta la bandera que apaga el ratón PS/2 y los informes relativos de
/// HID. Sin ella, los dos punteros pelean por el mismo cursor y el resultado
/// son saltos y movimiento doble.
#[test]
fn una_tableta_enumerada_levanta_la_bandera_del_puntero_absoluto() {
    let _abs = AbsPointerTurn::new();
    let (_hw, _x) = with(FakeDev::tablet());

    assert!(
        usb_abs_pointer_active(),
        "la bandera del puntero absoluto no se ha levantado"
    );
}

/// Con la tableta mandando, un ratón relativo en el mismo controlador se
/// calla: es el mismo cursor, y dos fuentes de movimiento sobre él es lo que
/// se nota como salto.
#[test]
fn con_una_tableta_delante_los_informes_relativos_se_callan() {
    let _abs = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::tablet());
    hw.plug(2, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS).expect("la tableta");
    x.setup_device(DevTopo::root(2), HS).expect("el ratón");
    let (lis, seen) = listener();

    let raton = x
        .hids
        .iter()
        .find(|h| h.protocol == HID_PROTO_MOUSE)
        .expect("el ratón se ha armado");
    let dci = raton.ep_dci;
    let slot = raton.slot_id;
    let puerto = if slot == 1 { 1 } else { 2 };
    hw.send_in(puerto, dci, &[0x00, 5, 5, 0]);
    x.process_irq_events(Some(&lis), false);

    assert!(
        seen.lock().iter().all(|(t, _, _)| *t != EV_REL),
        "el ratón relativo ha movido el cursor con la tableta delante: {:?}",
        seen.lock().clone()
    );
}

// --- los informes de QEMU -------------------------------------------------

/// El informe de QEMU: la posición va en los bytes 1..5, little endian, y
/// sale tal cual por los ejes absolutos. `libinput` mapea ese rango al
/// tamaño de la pantalla con `EVIOCGABS`.
#[test]
fn el_informe_de_qemu_trae_la_posicion_en_los_ejes_absolutos() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::tablet());
    let (lis, seen) = listener();

    // X = 0x1234, Y = 0x5678.
    report(&hw, &mut x, &lis, &[0x00, 0x34, 0x12, 0x78, 0x56, 0x00]);

    assert_eq!(
        axes(&seen),
        alloc::vec![(ABS_X, 0x1234), (ABS_Y, 0x5678)],
        "la posición absoluta no ha salido"
    );
    assert!(
        seen.lock()
            .iter()
            .any(|(t, c, _)| *t == EV_SYN && *c == SYN_REPORT),
        "el informe no se ha cerrado con un SYN"
    );
}

/// Los cinco botones de la tableta, con su flanco: un botón que se mantiene
/// pulsado entre dos informes no vuelve a salir, y uno que se suelta sale
/// con valor 0. Sin el flanco, el escritorio ve una pulsación por informe,
/// que a 100 Hz son cien clics por segundo.
#[test]
fn los_botones_de_la_tableta_salen_por_flanco_y_no_por_informe() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::tablet());
    let (lis, seen) = listener();

    report(&hw, &mut x, &lis, &[0x01, 0, 0, 0, 0, 0]);
    report(&hw, &mut x, &lis, &[0x01, 0x10, 0, 0, 0, 0]);
    report(&hw, &mut x, &lis, &[0x00, 0x10, 0, 0, 0, 0]);

    assert_eq!(
        keys(&seen),
        alloc::vec![(BTN_LEFT, 1), (BTN_LEFT, 0)],
        "los botones no han salido por flanco"
    );
}

/// Y los cinco se distinguen: el lateral y el extra son los que un ratón de
/// escritorio usa para atrás y adelante.
#[test]
fn los_cinco_botones_de_la_tableta_se_distinguen() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::tablet());
    let (lis, seen) = listener();

    report(&hw, &mut x, &lis, &[0x1f, 0, 0, 0, 0, 0]);

    assert_eq!(
        keys(&seen),
        alloc::vec![
            (BTN_LEFT, 1),
            (BTN_RIGHT, 1),
            (BTN_MIDDLE, 1),
            (BTN_SIDE, 1),
            (BTN_EXTRA, 1)
        ],
        "no salen los cinco botones"
    );
}

/// La rueda de la tableta va en el sexto byte, con signo, y sale como
/// desplazamiento. Es el único campo del informe que puede faltar: un
/// informe de seis bytes no trae el pan.
#[test]
fn la_rueda_de_la_tableta_sale_como_desplazamiento() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::tablet());
    let (lis, seen) = listener();

    // Rueda = -1, que es un clic hacia abajo.
    report(&hw, &mut x, &lis, &[0x00, 0, 0, 0, 0, 0xff]);

    assert!(
        !seen.lock().is_empty(),
        "el informe con rueda no ha salido en absoluto"
    );
    assert_eq!(
        x.hids[0].last_wheel,
        (-1, 0),
        "la rueda no se ha contado: {:?}",
        x.hids[0].last_wheel
    );
    assert_eq!(x.hids[0].wheel_count, 1, "no se ha contado el giro");
}

/// Un informe más corto que la posición no se decodifica: con cinco bytes,
/// la Y se leería medio fuera del informe y el puntero se iría a una
/// esquina.
#[test]
fn un_informe_mas_corto_que_la_posicion_no_se_decodifica() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::tablet());
    let (lis, seen) = listener();

    report(&hw, &mut x, &lis, &[0x00, 0x34, 0x12, 0x78]);

    assert!(
        axes(&seen).is_empty(),
        "ha decodificado un informe corto: {:?}",
        axes(&seen)
    );
}

// --- el informe de VirtualBox ---------------------------------------------

/// VirtualBox pone la posición **cuatro bytes más adelante**, con la rueda y
/// el pan delante. Decodificarlo con el diseño de QEMU leería la rueda como
/// la X y dejaría el puntero clavado arriba a la izquierda.
#[test]
fn el_informe_de_virtualbox_lleva_la_posicion_cuatro_bytes_mas_adelante() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::vbox_tablet());
    let (lis, seen) = listener();

    assert!(
        x.hids[0].vbox_tablet,
        "no se ha reconocido la de VirtualBox"
    );

    // [botones, dz, dw, relleno, X16, Y16] con X = 0x0abc, Y = 0x7654.
    report(
        &hw,
        &mut x,
        &lis,
        &[0x00, 0x00, 0x00, 0x00, 0xbc, 0x0a, 0x54, 0x76],
    );

    assert_eq!(
        axes(&seen),
        alloc::vec![(ABS_X, 0x0abc), (ABS_Y, 0x7654)],
        "la posición de VirtualBox no ha salido donde está"
    );
}

/// Y su rueda va en los bytes 1 y 2, que en el informe de QEMU son la X.
#[test]
fn la_rueda_de_virtualbox_va_delante_de_la_posicion() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::vbox_tablet());
    let (lis, seen) = listener();

    report(
        &hw,
        &mut x,
        &lis,
        &[0x00, 0xff, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
    );

    assert_eq!(
        x.hids[0].last_wheel,
        (-1, 1),
        "la rueda y el pan de VirtualBox no están donde VirtualBox los pone"
    );
    assert_eq!(
        axes(&seen),
        alloc::vec![(ABS_X, 0), (ABS_Y, 0)],
        "la rueda se ha leído como posición"
    );
}

/// El informe de VirtualBox necesita ocho bytes enteros: con siete, la Y se
/// leería a medias. El de QEMU sí vale con seis, y por eso el mínimo no
/// puede ser el mismo para los dos.
#[test]
fn el_informe_de_virtualbox_necesita_sus_ocho_bytes() {
    let _abs = AbsPointerTurn::new();
    let (hw, mut x) = with(FakeDev::vbox_tablet());
    let (lis, seen) = listener();

    report(&hw, &mut x, &lis, &[0x00, 0, 0, 0, 0xbc, 0x0a, 0x54]);

    assert!(
        axes(&seen).is_empty(),
        "ha decodificado un informe de siete bytes: {:?}",
        axes(&seen)
    );
}
