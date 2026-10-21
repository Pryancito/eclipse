//! `/proc/usbhid`: el informe que contesta «¿ve el sistema lo que acabo de
//! enchufar?».
//!
//! Es el único sitio donde se mira cuando algo no aparece, y lo que de
//! verdad se pega en un informe de fallo. Un informe que se calla una línea
//! manda a buscar el problema donde no está: `disk=-` y `disk=usbdisk0` son
//! la diferencia entre «el alta no se hizo» y «el problema está en quien
//! monta», y una interfaz sin armar que no saliera se confundiría con un
//! dispositivo que no enumeró.

use super::xhci_fake::{FakeDev, Hw};
use super::*;
use crate::scheme::InputScheme;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

fn ctrl(x: XhciInner) -> Arc<XhciUsbHid> {
    let arc = Arc::new(XhciUsbHid {
        listener: EventListener::new(),
        inner: Mutex::new(Some(x)),
        msi_vector: 0,
        halted: AtomicBool::new(false),
        has_rel_mouse_flag: AtomicBool::new(false),
        has_tablet_flag: AtomicBool::new(false),
        ack_op_base: AtomicUsize::new(0),
        ack_rt_base: AtomicUsize::new(0),
    });
    if let Some(xi) = arc.inner.lock().as_ref() {
        arc.refresh_role_flags(xi);
    }
    arc
}

/// Un controlador arrancado con lo que se le enchufe en el puerto 1.
fn with(dev: Option<FakeDev>) -> (Hw, Arc<XhciUsbHid>) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    if let Some(d) = dev {
        hw.plug(1, HS, d);
        x.setup_device(DevTopo::root(1), HS)
            .expect("el aparato enumera");
    }
    (hw, ctrl(x))
}

// --- los tres casos en los que no hay nada que contar ---------------------

/// Un controlador vacío tiene que decirlo con todas las letras. Un informe en
/// blanco no se distingue de un informe que no se generó, y el que lo lee se
/// va a buscar el fallo a otro sitio.
#[test]
fn un_controlador_sin_nada_enchufado_lo_dice() {
    let (_hw, dev) = with(None);

    let r = dev.debug_report();

    assert!(
        r.contains("no USB devices enumerated"),
        "no dice que no hay dispositivos: {}",
        r
    );
    assert!(
        r.contains("no HID interfaces bound"),
        "no dice que no hay interfaces armadas: {}",
        r
    );
}

/// Un controlador que no llegó a inicializarse es un caso distinto de uno
/// vacío: ahí el fallo está en el arranque del propio controlador, no en lo
/// que se enchufó.
#[test]
fn un_controlador_sin_inicializar_se_distingue_de_uno_vacio() {
    let (_hw, dev) = with(None);
    *dev.inner.lock() = None;

    let r = dev.debug_report();

    assert!(
        r.contains("controller not initialised"),
        "no dice que no está inicializado: {}",
        r
    );
}

/// El informe es un diagnóstico, y pedir el cerrojo aquí es cómo un `cat`
/// acaba teniéndolo cogido mientras una interrupción espera. Con el
/// controlador ocupado contesta eso y se va.
#[test]
fn con_el_controlador_ocupado_el_informe_no_espera() {
    let (_hw, dev) = with(Some(FakeDev::mouse()));
    let _ocupado = dev.inner.lock();

    let r = dev.debug_report();

    assert!(
        r.contains("controller busy"),
        "se quedó esperando el cerrojo o no lo dijo: {}",
        r
    );
}

// --- lo que sale de cada cosa enchufada -----------------------------------

/// Un ratón: la línea del dispositivo con su clase traducida y su sitio en el
/// árbol, y la de la interfaz armada con su papel. `bound=1` es lo que separa
/// «el sistema lo ve» de «el sistema lo usa».
#[test]
fn un_raton_sale_con_su_clase_su_sitio_y_su_papel() {
    let (_hw, dev) = with(Some(FakeDev::mouse()));

    let r = dev.debug_report();

    assert!(
        r.contains("046d:c077"),
        "no sale el identificador del ratón: {}",
        r
    );
    assert!(r.contains("root_port=1"), "no sale el puerto raíz: {}", r);
    assert!(r.contains("(hid)"), "no traduce la clase HID: {}", r);
    assert!(r.contains("bound=1"), "no dice que está armado: {}", r);
    assert!(
        r.contains("role=mouse(rel)"),
        "no dice el papel del ratón: {}",
        r
    );
    assert!(
        r.contains("has_rel_mouse=true"),
        "el resumen sin cerrojo no se publicó: {}",
        r
    );
}

/// Un teclado se distingue del ratón por el papel, que es lo que decide por
/// dónde salen sus informes.
#[test]
fn un_teclado_sale_con_su_papel_de_teclado() {
    let (_hw, dev) = with(Some(FakeDev::keyboard()));

    let r = dev.debug_report();

    assert!(
        r.contains("role=key"),
        "no dice el papel del teclado: {}",
        r
    );
    assert!(
        r.contains("has_rel_mouse=false"),
        "un teclado no es un ratón relativo: {}",
        r
    );
}

/// El último informe que llegó, en hexadecimal, y la cuenta de los que han
/// llegado. Sin esto, un aparato que está armado y no manda nada no se
/// distingue de uno que manda informes que no se entienden.
#[test]
fn el_ultimo_informe_que_llego_sale_en_hexadecimal() {
    let (hw, dev) = with(Some(FakeDev::mouse()));
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x01, 0x05, 0xfb, 0x00]);
    let lis = EventListener::<InputEvent>::new();
    dev.inner
        .lock()
        .as_mut()
        .expect("el controlador")
        .process_irq_events(Some(&lis), false);

    let r = dev.debug_report();

    assert!(r.contains("reports=1"), "no cuenta el informe: {}", r);
    assert!(
        r.contains("last=[01 05 fb 00]"),
        "no sale el informe en hexadecimal: {}",
        r
    );
}

/// Un hub: sus puertos y cuántos slots cuelgan de él. Sin esta línea, un
/// teclado detrás de un hub que no enumera no se distingue de un hub que
/// nunca se configuró.
#[test]
fn un_hub_sale_con_sus_puertos_y_sus_hijos() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(4));
    hw.plug_behind(1, 2, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");
    let dev = ctrl(x);

    let r = dev.debug_report();

    assert!(
        r.contains("hub slot=1") && r.contains("ports=4"),
        "no sale el hub con sus puertos: {}",
        r
    );
    assert!(
        r.contains("children=1"),
        "no cuenta el ratón que cuelga del hub: {}",
        r
    );
    assert!(r.contains("(hub)"), "no traduce la clase hub: {}", r);
    // Las interfaces armadas se cuentan POR DISPOSITIVO: el ratón tiene la
    // suya y el hub no tiene ninguna. Con una cuenta global los dos saldrían
    // iguales y la línea dejaría de contestar cuál de los dos usa el sistema.
    assert_eq!(
        r.matches("bound=1").count(),
        1,
        "la cuenta de interfaces armadas no es por dispositivo: {}",
        r
    );
    assert_eq!(
        r.matches("bound=0").count(),
        1,
        "el hub tendría que salir sin interfaces armadas: {}",
        r
    );
}

/// Con dos hubs, cada uno cuenta **los suyos**. Una cuenta que mire sólo si
/// un dispositivo tiene padre le atribuye a cada hub los hijos del otro, y
/// entonces la línea deja de servir para saber por dónde cuelga lo que no
/// aparece.
#[test]
fn con_dos_hubs_cada_uno_cuenta_sus_propios_hijos() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(4));
    hw.plug_behind(1, 2, HS, FakeDev::mouse());
    hw.plug(2, HS, FakeDev::hub(4));
    hw.plug_behind(2, 3, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el primer hub enumera");
    x.setup_device(DevTopo::root(2), HS)
        .expect("el segundo hub enumera");
    let dev = ctrl(x);

    let r = dev.debug_report();

    assert_eq!(
        r.matches("hub slot=").count(),
        2,
        "no salen los dos hubs: {}",
        r
    );
    assert_eq!(
        r.matches("children=1").count(),
        2,
        "los hijos no se cuentan por hub: {}",
        r
    );
}

/// Un pendrive, que es la línea que contesta «sí, el sistema ve tu
/// pendrive»: su nombre de catálogo, sus unidades lógicas y su tamaño.
#[test]
fn un_pendrive_sale_con_su_nombre_y_su_tamano() {
    let (_hw, dev) = with(Some(FakeDev::disk(64, 512)));

    let r = dev.debug_report();

    assert!(r.contains("msc slot=1"), "no sale la unidad: {}", r);
    assert!(
        r.contains("sectors512=64") && r.contains("block_size=512"),
        "no sale su tamaño: {}",
        r
    );
    assert!(
        r.contains("luns=1"),
        "no sale la cuenta de unidades lógicas: {}",
        r
    );
    assert!(
        r.contains("(almacenamiento)"),
        "no traduce la clase de almacenamiento: {}",
        r
    );
}

/// Lo PRIMERO que hay que mirar cuando una unidad sale aquí y en `/dev` no
/// hay nada: `disk=-` es que el alta no se hizo, y `disk=<nombre>` es que el
/// dispositivo de bloque existe y el problema está más arriba, en quien
/// monta. Los dos tienen que salir, y distintos.
#[test]
fn una_unidad_sin_dar_de_alta_sale_con_guion_y_con_disco_con_su_nombre() {
    let (_hw, dev) = with(Some(FakeDev::disk(64, 512)));

    assert!(
        dev.debug_report().contains("disk=-"),
        "sin alta tendría que salir con guión: {}",
        dev.debug_report()
    );

    // Lo que hace el alta: el controlador se queda la `Arc` del disco.
    let disco = Arc::new(UsbDisk {
        ctrl: Arc::downgrade(&dev),
        disk_id: dev.inner.lock().as_ref().expect("el controlador").mscs[0].disk_id,
        name: alloc::string::String::from("usbdisk7"),
        dev_blocks: 64,
        dev_block_size: 512,
    });
    dev.inner.lock().as_mut().expect("el controlador").mscs[0].disk = Some(disco);

    assert!(
        dev.debug_report().contains("disk=usbdisk7"),
        "con el disco dado de alta tendría que salir su nombre: {}",
        dev.debug_report()
    );
}

/// Una unidad que habló pero no tiene medio dentro: `cap=none` es distinto de
/// no estar, y es la respuesta a «el lector de tarjetas sale pero no hay
/// disco».
#[test]
fn una_unidad_sin_medio_sale_con_cap_none() {
    let (_hw, dev) = with(Some(FakeDev::disk(64, 512).with_not_ready(u32::MAX)));

    let r = dev.debug_report();

    assert!(r.contains("msc slot=1"), "no sale la unidad: {}", r);
    assert!(
        r.contains("cap=none"),
        "no dice que no tiene capacidad: {}",
        r
    );
}

/// Un dispositivo que enumera y cuya interfaz no se arma tiene que salir
/// igual, con `bound=0`: a la pregunta de si el sistema lo ve, esa línea es
/// la respuesta, y sin ella se confunde con uno que no enumeró.
#[test]
fn un_dispositivo_sin_interfaz_armada_sale_con_bound_cero() {
    let (_hw, dev) = with(Some(FakeDev::disk(64, 512)));

    let r = dev.debug_report();

    assert!(r.contains("bound=0"), "no sale con bound=0: {}", r);
    assert!(
        r.contains("no HID interfaces bound"),
        "un pendrive no arma interfaces HID: {}",
        r
    );
}

// --- la tabla de clases ---------------------------------------------------

/// Los números de clase los traduce esta tabla, y es lo que hace legible el
/// informe. Una clase que no está en la tabla sale como `?`, no en blanco: el
/// hueco se leería como un fallo del informe.
#[test]
fn las_clases_de_usb_se_nombran_y_las_que_no_estan_salen_con_interrogacion() {
    // La tabla entera, porque un número mal puesto en una fila sale como el
    // nombre de la clase de al lado y eso manda a leer el informe al revés.
    let tabla = [
        (0x00u8, "por interfaz"),
        (0x01, "audio"),
        (0x02, "cdc"),
        (USB_CLASS_HID, "hid"),
        (0x05, "fisico"),
        (0x06, "imagen"),
        (0x07, "impresora"),
        (0x08, "almacenamiento"),
        (USB_CLASS_HUB, "hub"),
        (0x0a, "datos-cdc"),
        (0x0b, "tarjeta-chip"),
        (0x0d, "seguridad"),
        (0x0e, "video"),
        (0x0f, "salud"),
        (0x10, "audio-video"),
        (0x11, "pantalla"),
        (0xdc, "diagnostico"),
        (0xe0, "inalambrico"),
        (0xef, "varios"),
        (0xfe, "especifico"),
        (0xff, "del fabricante"),
    ];
    for (clase, nombre) in tabla {
        assert_eq!(usb_class_name(clase), nombre, "la clase {:#04x}", clase);
    }
    assert_eq!(usb_class_name(USB_CLASS_HID), "hid");
    assert_eq!(usb_class_name(0x42), "?");
    assert_eq!(usb_class_name(0x0c), "?", "0x0c no está asignada");
}
