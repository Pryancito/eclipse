//! La enumeración de un dispositivo: lo que pasa entre «hay algo en el puerto
//! 1» y «hay un ratón».
//!
//! La tanda anterior le puso al fake un controlador; ésta le pone un
//! *dispositivo* ([`super::xhci_fake::FakeDev`]), que es lo que le faltaba a
//! `setup_device` para poder llamarse: hasta aquí los comandos se contestaban
//! pero EP0 no, así que la función se quedaba en el primer GET_DESCRIPTOR.

use super::xhci_fake::{FakeDev, Hw, Setup};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;

/// Velocidad de PORTSC: 3 = High Speed.
const HS: u8 = 3;
/// 1 = Full Speed, que es donde EP0 empieza a 8 bytes.
const FS: u8 = 1;

/// Un controlador ya arrancado, con un ratón en el puerto 1.
fn with_mouse(speed: u8) -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, speed, FakeDev::mouse());
    (hw, x)
}

/// La enumeración entera, que son cinco peticiones de control en un orden que
/// no es negociable: sin el descriptor de dispositivo no se sabe el tamaño de
/// EP0, y sin la cabecera de la configuración no se sabe cuánto pedir de ella.
#[test]
fn un_raton_enchufado_se_enumera_entero() {
    let (hw, mut x) = with_mouse(HS);
    x.setup_device(DevTopo::root(1), HS)
        .expect("el ratón enumera");

    let reqs = hw.requests(1);
    assert_eq!(
        reqs.first().copied(),
        Some(Setup {
            bmrt: 0x80,
            breq: 0x06,
            wvalue: 0x0100,
            windex: 0,
            wlen: 18,
        }),
        "la primera petición no es el descriptor de dispositivo: {:?}",
        reqs
    );
    // La cabecera de la configuración se pide a 9 bytes y sólo después
    // entera: el total vive en los bytes 2 y 3 de esa cabecera.
    let cfg: Vec<u16> = reqs
        .iter()
        .filter(|s| s.bmrt == 0x80 && s.breq == 0x06 && s.wvalue == 0x0200)
        .map(|s| s.wlen)
        .collect();
    assert_eq!(
        cfg,
        alloc::vec![9, 34],
        "la configuración no se ha pedido primero por su cabecera: {:?}",
        reqs
    );
    assert!(
        reqs.iter()
            .any(|s| s.bmrt == 0x00 && s.breq == 0x09 && s.wvalue == 1),
        "no se ha configurado el dispositivo: {:?}",
        reqs
    );
    assert!(
        reqs.iter()
            .any(|s| s.bmrt == 0x81 && s.breq == 0x06 && s.wvalue == 0x2200),
        "no se ha pedido el descriptor de informe: {:?}",
        reqs
    );

    // Y lo que el driver se queda de todo eso.
    let dev = x.devs.iter().find(|d| d.slot == 1).expect("un dispositivo");
    assert_eq!((dev.vid, dev.pid), (0x046d, 0xc077), "VID/PID mal leídos");
    assert_eq!(dev.class, 0, "la clase sale del descriptor de dispositivo");
    assert_eq!(dev.ifaces.len(), 1, "la interfaz HID no se ha apuntado");
    assert_eq!(dev.ifaces[0].class, USB_CLASS_HID);

    // Y el endpoint de interrupción, que es lo que de verdad trae los
    // movimientos: EP1 IN es el DCI 3 (2*1 + dirección).
    assert_eq!(x.hids.len(), 1, "el endpoint HID no se ha armado");
    assert_eq!(
        x.hids[0].ep_dci, 3,
        "DCI mal calculado desde bEndpointAddress"
    );
    assert_eq!(x.hids[0].slot_id, 1);
    assert_eq!(x.hids[0].iface, 0);
    assert_ne!(x.hids[0].protocol, 0, "el ratón no se ha clasificado");
}

/// EP0 arranca a ocho bytes en plena velocidad porque es lo único que el
/// estándar garantiza antes de leer el descriptor; cuando el descriptor dice
/// 64, hay que decírselo al controlador o las transferencias siguientes se
/// parten en paquetes de ocho.
#[test]
fn un_ep0_mas_grande_que_el_de_arranque_se_le_dice_al_controlador() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, FS, FakeDev::mouse().with_mps0(64));

    x.setup_device(DevTopo::root(1), FS)
        .expect("el ratón enumera");

    assert_eq!(
        hw.eval_mps(),
        alloc::vec![64],
        "el Evaluate Context no lleva el tamaño del descriptor: {:?}",
        hw.commands()
    );
}

/// Y al revés: un dispositivo que ya cabía no gasta un comando. Un Evaluate
/// Context de más no es gratis, va por el anillo de comandos como todo.
#[test]
fn un_ep0_que_ya_era_del_tamano_bueno_no_gasta_comando() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, FS, FakeDev::mouse().with_mps0(8));

    x.setup_device(DevTopo::root(1), FS)
        .expect("el ratón enumera");

    assert!(
        hw.eval_mps().is_empty(),
        "se ha emitido un Evaluate Context que no hacía falta: {:?}",
        hw.eval_mps()
    );
}

/// Un aparato que no contesta por EP0 no se queda a medio enumerar: ni entra
/// en la lista de dispositivos ni deja el slot ocupado.
#[test]
fn un_dispositivo_que_no_contesta_no_entra_en_la_lista() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(10_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::mouse().mute());

    assert!(
        x.setup_device(DevTopo::root(1), HS).is_err(),
        "un dispositivo que no contesta ha enumerado"
    );
    assert!(
        x.devs.is_empty(),
        "ha quedado un dispositivo a medias: {:?}",
        x.devs.iter().map(|d| d.slot).collect::<Vec<_>>()
    );
    assert_eq!(
        hw.requests(1).len(),
        1,
        "se ha seguido pidiendo después del plazo: {:?}",
        hw.requests(1)
    );
}

/// Un dispositivo que tarda en despertar: el primer intento se va a su plazo
/// y el reintero de `try_port_hid` lo coge. Es el caso por el que ese
/// reintento existe, y sin él un ratón lento no aparece.
#[test]
fn un_dispositivo_lento_lo_coge_el_reintento_del_puerto() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(10_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::mouse().mute_for(1));

    x.try_port_hid(1).expect("el puerto acaba enumerando");

    assert_eq!(
        x.devs.len(),
        1,
        "el reintento no ha llegado a enumerar nada"
    );
    let enables = hw.commands().iter().filter(|c| c.ty == 9).count();
    assert_eq!(enables, 2, "no se ha reintentado: {} Enable Slot", enables);
}

/// Un puerto vacío no toca nada: ni un comando, ni un slot.
#[test]
fn un_puerto_vacio_no_enumera_nada() {
    let (hw, mut x) = with_mouse(HS);
    let antes = hw.commands().len();

    x.try_port_hid(2).expect("un puerto vacío no es un error");

    assert_eq!(
        hw.commands().len(),
        antes,
        "un puerto vacío ha emitido comandos: {:?}",
        hw.commands()
    );
    assert!(x.devs.is_empty(), "un puerto vacío ha dado un dispositivo");
}

/// Una velocidad que el controlador no sabe traducir a un tamaño de EP0 se
/// rechaza *antes* de direccionar el dispositivo: con un EP0 de tamaño
/// inventado ninguna transferencia cuadra.
#[test]
fn una_velocidad_desconocida_no_llega_a_direccionar() {
    let (hw, mut x) = with_mouse(HS);

    assert!(
        x.setup_device(DevTopo::root(1), 9).is_err(),
        "una velocidad que no existe ha enumerado"
    );
    assert!(
        !hw.commands().iter().any(|c| c.ty == 11),
        "se ha direccionado un dispositivo de velocidad desconocida: {:?}",
        hw.commands()
    );
}

/// El slot viene de un evento del controlador, así que puede venir fuera de
/// rango: las tablas del driver están dimensionadas a `max_slots`, y
/// indexarlas con lo que diga el evento es un pánico del kernel.
#[test]
fn un_slot_fuera_de_rango_se_rechaza_antes_de_usarlo() {
    let (hw, mut x) = with_mouse(HS);
    hw.set_next_slot(SLOTS + 1);

    assert!(
        x.setup_device(DevTopo::root(1), HS).is_err(),
        "un slot fuera de rango ha enumerado"
    );
    assert!(
        !hw.commands().iter().any(|c| c.ty == 11),
        "se ha direccionado un slot que no existe: {:?}",
        hw.commands()
    );
}

/// Una cabecera de configuración que dice un total imposible se rechaza: ese
/// total es lo que decide cuánta memoria se pide y cuántos bytes se recorren
/// después.
#[test]
fn un_total_de_configuracion_imposible_aborta_la_enumeracion() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    // wTotalLength = 4, menos que la propia cabecera.
    let cfg = alloc::vec![0x09, 0x02, 0x04, 0x00, 0x01, 0x01, 0x00, 0xa0, 0x32];
    hw.plug(1, HS, FakeDev::mouse().with_config(cfg));

    assert!(
        x.setup_device(DevTopo::root(1), HS).is_err(),
        "una configuración imposible ha enumerado"
    );
    assert!(
        !hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x00 && s.breq == 0x09),
        "se ha configurado un dispositivo con una configuración ilegible"
    );
}

/// Las interfaces se apuntan todas, no sólo las que este driver sabe usar: un
/// dispositivo cuya segunda interfaz es de una clase desconocida tiene que
/// verse igual desde fuera.
#[test]
fn se_apuntan_tambien_las_interfaces_de_clases_que_no_se_manejan() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let mut cfg = alloc::vec![
        0x09, 0x02, 0x00, 0x00, 0x02, 0x01, 0x00, 0xa0, 0x32, // 2 interfaces
        0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, // iface 0: HID
        0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0a, // EP1 IN interrupción
        0x09, 0x04, 0x01, 0x00, 0x00, 0xfe, 0x01, 0x01, 0x00, // iface 1: clase 0xfe
    ];
    let total = cfg.len() as u16;
    cfg[2..4].copy_from_slice(&total.to_le_bytes());
    hw.plug(1, HS, FakeDev::mouse().with_config(cfg));

    x.setup_device(DevTopo::root(1), HS)
        .expect("el dispositivo enumera");

    let dev = x.devs.iter().find(|d| d.slot == 1).expect("un dispositivo");
    let clases: Vec<u8> = dev.ifaces.iter().map(|i| i.class).collect();
    assert_eq!(
        clases,
        alloc::vec![USB_CLASS_HID, 0xfe],
        "no se han apuntado las dos interfaces"
    );
    // Y SET_CONFIGURATION lleva bConfigurationValue (byte 5), no el número de
    // interfaces (byte 4), que aquí ya no valen lo mismo.
    let cfg_val = hw
        .requests(1)
        .iter()
        .find(|s| s.bmrt == 0x00 && s.breq == 0x09)
        .map(|s| s.wvalue);
    assert_eq!(
        cfg_val,
        Some(1),
        "SET_CONFIGURATION no lleva el valor de la configuración"
    );
}

/// Un dispositivo que contesta menos de lo que se le pidió termina con Short
/// Packet, que en una transferencia de control es un éxito: los primeros ocho
/// bytes del descriptor de dispositivo son lo que muchos aparatos dan a la
/// primera, y traen ya el tamaño de EP0.
#[test]
fn un_descriptor_mas_corto_de_lo_pedido_no_es_un_fallo() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    // Sólo hasta bMaxPacketSize0; de VID y PID, nada.
    let corto = alloc::vec![0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40];
    hw.plug(1, HS, FakeDev::mouse().with_device(corto));

    x.setup_device(DevTopo::root(1), HS)
        .expect("un Short Packet en control es un éxito");

    let dev = x.devs.iter().find(|d| d.slot == 1).expect("un dispositivo");
    assert_eq!(
        (dev.vid, dev.pid),
        (0, 0),
        "lo que el dispositivo no mandó no puede haberse leído"
    );
    assert_eq!(x.hids.len(), 1, "el endpoint HID se arma igual");
}

/// Un STALL es la forma que tiene un dispositivo de decir «no sé qué me
/// pides», y un ratón de los baratos rechaza más de una: ni las opcionales de
/// HID ni el propio descriptor de informe pueden tumbar la enumeración, que
/// para eso está la disposición de arranque.
#[test]
fn un_stall_no_tumba_la_enumeracion() {
    let hw = Hw::new(SLOTS, PORTS);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    // SET_IDLE y SET_PROTOCOL, las dos que el driver manda sin necesitarlas,
    // y el descriptor de informe, que sí usa.
    hw.plug(
        1,
        HS,
        FakeDev::mouse()
            .stalling(0x21, 0x0a)
            .stalling(0x21, 0x0b)
            .stalling(0x81, 0x06),
    );

    x.setup_device(DevTopo::root(1), HS)
        .expect("un STALL no es un fallo");

    assert_eq!(x.devs.len(), 1, "el dispositivo no se ha quedado");
    assert_eq!(
        x.hids.len(),
        1,
        "sin descriptor de informe el ratón se queda sin endpoint"
    );
}

/// Un puerto que dice tener algo conectado pero nunca negocia una velocidad
/// no se enumera: el tamaño de EP0 sale de la velocidad, y sin ella el
/// dispositivo se direccionaría con un contexto inventado.
#[test]
fn un_puerto_que_no_entrena_no_se_enumera() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(10_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let antes = hw.commands().len();
    // Conectado, sí; con velocidad, no.
    hw.connect(1, 0);

    x.try_port_hid(1)
        .expect("un puerto que no entrena no es un error");

    assert_eq!(
        hw.commands().len(),
        antes,
        "se ha intentado direccionar un dispositivo sin velocidad: {:?}",
        hw.commands()
    );
    assert!(x.devs.is_empty(), "ha salido un dispositivo de la nada");
}
