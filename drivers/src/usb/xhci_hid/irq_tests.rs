//! El camino de la interrupción: qué se confirma, con el cerrojo y sin él, y
//! el handoff del BIOS que lo precede.
//!
//! Una interrupción de MSI que no se confirma no se vuelve a entregar: el
//! EINT de USBSTS y el IP del interruptor son pegajosos, así que dejarlos
//! puestos es dejar el teclado muerto hasta que el sondeo de respaldo pase
//! por ahí. Y el manejador corre en contexto de interrupción, donde el
//! cerrojo de `inner` puede estar cogido por un `cat /proc/usbhid` en esta
//! misma CPU: coger el cerrojo a lo bruto ahí es un deadlock contra alguien
//! que no puede correr hasta que volvamos. De eso sale el camino sin cerrojo,
//! que es el que estos tests fijan.
//!
//! Y de paso lo que pasa cuando un slot se libera y el controlador no lo
//! suelta: ahí la memoria se abandona a propósito, porque devolverla es
//! dejarle un puntero vivo a algo ya reutilizado.

use super::xhci_fake::{FakeDev, Hw, BAR_SIZE, RTSOFF};
use super::*;
use crate::utils::host_hooks as shim;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;
const VECTOR: usize = 7;

/// EINT de USBSTS: «el controlador tiene un evento que te concierne».
const USBSTS_EINT: u32 = 0x08;
/// PCD de USBSTS: un cambio en un puerto. Pegajoso y de borrar-escribiendo-uno
/// como el EINT, así que sirve de testigo: confirmar el EINT escribiendo el
/// registro entero se lo llevaría por delante.
const USBSTS_PCD: u32 = 0x10;
/// IP e IE del interruptor 0: pendiente y habilitado.
const IMAN_IP: u32 = 0x01;
const IMAN_IE: u32 = 0x02;

fn ctrl(mut x: XhciInner) -> Arc<XhciUsbHid> {
    x.boot_enum_pending = false;
    let ack_op = x.mmio.op_base;
    let ack_rt = x.mmio.rt_base;
    Arc::new(XhciUsbHid {
        listener: EventListener::new(),
        inner: Mutex::new(Some(x)),
        msi_vector: VECTOR,
        halted: AtomicBool::new(false),
        has_rel_mouse_flag: AtomicBool::new(false),
        has_tablet_flag: AtomicBool::new(false),
        ack_op_base: AtomicUsize::new(ack_op),
        ack_rt_base: AtomicUsize::new(ack_rt),
    })
}

/// Un controlador arrancado con un ratón en el puerto 1, para que la
/// interrupción tenga algo que drenar.
fn with_mouse() -> (Hw, Arc<XhciUsbHid>) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, VECTOR);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el ratón enumera");
    (hw, ctrl(x))
}

/// Lo que el controlador deja cuando tiene un evento pendiente: el EINT
/// pegajoso y el IP del interruptor, con su IE ya habilitado por el driver.
fn con_interrupcion_pendiente(hw: &Hw) {
    hw.set_usbsts(USBSTS_EINT | USBSTS_PCD);
    hw.set_cap(RTSOFF + 0x20, IMAN_IP | IMAN_IE);
}

// --- el camino sin cerrojo -------------------------------------------------

/// Con el cerrojo cogido por otro, la interrupción se confirma igual: si no,
/// no se vuelve a entregar y la entrada se queda esperando al sondeo.
#[test]
fn con_el_cerrojo_ocupado_la_interrupcion_se_confirma_sin_el() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);
    let _ocupado = dev.inner.lock();

    dev.handle_irq(VECTOR);

    assert_eq!(
        hw.op(0x04),
        USBSTS_EINT,
        "no ha escrito la máscara de borrado del EINT: no habrá otra interrupción"
    );
    assert_eq!(hw.rt(0x20) & IMAN_IP, IMAN_IP, "el IP no se ha confirmado");
}

/// Y confirmar el IP no puede llevarse por delante el IE: el interruptor se
/// quedaría deshabilitado y no volvería a interrumpir nunca.
#[test]
fn confirmar_el_ip_no_apaga_el_interruptor() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);
    let _ocupado = dev.inner.lock();

    dev.handle_irq(VECTOR);

    assert_eq!(
        hw.rt(0x20) & IMAN_IE,
        IMAN_IE,
        "el IE se ha perdido al confirmar el IP"
    );
}

/// Sin EINT no hay nada que limpiar en USBSTS, pero el IP se confirma igual:
/// el interruptor lo levanta por su cuenta. Y USBSTS no se toca, que es una
/// escritura de MMIO menos en cada interrupción de las que no son suyas.
#[test]
fn sin_eint_el_ip_se_confirma_sin_tocar_usbsts() {
    let (hw, dev) = with_mouse();
    hw.set_usbsts(USBSTS_PCD);
    hw.set_cap(RTSOFF + 0x20, IMAN_IP | IMAN_IE);
    let _ocupado = dev.inner.lock();

    dev.handle_irq(VECTOR);

    assert_eq!(hw.rt(0x20) & IMAN_IP, IMAN_IP, "el IP no se ha confirmado");
    assert_eq!(
        hw.op(0x04),
        USBSTS_PCD,
        "ha escrito USBSTS sin tener un EINT que limpiar"
    );
}

/// Antes de que `probe` termine no hay ventanas que confirmar, y el camino
/// sin cerrojo no puede escribir en la dirección cero.
#[test]
fn sin_las_ventanas_apuntadas_el_camino_sin_cerrojo_no_escribe_nada() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);
    dev.ack_op_base.store(0, Ordering::Relaxed);
    dev.ack_rt_base.store(0, Ordering::Relaxed);
    let _ocupado = dev.inner.lock();

    dev.handle_irq(VECTOR);

    assert_eq!(
        hw.op(0x04),
        USBSTS_EINT | USBSTS_PCD,
        "ha confirmado sin tener dónde"
    );
}

/// Y con una sola de las dos ventanas tampoco: las dos escrituras van juntas.
#[test]
fn con_una_sola_ventana_apuntada_tampoco_se_confirma() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);
    dev.ack_rt_base.store(0, Ordering::Relaxed);
    let _ocupado = dev.inner.lock();

    dev.handle_irq(VECTOR);

    assert_eq!(
        hw.op(0x04),
        USBSTS_EINT | USBSTS_PCD,
        "ha confirmado a medias"
    );
}

// --- el manejador ----------------------------------------------------------

/// La interrupción de otro vector no es nuestra: ni se confirma ni se drena.
#[test]
fn una_interrupcion_de_otro_vector_no_se_atiende() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);

    dev.handle_irq(VECTOR + 1);

    assert_eq!(
        hw.op(0x04),
        USBSTS_EINT | USBSTS_PCD,
        "ha confirmado una interrupción que no era suya"
    );
}

/// La vuelta normal: con el cerrojo libre se confirma y se drena el anillo,
/// que es lo que hace que un informe del ratón llegue sin esperar al sondeo.
#[test]
fn con_el_cerrojo_libre_la_interrupcion_drena_el_anillo() {
    let (hw, dev) = with_mouse();
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x00, 7, 0, 0]);
    con_interrupcion_pendiente(&hw);

    dev.handle_irq(VECTOR);

    assert_eq!(
        dev.inner.lock().as_ref().expect("el controlador").hids[0].report_count,
        1,
        "la interrupción no ha drenado el informe"
    );
    assert_eq!(
        hw.usbsts() & USBSTS_EINT,
        0,
        "el EINT se ha quedado puesto con el cerrojo libre"
    );
    assert_eq!(
        hw.usbsts() & USBSTS_PCD,
        USBSTS_PCD,
        "se ha llevado por delante el cambio de puerto pendiente"
    );
}

/// Un controlador que ya no está —`inner` vacío, que es lo que deja un
/// apagado— no puede atender nada, y la interrupción no tiene que reventar.
#[test]
fn sin_controlador_la_interrupcion_no_hace_nada() {
    let (hw, dev) = with_mouse();
    con_interrupcion_pendiente(&hw);
    *dev.inner.lock() = None;

    dev.handle_irq(VECTOR);

    assert_eq!(
        hw.op(0x04),
        USBSTS_EINT | USBSTS_PCD,
        "ha tocado los registros sin controlador detrás"
    );
}

// --- el handoff del BIOS ---------------------------------------------------

const LEGSUP_OFF: usize = 0x500;
const BIOS_OWNED: u32 = 1 << 16;
const OS_OWNED: u32 = 1 << 24;

fn mmio(hw: &Hw) -> XhciMmio {
    XhciMmio::from_virt(hw.base(), BAR_SIZE).expect("una BAR válida")
}

/// Con el BIOS dueño del controlador se pide el semáforo, y el BIOS lo
/// suelta: eso es un arranque en el que el xHCI llega entero a la OS.
#[test]
fn con_el_bios_dueno_se_pide_el_semaforo_y_lo_suelta() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.set_legacy_cap(LEGSUP_OFF);

    mmio(&hw).perform_bios_handoff();

    assert_eq!(
        hw.legsup() & OS_OWNED,
        OS_OWNED,
        "no se ha pedido el semáforo"
    );
    assert_eq!(hw.legsup() & BIOS_OWNED, 0, "el BIOS sigue siendo el dueño");
    assert_eq!(
        hw.legctlsts(),
        0xffff_0000,
        "los SMI del BIOS se han quedado habilitados"
    );
}

/// Un BIOS que no suelta el controlador no puede colgar el arranque: se
/// agota el plazo y se sigue adelante forzando el control.
#[test]
fn un_bios_que_no_suelta_el_controlador_no_cuelga_el_arranque() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.set_legacy_cap(LEGSUP_OFF);
    hw.bios_keeps_the_controller();

    mmio(&hw).perform_bios_handoff();

    assert_eq!(
        hw.legsup() & BIOS_OWNED,
        BIOS_OWNED,
        "el fake ha soltado un controlador que no suelta"
    );
    assert_eq!(
        hw.legctlsts(),
        0xffff_0000,
        "no se ha seguido adelante tras el plazo"
    );
}

/// Y si el BIOS no lo tiene cogido, no se le pide nada: sólo se apagan sus
/// SMI, que es lo que queda por hacer.
#[test]
fn sin_que_el_bios_lo_tenga_no_se_pide_el_semaforo() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.set_legacy_cap(LEGSUP_OFF);
    hw.set_cap(LEGSUP_OFF, 1);

    mmio(&hw).perform_bios_handoff();

    assert_eq!(
        hw.legsup() & OS_OWNED,
        0,
        "ha pedido un semáforo que nadie tenía"
    );
    assert_eq!(hw.legctlsts(), 0xffff_0000, "no se han apagado los SMI");
}

/// Sin cadena xECP no hay nada que ceder: el campo de HCCPARAMS1 a cero es lo
/// que trae un controlador sin capacidades extendidas.
#[test]
fn sin_cadena_de_capacidades_no_se_toca_nada() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    hw.set_legacy_cap(LEGSUP_OFF);
    hw.set_cap(0x10, 0);

    mmio(&hw).perform_bios_handoff();

    assert_eq!(hw.legsup(), 1 | BIOS_OWNED, "ha recorrido una cadena vacía");
    assert_eq!(hw.legctlsts(), 0, "ha escrito los SMI sin capacidad legacy");
}

// --- liberar un slot -------------------------------------------------------

/// Terminación 19: Context State Error, que es con la que el controlador
/// rechaza un Disable Slot sobre un slot que no está donde él cree.
const CC_CONTEXT_STATE: u32 = 19;

/// La vuelta normal: con el Disable Slot aceptado el controlador ha soltado
/// los contextos y los anillos, así que la memoria se puede devolver.
#[test]
fn un_disable_slot_aceptado_devuelve_la_memoria_del_slot() {
    shim::alone_with_the_allocator(|| {
        let (_hw, dev) = with_mouse();
        let antes = shim::DEALLOC_PAGES.load(Ordering::SeqCst);

        dev.inner
            .lock()
            .as_mut()
            .expect("el controlador")
            .free_slot(1)
            .expect("el slot se libera");

        assert!(
            shim::DEALLOC_PAGES.load(Ordering::SeqCst) > antes,
            "no se ha devuelto nada al liberar un slot que el controlador soltó"
        );
    });
}

/// Y si el Disable Slot falla, el controlador puede seguir mirando esos
/// contextos: devolver las páginas es darle memoria ya reutilizada, así que
/// se abandonan a propósito.
#[test]
fn un_disable_slot_que_falla_abandona_la_memoria_en_vez_de_devolverla() {
    shim::alone_with_the_allocator(|| {
        let (hw, dev) = with_mouse();
        hw.answer_commands_with(CC_CONTEXT_STATE);
        let antes = shim::DEALLOC_PAGES.load(Ordering::SeqCst);

        dev.inner
            .lock()
            .as_mut()
            .expect("el controlador")
            .free_slot(1)
            .expect("el slot se libera igual");

        assert_eq!(
            shim::DEALLOC_PAGES.load(Ordering::SeqCst),
            antes,
            "ha devuelto memoria que el controlador todavía puede estar mirando"
        );
    });
}

/// Pase lo que pase con el Disable Slot, el slot queda libre para lo
/// siguiente que enchufen: dejarlo ocupado es un puerto muerto hasta
/// reiniciar.
#[test]
fn liberar_un_slot_lo_deja_libre_aunque_el_disable_falle() {
    let (hw, dev) = with_mouse();
    hw.answer_commands_with(CC_CONTEXT_STATE);
    let mut g = dev.inner.lock();
    let xi = g.as_mut().expect("el controlador");

    xi.free_slot(1).expect("el slot se libera");

    assert!(xi.slot_topo[1].is_none(), "el slot sigue ocupado");
    assert_eq!(xi.slot_speed[1], 0, "la velocidad del slot se ha quedado");
    assert!(
        xi.devs.iter().all(|d| d.slot != 1),
        "el dispositivo sigue en el censo"
    );
}

/// Un slot que no está asignado no es un error: el camino de desconexión
/// pasa por aquí con lo que el puerto diga, y un hub que se va arrastra
/// slots que ya se liberaron.
#[test]
fn liberar_un_slot_que_no_esta_asignado_no_hace_nada() {
    shim::alone_with_the_allocator(|| {
        let (_hw, dev) = with_mouse();
        let antes = shim::DEALLOC_PAGES.load(Ordering::SeqCst);

        dev.inner
            .lock()
            .as_mut()
            .expect("el controlador")
            .free_slot(5)
            .expect("liberar un slot vacío no falla");

        assert_eq!(
            shim::DEALLOC_PAGES.load(Ordering::SeqCst),
            antes,
            "ha devuelto memoria de un slot que no tenía"
        );
    });
}

/// Y sobre todo no se le manda nada al controlador por ese slot: un Disable
/// Slot sobre un slot que nunca habilitó es un comando que contesta error en
/// el mejor caso, y en el peor tira el que de verdad lo tenga.
#[test]
fn liberar_un_slot_que_no_esta_asignado_no_manda_ningun_comando() {
    let (hw, dev) = with_mouse();
    let antes = hw.commands().len();

    dev.inner
        .lock()
        .as_mut()
        .expect("el controlador")
        .free_slot(5)
        .expect("liberar un slot vacío no falla");

    assert_eq!(
        hw.commands().len(),
        antes,
        "ha mandado comandos por un slot que no tenía"
    );
}
