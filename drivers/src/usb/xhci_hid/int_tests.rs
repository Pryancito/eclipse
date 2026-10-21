//! Las transferencias de interrupción: lo que llega por los endpoints que no
//! son EP0.
//!
//! Hasta aquí el fake sólo contestaba control. Ésta le pone el otro lado: el
//! Normal TRB que el driver dejó armado se llena con lo que el dispositivo
//! tenga que entregar y se contesta con su Transfer Event, con el *residual*
//! en los 24 bits bajos. Por ahí pasan las dos cosas que el driver atiende en
//! caliente: los informes de un ratón y el mapa de bits de cambios de un hub.

use super::xhci_fake::{FakeDev, Hw};
use super::*;
use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// Terminación 23: Missed Service Error, que no deja el endpoint con halt.
const CC_MISSED: u32 = 23;
/// Terminación 6: Stall Error, que sí lo deja.
const CC_STALL: u32 = 6;

/// Un controlador arrancado con un ratón ya enumerado en el puerto 1.
fn with_mouse() -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el ratón enumera");
    (hw, x)
}

/// Un controlador arrancado con un hub de cuatro puertos ya enumerado.
fn with_hub() -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(4));
    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");
    (hw, x)
}

/// Un oyente que apunta todo lo que le llega, como un cliente de `/dev/input`.
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

/// Los movimientos relativos que salieron, en orden.
fn motions(seen: &Arc<Mutex<Vec<(u16, u16, i32)>>>) -> Vec<(u16, i32)> {
    seen.lock()
        .iter()
        .filter(|(t, _, _)| *t == EV_REL)
        .map(|(_, c, v)| (*c, *v))
        .collect()
}

// --- los informes de un ratón --------------------------------------------

/// El camino entero de un informe: el dispositivo lo entrega por su endpoint
/// de interrupción, el controlador lo escribe en el buffer del TRB armado y
/// el driver lo decodifica y lo publica.
#[test]
fn un_informe_de_un_raton_sale_como_movimiento_relativo() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    assert_eq!(dci, 3, "el endpoint de interrupción IN 1 es el DCI 3");
    let (lis, seen) = listener();

    // Botones a cero, +5 en X, -5 en Y.
    hw.send_in(1, dci, &[0x00, 5, 0xfb, 0]);
    x.process_irq_events(Some(&lis), false);

    assert_eq!(
        seen.lock().clone(),
        alloc::vec![
            (EV_REL, REL_X, 5),
            (EV_REL, REL_Y, -5),
            (EV_SYN, SYN_REPORT, 0)
        ],
        "el informe no ha salido como un movimiento"
    );
}

/// Un informe más corto que el buffer no puede leerse entero: lo que hay
/// detrás es la cola del informe ANTERIOR que ocupó ese mismo buffer, y el
/// residual del evento es lo único que lo dice.
#[test]
fn un_informe_corto_no_arrastra_la_cola_del_anterior() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    // Tantos informes completos como buffers tiene el anillo, para que el
    // corto caiga en uno ya usado y con datos vivos dentro.
    for _ in 0..HID_QUEUE_DEPTH {
        hw.send_in(1, dci, &[0x00, 40, 40, 0]);
        x.process_irq_events(Some(&lis), false);
    }
    seen.lock().clear();

    // Un informe de un solo byte: botones, y nada más.
    hw.send_in(1, dci, &[0x00]);
    x.process_irq_events(Some(&lis), false);

    assert_eq!(
        motions(&seen),
        Vec::new(),
        "el informe corto ha traído movimiento de la cola del anterior"
    );
}

/// Una transferencia de longitud cero es legal en un endpoint de
/// interrupción y quiere decir «nada que contar». Decodificarla es leer un
/// informe de ceros, y un informe de ceros no es «nada»: es todos los botones
/// sueltos. Con un botón pulsado, eso suelta el botón él solo.
#[test]
fn un_informe_de_longitud_cero_no_se_entrega() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    // Botón izquierdo pulsado y sin soltar.
    hw.send_in(1, dci, &[0x01, 0, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    assert!(
        seen.lock().contains(&(EV_KEY, BTN_LEFT, 1)),
        "el botón no se ha pulsado: {:?}",
        seen.lock().clone()
    );
    seen.lock().clear();

    hw.send_in(1, dci, &[]);
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        seen.lock().clone(),
        Vec::new(),
        "una transferencia vacía ha publicado algo: ha soltado el botón"
    );

    // Y el endpoint sigue vivo: el TRB se rearmó igual.
    hw.send_in(1, dci, &[0x01, 7, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        motions(&seen),
        alloc::vec![(REL_X, 7)],
        "el endpoint se ha quedado mudo tras la transferencia vacía"
    );
    assert!(
        !seen.lock().contains(&(EV_KEY, BTN_LEFT, 1)),
        "el botón se ha vuelto a pulsar: la vacía lo había soltado"
    );
}

/// Más informes seguidos que TRBs tiene el anillo, así que la ráfaga pasa por
/// encima de su Link TRB y vuelve al principio. Es la prueba de que las dos
/// cabezas (la que arma y la que entrega) van en paso: si se separan, los
/// informes empiezan a salir del buffer de la vez anterior y el orden se
/// rompe aunque no se pierda ninguno.
#[test]
fn una_rafaga_mas_larga_que_el_anillo_sale_en_orden() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    // El anillo del endpoint son 64 TRBs: 100 informes lo dan la vuelta.
    let rafaga = HID_QUEUE_DEPTH * 25;
    for i in 0..rafaga {
        hw.send_in(1, dci, &[0x00, i as u8 + 1, 0, 0]);
    }
    // Lo que no cupo en un TRB armado se queda esperando, y cada rearme lo va
    // sacando: eso es lo que el drenaje tiene que acabar de vaciar.
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        hw.undelivered(1),
        0,
        "ha quedado algo sin entregar: el endpoint no se rearma"
    );
    x.process_irq_events(Some(&lis), false);

    let esperado: Vec<(u16, i32)> = (0..rafaga).map(|i| (REL_X, i as i32 + 1)).collect();
    assert_eq!(
        motions(&seen),
        esperado,
        "los informes no han salido en orden o se ha perdido alguno"
    );
}

/// Un STALL deja el endpoint con halt, y de ahí no sale solo: hay que
/// quitarle el halt al dispositivo (`CLEAR_FEATURE(ENDPOINT_HALT)`) y
/// resetear el endpoint en el controlador, y eso no puede correr dentro del
/// drenaje de eventos.
#[test]
fn un_stall_en_el_endpoint_lo_recupera_y_sigue_entregando() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    hw.fail_in(1, dci, CC_STALL);
    x.process_irq_events(Some(&lis), false);

    let reqs = hw.requests(1);
    assert!(
        reqs.iter()
            .any(|s| s.bmrt == 0x02 && s.breq == 0x01 && s.windex == 0x81),
        "no se le ha quitado el halt al endpoint 1 IN: {:?}",
        reqs
    );
    // Reset Endpoint es el TRB tipo 14.
    assert!(
        hw.commands().iter().any(|c| c.ty == 14),
        "no se ha reseteado el endpoint en el controlador"
    );
    assert!(
        x.pending_ep_resets.is_empty(),
        "el reseteo pendiente no se ha consumido"
    );

    // Y después del rescate el ratón vuelve a mover el puntero.
    hw.send_in(1, dci, &[0x00, 3, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        motions(&seen),
        alloc::vec![(REL_X, 3)],
        "el endpoint no ha vuelto a entregar tras el rescate"
    );
}

/// Un Missed Service Error es el host saltándose un intervalo, y NO deja
/// nada con halt: resetear un endpoint que está corriendo contesta Context
/// State Error, así que un informe perdido se convertiría en un rescate
/// fallido.
#[test]
fn un_fallo_que_no_deja_halt_no_resetea_el_endpoint() {
    let (hw, mut x) = with_mouse();
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    hw.fail_in(1, dci, CC_MISSED);
    x.process_irq_events(Some(&lis), false);

    assert!(
        !hw.commands().iter().any(|c| c.ty == 14),
        "se ha reseteado un endpoint que no estaba con halt"
    );
    assert!(
        !hw.requests(1)
            .iter()
            .any(|s| s.bmrt == 0x02 && s.breq == 0x01),
        "se le ha quitado un halt que el dispositivo no tenía"
    );

    // El TRB saltado se repone, así que el siguiente informe entra.
    hw.send_in(1, dci, &[0x00, 9, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        motions(&seen),
        alloc::vec![(REL_X, 9)],
        "el informe de después del fallo no ha llegado"
    );
}

// --- el endpoint de estado de un hub -------------------------------------

/// Lo que un hub manda por su endpoint de estado no es un informe HID: es un
/// mapa de bits con un bit por puerto. El driver tiene que leerlo, apuntar
/// esos puertos y atenderlos FUERA del drenaje, porque enumerar emite
/// comandos que esperan en el anillo de eventos.
#[test]
fn un_cambio_avisado_por_el_hub_enumera_lo_que_se_acaba_de_enchufar() {
    let (hw, mut x) = with_hub();
    let dci = x.hubs[0].ep_dci;
    assert_ne!(dci, 0, "el hub no tiene endpoint de estado");
    let (lis, _seen) = listener();

    // Un ratón en el puerto 3 del hub, con el hub ya enumerado: nadie lo ha
    // visto todavía.
    hw.plug_behind(1, 3, HS, FakeDev::mouse());
    assert_eq!(x.devs.len(), 1, "sólo el hub está enumerado");

    hw.hub_notify(1, dci);
    x.process_irq_events(Some(&lis), true);

    assert_eq!(
        x.devs.len(),
        2,
        "el cambio avisado no ha enumerado el ratón: {:?}",
        x.pending_hub_ports
    );
    let hijo = x
        .devs
        .iter()
        .find(|d| d.topo.route != 0)
        .expect("el ratón detrás del hub");
    assert_eq!(hijo.topo.route, 3, "el route string no lleva el puerto 3");
    assert!(
        x.pending_hub_ports.is_empty(),
        "el puerto señalado se ha quedado pendiente"
    );
}

/// Un mapa de bits sin ningún bit puesto es el informe de un hub al que no le
/// pasa nada: señalar un puerto ahí sería enumerar lo que ya está enumerado.
#[test]
fn un_mapa_de_bits_sin_cambios_no_senala_ningun_puerto() {
    let (hw, mut x) = with_hub();
    let dci = x.hubs[0].ep_dci;
    let (lis, _seen) = listener();

    hw.hub_notify(1, dci);
    x.process_irq_events(Some(&lis), false);

    assert!(
        x.pending_hub_ports.is_empty(),
        "un mapa vacío ha señalado puertos: {:?}",
        x.pending_hub_ports
    );
}

/// Un informe de estado que falla se lleva por delante los cambios que
/// traía, y de esos ya no hay otro aviso: lo único que los cubre es un
/// barrido de los puertos del hub.
#[test]
fn un_fallo_del_endpoint_de_estado_deja_un_barrido_pendiente() {
    let (hw, mut x) = with_hub();
    let dci = x.hubs[0].ep_dci;
    let (lis, _seen) = listener();
    x.hubs[0].scan_due = false;

    hw.fail_in(1, dci, CC_STALL);
    x.process_irq_events(Some(&lis), false);

    assert!(
        x.hubs[0].scan_due,
        "el fallo del endpoint de estado no ha dejado un barrido pendiente"
    );
}

/// El endpoint de estado tiene un solo TRB, así que un informe que no se
/// rearma deja el hub mudo para siempre y los cambios sólo los coge el
/// sondeo.
#[test]
fn el_endpoint_de_estado_se_rearma_tras_cada_informe() {
    let (hw, mut x) = with_hub();
    let dci = x.hubs[0].ep_dci;
    let (lis, _seen) = listener();

    for _ in 0..3 {
        hw.hub_notify(1, dci);
        x.process_irq_events(Some(&lis), false);
        assert_eq!(
            hw.undelivered(1),
            0,
            "el endpoint de estado no se ha rearmado"
        );
    }
}

// --- los informes de un teclado ------------------------------------------

/// Un teclado no tiene longitud mínima que lo proteja: sus teclas van en un
/// array, así que un informe de ceros es «no hay ninguna pulsada» y una
/// transferencia vacía decodificada suelta la tecla que estaba puesta. Es el
/// mismo fallo que en el ratón, pero aquí sí se nota.
#[test]
fn un_informe_vacio_no_suelta_las_teclas_pulsadas() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::keyboard());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el teclado enumera");
    let dci = x.hids[0].ep_dci;
    let (lis, seen) = listener();

    // La «a» pulsada y sin soltar.
    hw.send_in(1, dci, &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    assert!(
        seen.lock().contains(&(EV_KEY, KEY_A, 1)),
        "la tecla no se ha pulsado: {:?}",
        seen.lock().clone()
    );
    seen.lock().clear();

    hw.send_in(1, dci, &[]);
    x.process_irq_events(Some(&lis), false);
    assert_eq!(
        seen.lock().clone(),
        Vec::new(),
        "la transferencia vacía ha soltado la tecla"
    );

    // Y el informe idéntico de después no la vuelve a pulsar: para el driver
    // nunca se soltó, así que no hay delta ninguno que publicar.
    hw.send_in(1, dci, &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    x.process_irq_events(Some(&lis), false);
    let teclas: Vec<(u16, u16, i32)> = seen
        .lock()
        .iter()
        .filter(|(t, _, _)| *t == EV_KEY)
        .copied()
        .collect();
    assert_eq!(
        teclas,
        Vec::new(),
        "la tecla se ha vuelto a pulsar: la vacía la había soltado"
    );
}
