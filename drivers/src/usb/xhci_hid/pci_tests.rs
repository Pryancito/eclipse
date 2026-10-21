//! El enganche con el bus: de un dispositivo PCI al controlador en marcha.
//!
//! Es el camino por el que el driver existe —lo recorre el sondeo del bus al
//! arrancar— y era el único trozo grande del fichero que ningún test
//! llamaba, porque pide un `PCIDevice` y no sólo un controlador. Aquí la
//! traducción de física a virtual es la identidad, así que la ventana de
//! registros del fake vale tal cual como dirección de BAR y `init` acaba
//! hablando con el controlador falso de verdad.
//!
//! Lo que se decide en este camino y no se puede decidir en otro sitio: si
//! este dispositivo es nuestro, si su BAR sirve, qué vector de interrupción
//! le toca, y que un pendrive que ya estuviera enchufado al encender se
//! publique —que es lo que hace que arrancar con el disco puesto se vea igual
//! que enchufarlo después.

use super::xhci_fake::{FakeDev, Hw};
use super::*;
use pci::{
    CSpaceAccessMethod, Command, DeviceDetails, DeviceKind, Identifier, Location, Prefetchable,
    Status, Type, BAR,
};

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// Un dispositivo PCI con esa clase y ese BAR.
fn device(class: u8, subclass: u8, prog_if: u8, bar: Option<BAR>) -> PCIDevice {
    PCIDevice {
        loc: Location {
            bus: 0,
            device: 20,
            function: 0,
        },
        id: Identifier {
            vendor_id: 0x8086,
            device_id: 0x1e31,
            revision_id: 0,
            prog_if,
            class,
            subclass,
        },
        command: Command::empty(),
        status: Status::empty(),
        cache_line_size: 0,
        latency_timer: 0,
        multifunction: false,
        bist_capable: false,
        bars: [bar, None, None, None, None, None],
        kind: DeviceKind::Device(DeviceDetails {
            cardbus_cis_ptr: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
            expansion_rom_base_addr: 0,
            min_grant: 0,
            max_latency: 0,
        }),
        pic_interrupt_line: 0,
        interrupt_pin: None,
        cspace_access_method: CSpaceAccessMethod::IO,
        capabilities: None,
    }
}

/// Un xHCI con su ventana de registros en el fake.
fn xhci_en(hw: &Hw) -> PCIDevice {
    device(
        0x0c,
        0x03,
        0x30,
        Some(BAR::Memory(
            hw.base() as u64,
            0x2000,
            Prefetchable::No,
            Type::Bits64,
        )),
    )
}

// --- a quién reconoce el driver -------------------------------------------

/// El filtro por fabricante es el primer tamiz que aplica el sondeo del bus,
/// y lo único que descarta es el 0xffff: ése es el valor que devuelve el
/// espacio de configuración cuando **no hay nadie** en esa ranura, así que
/// aceptarlo sería intentar arrancar un controlador que no existe. El resto
/// lo decide `matched_dev` por clase.
#[test]
fn el_filtro_por_fabricante_solo_descarta_la_ranura_vacia() {
    let drv = XhciDriverPci;

    assert_eq!(drv.name(), "xhci");
    assert!(drv.matched(0x8086, 0x1e31), "no reconoce un xHCI de Intel");
    assert!(drv.matched(0x1033, 0x0194), "no reconoce un xHCI de NEC");
    assert!(
        !drv.matched(0xffff, 0xffff),
        "ha reconocido una ranura vacía"
    );
}

/// El driver se queda con los controladores USB y con nada más: la clase
/// 0x0c subclase 0x03 es «controlador de bus serie / USB», y el sondeo del
/// bus le ofrece todo lo que encuentra.
#[test]
fn el_driver_reconoce_los_controladores_usb_y_nada_mas() {
    let drv = XhciDriverPci;
    let hw = Hw::new(SLOTS, PORTS);

    assert!(drv.matched_dev(&xhci_en(&hw)), "no reconoce un xHCI");
    assert!(
        !drv.matched_dev(&device(0x0c, 0x05, 0x00, None)),
        "ha reconocido un controlador SMBus"
    );
    assert!(
        !drv.matched_dev(&device(0x02, 0x00, 0x00, None)),
        "ha reconocido una tarjeta de red"
    );
    assert!(
        !drv.matched_dev(&device(0x01, 0x08, 0x02, None)),
        "ha reconocido un NVMe"
    );
    // La subclase sola no basta: 0x03 dentro de la clase 0x04 es un aparato
    // de telefonía, y mirar una mitad del par se lo llevaría por delante.
    assert!(
        !drv.matched_dev(&device(0x04, 0x03, 0x00, None)),
        "ha reconocido un aparato de telefonía"
    );
}

/// Un USB que no es xHCI (los antiguos: EHCI, OHCI, UHCI) se reconoce como
/// USB pero no se arranca: sin la feature de los antiguos no hay nada que
/// ponerle delante, y decirlo es mejor que quedarse con el dispositivo.
#[test]
fn un_controlador_usb_antiguo_se_reconoce_pero_no_se_arranca() {
    let drv = XhciDriverPci;
    let hw = Hw::new(SLOTS, PORTS);
    let mut dev = xhci_en(&hw);
    dev.id.prog_if = 0x20; // EHCI

    assert!(drv.matched_dev(&dev), "un EHCI también es USB");
    assert_eq!(
        drv.init(&dev, &None, None).err(),
        Some(DeviceError::NotSupported),
        "ha arrancado un controlador que no está implementado"
    );
}

// --- el BAR ---------------------------------------------------------------

/// Un xHCI habla por memoria, no por puertos: un BAR de entrada/salida no
/// sirve, y seguir adelante con él leería registros de donde no están.
#[test]
fn un_bar_de_puertos_de_entrada_salida_no_sirve() {
    let drv = XhciDriverPci;

    assert_eq!(
        drv.init(
            &device(0x0c, 0x03, 0x30, Some(BAR::IO(0x3000, 0x20))),
            &None,
            None
        )
        .err(),
        Some(DeviceError::NotSupported),
        "ha aceptado un BAR de puertos"
    );
    assert_eq!(
        drv.init(&device(0x0c, 0x03, 0x30, None), &None, None).err(),
        Some(DeviceError::NotSupported),
        "ha aceptado un dispositivo sin BAR"
    );
}

/// Y un BAR a cero es un BAR que el firmware no asignó: no hay ventana que
/// mapear.
#[test]
fn un_bar_sin_asignar_no_sirve() {
    let drv = XhciDriverPci;

    assert_eq!(
        drv.init(
            &device(
                0x0c,
                0x03,
                0x30,
                Some(BAR::Memory(0, 0x2000, Prefetchable::No, Type::Bits64))
            ),
            &None,
            None
        )
        .err(),
        Some(DeviceError::NotSupported),
        "ha aceptado un BAR sin asignar"
    );
}

// --- el arranque ----------------------------------------------------------

/// El camino entero: de un dispositivo PCI a un controlador en marcha, con
/// lo que estuviera enchufado ya enumerado. La enumeración ocurre **en el
/// arranque** a propósito: el escritorio sondea `/dev/input/event*` una vez
/// al arrancar, y si la enumeración se dejara para un sondeo posterior se
/// quedaría con la entrada muerta.
#[test]
fn un_xhci_con_un_raton_enchufado_arranca_con_el_raton_ya_armado() {
    let _poll = PollTurn::new();
    let _abs = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.plug(1, HS, FakeDev::mouse());

    let dev = XhciUsbHid::probe(&xhci_en(&hw), hw.base(), 0x2000, NO_MSI_VECTOR)
        .expect("el controlador arranca");

    let g = dev.inner.lock();
    let xi = g.as_ref().expect("el controlador quedó inicializado");
    assert_eq!(xi.hids.len(), 1, "el ratón no se enumeró en el arranque");
    assert!(
        !xi.boot_enum_pending,
        "la enumeración de arranque se quedó pendiente"
    );
    assert!(hw.running(), "el controlador no se puso en marcha");
}

/// Un controlador que dice no tener ningún slot no se puede usar: todo lo de
/// después (el array de contextos de dispositivo, los anillos) se dimensiona
/// con ese número.
#[test]
fn un_controlador_que_dice_no_tener_slots_no_se_arranca() {
    let _poll = PollTurn::new();
    let hw = Hw::new(0, PORTS);

    assert_eq!(
        XhciUsbHid::probe(&xhci_en(&hw), hw.base(), 0x2000, NO_MSI_VECTOR).err(),
        Some(DeviceError::InvalidParam),
        "ha arrancado un controlador sin slots"
    );
}

/// El vector de MSI que le toca es el índice de la interrupción **más 32**:
/// los 32 primeros son las excepciones del procesador, y entregarle el 1 a
/// secas sería entregarle la de depuración.
#[test]
fn el_vector_de_msi_es_el_indice_de_la_interrupcion_mas_32() {
    let _poll = PollTurn::new();
    let msi = MsiTurn::new();
    let _abs = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let drv = XhciDriverPci;

    let dev = drv
        .init(&xhci_en(&hw), &None, Some(5))
        .expect("el controlador arranca");

    match dev {
        Device::Input(i) => assert_eq!(i.name(), "xhci-usb-hid"),
        _ => panic!("no ha salido un dispositivo de entrada"),
    }
    // El MSI queda apuntado para enlazarlo cuando el host de interrupciones
    // exista: en el arranque todavía no está.
    assert!(
        msi.vectores().contains(&37),
        "el vector no se apuntó: {:?}",
        msi.vectores()
    );
}

/// Sin interrupción asignada el controlador arranca igual y no apunta
/// ningún MSI: lo drena el sondeo del timer, que es lo que pasa en las
/// máquinas donde el firmware no reparte MSI.
#[test]
fn sin_interrupcion_asignada_no_se_apunta_ningun_msi() {
    let _poll = PollTurn::new();
    let msi = MsiTurn::new();
    let _abs = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);

    XhciDriverPci
        .init(&xhci_en(&hw), &None, None)
        .expect("el controlador arranca");

    assert!(
        msi.vectores().is_empty(),
        "ha apuntado un MSI sin interrupción asignada: {:?}",
        msi.vectores()
    );
}

/// Y el controlador arrancado queda en la lista del sondeo: ése es el
/// respaldo para los MSI perdidos, y sin apuntarse ahí un controlador sin
/// interrupción no entregaría nada nunca.
#[test]
fn el_controlador_arrancado_queda_apuntado_para_el_sondeo() {
    let _poll = PollTurn::new();
    let _abs = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.plug(1, HS, FakeDev::mouse());

    let dev = XhciUsbHid::probe(&xhci_en(&hw), hw.base(), 0x2000, NO_MSI_VECTOR)
        .expect("el controlador arranca");
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x00, 3, 0, 0]);
    poll();

    assert_eq!(
        dev.inner.lock().as_ref().expect("el controlador").hids[0].report_count,
        1,
        "el sondeo no llegó a este controlador: no quedó apuntado"
    );
}

/// Un pendrive que ya estuviera enchufado al encender se publica en el
/// arranque. La enumeración corre antes de que exista la `Arc` del
/// controlador, así que el disco se queda apuntado y sin publicar; que el
/// arranque vacíe esa cola es lo que hace que encender con el disco puesto se
/// vea igual que enchufarlo después.
#[test]
fn un_pendrive_ya_enchufado_al_encender_se_publica_en_el_arranque() {
    let _poll = PollTurn::new();
    let _turno = crate::hotplug::TEST_TURNSTILE.lock();
    crate::hotplug::clear_sink();
    crate::hotplug::set_sink(cuenta_alta, cuenta_baja);
    ALTAS.store(0, Ordering::SeqCst);
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.plug(1, HS, FakeDev::disk(64, 512));

    let dev = XhciUsbHid::probe(&xhci_en(&hw), hw.base(), 0x2000, NO_MSI_VECTOR)
        .expect("el controlador arranca");

    assert_eq!(
        ALTAS.load(Ordering::SeqCst),
        1,
        "el pendrive que ya estaba enchufado no se publicó"
    );
    assert!(
        dev.inner.lock().as_ref().expect("el controlador").mscs[0]
            .disk
            .is_some(),
        "el disco no quedó anotado en la unidad"
    );
    crate::hotplug::clear_sink();
}

static ALTAS: AtomicUsize = AtomicUsize::new(0);

fn cuenta_alta(_dev: Device) {
    ALTAS.fetch_add(1, Ordering::SeqCst);
}

fn cuenta_baja(_dev: &Device) -> bool {
    false
}
