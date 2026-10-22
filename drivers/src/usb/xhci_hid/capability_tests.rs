//! La cara que el controlador le enseña a libinput, y el informe cuando no
//! se puede mirar.
//!
//! Lo que `capability` no declara se **descarta antes de llegar al
//! compositor**, así que un bit de menos ahí no da un error en ningún sitio:
//! da una rueda que no gira o un puntero que no se mueve, que es la forma del
//! #1401. `hid_capability` ya está probada aparte, con sus combinaciones; lo
//! que se prueba aquí es el envoltorio, o sea que el nodo le pasa los hechos
//! del controlador que tiene y no otros, y el rango absoluto que lo acompaña.

extern crate std;

use super::test_turns::AbsPointerTurn;
use super::xhci_fake::{FakeDev, Hw};
use super::*;
use crate::scheme::input::{AbsInfo, CapabilityType};
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

/// Un controlador arrancado con lo que se le enchufe en el puerto 1. El
/// torniquete va porque enumerar una tableta pone un global del proceso.
fn with(dev: Option<FakeDev>) -> (AbsPointerTurn, Hw, Arc<XhciUsbHid>) {
    let turn = AbsPointerTurn::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    if let Some(d) = dev {
        hw.plug(1, HS, d);
        x.setup_device(DevTopo::root(1), HS)
            .expect("el aparato enumera");
    }
    (turn, hw, ctrl(x))
}

fn codigos(dev: &Arc<XhciUsbHid>, t: CapabilityType, codes: &[u16]) -> alloc::vec::Vec<u16> {
    let cap = dev.capability(t);
    codes.iter().copied().filter(|c| cap.contains(*c)).collect()
}

// --- lo que declara un raton ----------------------------------------------

/// Un ratón enchufado tiene que declarar los dos tipos de evento que manda,
/// los dos ejes relativos y **las cuatro** ruedas: libinput sólo sintetiza
/// los eventos de alta resolución para un dispositivo que NO declara el eje,
/// así que declarar uno sin el otro es la combinación que rompe el desplazar
/// del todo.
#[test]
fn un_raton_declara_sus_eventos_sus_ejes_y_las_cuatro_ruedas() {
    let (_t, _hw, dev) = with(Some(FakeDev::mouse()));

    assert_eq!(
        codigos(&dev, CapabilityType::Event, &[EV_KEY, EV_REL, EV_ABS]),
        alloc::vec![EV_KEY, EV_REL],
        "los tipos de evento de un ratón"
    );
    assert_eq!(
        codigos(
            &dev,
            CapabilityType::RelAxis,
            &[
                REL_X,
                REL_Y,
                REL_WHEEL,
                REL_HWHEEL,
                REL_WHEEL_HI_RES,
                REL_HWHEEL_HI_RES
            ]
        )
        .len(),
        6,
        "le falta un eje relativo o una de las cuatro ruedas"
    );
    assert!(
        dev.capability(CapabilityType::Key).contains(BTN_LEFT),
        "no declara el botón izquierdo"
    );
}

/// Y no declara nada absoluto: ni el eje, ni su rango. Un nodo que dice tener
/// ejes absolutos y no manda ninguno deja a libinput esperándolos.
#[test]
fn un_raton_no_declara_ejes_absolutos_ni_su_rango() {
    let (_t, _hw, dev) = with(Some(FakeDev::mouse()));

    assert!(
        codigos(&dev, CapabilityType::AbsAxis, &[ABS_X, ABS_Y]).is_empty(),
        "un ratón declara ejes absolutos"
    );
    assert!(dev.abs_info(ABS_X).is_none());
    assert!(dev.abs_info(ABS_Y).is_none());
}

// --- lo que declara una tableta -------------------------------------------

/// Una tableta de máquina virtual declara los ejes absolutos, su propiedad de
/// puntero y el rango que de verdad manda: sin el rango, el compositor
/// escala la posición contra un máximo inventado y el cursor acaba en una
/// esquina.
#[test]
fn una_tableta_declara_sus_ejes_absolutos_su_rango_y_que_es_un_puntero() {
    let (_t, _hw, dev) = with(Some(FakeDev::tablet()));

    assert_eq!(
        codigos(&dev, CapabilityType::AbsAxis, &[ABS_X, ABS_Y]),
        alloc::vec![ABS_X, ABS_Y]
    );
    assert!(
        dev.capability(CapabilityType::InputProp)
            .contains(INPUT_PROP_POINTER),
        "no se declara como puntero"
    );
    // `AbsInfo` no es comparable, así que se mira el rango que es lo que el
    // compositor usa para escalar.
    let esperado = AbsInfo::range(0, TABLET_RANGE);
    for axis in [ABS_X, ABS_Y] {
        let info = dev.abs_info(axis).expect("un eje absoluto sin rango");
        assert_eq!(
            (info.minimum, info.maximum),
            (esperado.minimum, esperado.maximum),
            "el rango del eje {:#x}",
            axis
        );
    }
}

/// Y le quita al nodo los ejes relativos, que es lo único que los calla: el
/// mismo nodo entrega los dos y declarar los dos a la vez es lo que hacía que
/// el puntero diera saltos.
#[test]
fn una_tableta_de_maquina_virtual_le_quita_los_ejes_relativos() {
    let (_t, _hw, dev) = with(Some(FakeDev::tablet()));

    assert!(
        codigos(&dev, CapabilityType::RelAxis, &[REL_X, REL_Y]).is_empty(),
        "declara ejes relativos con una tableta de máquina virtual delante"
    );
}

/// El rango absoluto es de los ejes X e Y y de ningún otro: contestar un
/// rango para un eje que el dispositivo no manda es describirle a libinput un
/// dispositivo que no existe.
#[test]
fn el_rango_absoluto_es_solo_de_los_dos_ejes_del_puntero() {
    let (_t, _hw, dev) = with(Some(FakeDev::tablet()));

    assert!(dev.abs_info(ABS_Z).is_none());
    assert!(dev.abs_info(ABS_PRESSURE).is_none());
}

/// Un controlador sin nada enchufado declara los ejes relativos **igual**, y
/// a propósito: libinput lee las capacidades UNA vez, al abrir el nodo, y en
/// hardware de verdad el compositor lo abre antes de que el ratón acabe de
/// enumerar. Un nodo que en ese momento dice no tener puntero se queda sin
/// él toda la sesión.
#[test]
fn un_controlador_vacio_declara_el_puntero_relativo_de_todas_formas() {
    let (_t, _hw, dev) = with(None);

    assert_eq!(
        codigos(&dev, CapabilityType::RelAxis, &[REL_X, REL_Y]),
        alloc::vec![REL_X, REL_Y],
        "un nodo sin ratón enumerado todavía no declara el puntero"
    );
    assert!(
        dev.capability(CapabilityType::Event).contains(EV_REL),
        "declara los ejes relativos sin declarar EV_REL, que es como se caen"
    );
}

/// Lo que sí es que no declara nada absoluto: de eso no hay ninguno, y un eje
/// absoluto que el nodo no manda deja a libinput esperándolo.
#[test]
fn un_controlador_vacio_no_declara_ningun_eje_absoluto() {
    let (_t, _hw, dev) = with(None);

    assert!(codigos(&dev, CapabilityType::AbsAxis, &[ABS_X, ABS_Y]).is_empty());
    assert!(!dev.capability(CapabilityType::Event).contains(EV_ABS));
    assert!(dev.abs_info(ABS_X).is_none());
}

// --- el informe cuando no se puede mirar ----------------------------------

/// `/proc/usbhid` de un controlador que no llegó a arrancar lo dice con todas
/// las letras: un informe en blanco manda a buscar el fallo a otro sitio.
#[test]
fn el_informe_de_un_controlador_sin_arrancar_lo_dice() {
    let dev = Arc::new(XhciUsbHid {
        listener: EventListener::new(),
        inner: Mutex::new(None),
        msi_vector: 0,
        halted: AtomicBool::new(false),
        has_rel_mouse_flag: AtomicBool::new(false),
        has_tablet_flag: AtomicBool::new(false),
        ack_op_base: AtomicUsize::new(0),
        ack_rt_base: AtomicUsize::new(0),
    });

    let r = dev.debug_report();

    assert!(
        r.contains("not initialised"),
        "el informe no dice que el controlador no arrancó: {}",
        r
    );
}

/// Y un `cat` que llega mientras el controlador está ocupado contesta eso,
/// **sin esperar**: bloquear aquí es cómo un `cat` acaba con el cerrojo del
/// controlador en la mano mientras una interrupción lo espera.
///
/// El informe se pide desde otro hilo con el cerrojo tomado aquí, porque
/// «no espera» sólo se puede afirmar si esperar se nota: pedirlo en este
/// mismo hilo con un `lock()` en vez de un `try_lock()` se queda colgado
/// para siempre y un test colgado no es un veredicto.
#[test]
fn el_informe_no_se_queda_esperando_el_cerrojo_del_controlador() {
    let (_t, _hw, dev) = with(Some(FakeDev::mouse()));
    let guard = dev.inner.lock();

    let otro = dev.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(otro.debug_report());
    });

    let r = rx
        .recv_timeout(core::time::Duration::from_secs(5))
        .expect("el informe se quedó esperando el cerrojo del controlador");
    assert!(
        r.contains("busy"),
        "el informe no dice que el controlador está ocupado: {}",
        r
    );
    drop(guard);
}

/// Un dispositivo con más interfaces de las que el driver apunta tiene que
/// decir cuántas se quedaron fuera: sin esa cuenta, un informe recortado se
/// lee como el dispositivo entero y la interfaz que falta se busca en el
/// sitio equivocado.
#[test]
fn el_informe_cuenta_las_interfaces_que_no_caben() {
    let (_t, _hw, dev) = with(Some(FakeDev::hid_many_ifaces(
        MAX_IFACES_RECORDED as u8 + 3,
    )));

    let r = dev.debug_report();

    assert!(
        r.contains("+3 mas"),
        "el informe no cuenta las interfaces que no caben: {}",
        r
    );
}
