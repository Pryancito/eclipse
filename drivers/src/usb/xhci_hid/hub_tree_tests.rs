//! Un hub y lo que cuelga de él.
//!
//! `hub_tests` prueba las funciones puras de la clase hub (el mapa de bits de
//! cambios, la tabla de características, el descriptor); esto prueba el
//! camino: `setup_hub` pidiendo el descriptor, configurando el slot como hub,
//! encendiendo los puertos y enumerando lo que haya detrás, con el route
//! string y el traductor de transacciones que le toquen a cada hijo.

use super::xhci_fake::{FakeDev, Hw};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;
const FS: u8 = 1;

/// Un controlador arrancado, con un hub de cuatro puertos en el puerto 1.
fn with_hub() -> (Hw, XhciInner) {
    let hw = Hw::new(SLOTS, PORTS);
    // Los plazos del hub son de cientos de milisegundos y sus bucles no leen
    // registros, así que el reloj tiene que correr más.
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(4));
    (hw, x)
}

/// Un hub no es un dispositivo más: hasta que no se le dice al controlador
/// que lo es, con su número de puertos, no rutea nada aguas abajo (§4.3.3).
#[test]
fn un_hub_se_configura_como_hub_y_enciende_sus_puertos() {
    let (hw, mut x) = with_hub();

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    assert_eq!(x.hubs.len(), 1, "el hub no se ha apuntado");
    assert_eq!(
        x.hubs[0].ports, 4,
        "los puertos salen del descriptor de hub"
    );
    assert_eq!(x.hubs[0].slot, 1);

    let reqs = hw.requests(1);
    assert!(
        reqs.iter().any(|s| s.bmrt == 0xa0 && s.breq == 0x06),
        "no se ha pedido el descriptor de hub: {:?}",
        reqs
    );
    // Y PORT_POWER en los cuatro puertos: un puerto sin alimentar no ve nada.
    let encendidos: Vec<u16> = reqs
        .iter()
        .filter(|s| s.bmrt == 0x23 && s.breq == 0x03 && s.wvalue == 8)
        .map(|s| s.windex)
        .collect();
    assert_eq!(
        encendidos,
        alloc::vec![1, 2, 3, 4],
        "no se han encendido los cuatro puertos"
    );
    // El bit Hub del Slot Context va por Configure Endpoint, no por Evaluate
    // Context: éste último sólo evalúa latencia e interrupter (§4.6.7).
    assert!(
        hw.commands().iter().any(|c| c.ty == 12 && c.slot == 1),
        "el slot no se ha configurado como hub: {:?}",
        hw.commands()
    );
}

/// Lo que de verdad importa de un hub: que lo que cuelga de él enumere, y que
/// el controlador sepa por dónde llegar a cada cosa. Eso es el route string.
#[test]
fn un_raton_detras_de_un_hub_se_enumera_con_su_route_string() {
    let (hw, mut x) = with_hub();
    hw.plug_behind(1, 2, HS, FakeDev::mouse());

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    assert_eq!(x.devs.len(), 2, "el hub y el ratón son dos dispositivos");
    let hijo = x
        .devs
        .iter()
        .find(|d| d.topo.parent_slot == 1)
        .expect("el ratón detrás del hub");
    assert_eq!(hijo.topo.root_port, 1, "el puerto raíz es el del hub");
    assert_eq!(
        hijo.topo.route, 2,
        "el primer nivel va en los bits 3:0 del route string"
    );
    assert_eq!(hijo.topo.parent_port, 2);
    assert_eq!(hijo.topo.depth, 1);
    assert_eq!((hijo.vid, hijo.pid), (0x046d, 0xc077));
    // Y es el ratón el que ha contestado, no el hub: lo único que distingue a
    // los dos en el mismo puerto raíz es el route.
    assert!(
        hw.requests_at(1, 2)
            .iter()
            .any(|s| s.bmrt == 0x80 && s.breq == 0x06 && s.wvalue == 0x0100),
        "el hijo no ha recibido su descriptor de dispositivo"
    );
    assert_eq!(x.hids.len(), 1, "el ratón del hub no se ha armado");
}

/// Un dispositivo de plena o baja velocidad detrás de un hub de alta
/// velocidad habla por el traductor de transacciones de ese hub. Sin el slot
/// y el puerto del traductor en el contexto, el controlador no sabe por dónde
/// pasar sus transacciones y el dispositivo no enumera.
#[test]
fn un_dispositivo_lento_detras_de_un_hub_lleva_su_traductor() {
    let (hw, mut x) = with_hub();
    hw.plug_behind(1, 3, FS, FakeDev::mouse());

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    let hijo = x
        .devs
        .iter()
        .find(|d| d.topo.parent_slot == 1)
        .expect("el ratón detrás del hub");
    assert_eq!(hijo.speed, FS, "la velocidad sale de wPortStatus");
    assert_eq!(
        (hijo.topo.tt_slot, hijo.topo.tt_port),
        (1, 3),
        "el traductor es el hub y su puerto"
    );
}

/// Los bits de cambio de un puerto se reconocen *antes* de enumerar: la
/// enumeración tarda, y un desenchufe que ocurra mientras corre no se puede
/// perder por haberlo borrado después.
#[test]
fn los_cambios_de_un_puerto_quedan_reconocidos() {
    let (hw, mut x) = with_hub();
    hw.plug_behind(1, 2, HS, FakeDev::mouse());

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    for p in 1..=4 {
        assert_eq!(
            hw.hub_port_change(1, p),
            0,
            "el puerto {} se ha quedado con cambios sin reconocer",
            p
        );
    }
}

/// Un puerto que no se habilita por mucho que se resetee no enumera nada, y
/// no se lleva por delante los demás puertos del hub.
#[test]
fn un_puerto_de_hub_que_no_se_habilita_no_enumera() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(20_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(2).with_dead_ports());
    hw.plug_behind(1, 1, HS, FakeDev::mouse());

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    assert_eq!(x.hubs.len(), 1, "el hub sí tiene que quedarse");
    assert_eq!(
        x.devs.len(),
        1,
        "ha enumerado algo detrás de un puerto que no se habilita"
    );
}

/// Un hub vacío es lo normal, y no tiene que dejar ningún slot a medias.
#[test]
fn un_hub_vacio_no_enumera_nada_detras() {
    let (hw, mut x) = with_hub();

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    assert_eq!(x.devs.len(), 1, "sólo está el hub");
    assert!(
        hw.commands().iter().filter(|c| c.ty == 9).count() == 1,
        "se han pedido slots para puertos vacíos: {:?}",
        hw.commands()
    );
}

/// Un descriptor de hub que no dice ningún puerto no se puede usar: todo lo
/// de después (el número de puertos del Slot Context, el barrido, el mapa de
/// cambios) se dimensiona con ese número.
#[test]
fn un_hub_que_no_dice_puertos_no_se_configura() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(0));

    assert!(
        x.setup_device(DevTopo::root(1), HS).is_err(),
        "un hub sin puertos se ha configurado"
    );
    assert!(x.hubs.is_empty(), "ha quedado un hub sin puertos apuntado");
}

/// Un ratón que se enchufa *después* de que el hub esté listo: lo coge el
/// barrido, que es lo que corre cuando el endpoint de cambios avisa (o cada
/// pocos segundos si no hay endpoint). Es el camino del hotplug detrás de un
/// hub, que es como está casi todo lo que la gente enchufa.
#[test]
fn un_raton_enchufado_despues_lo_coge_el_barrido() {
    let (hw, mut x) = with_hub();
    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");
    assert_eq!(x.devs.len(), 1, "el hub empieza vacío");

    hw.plug_behind(1, 4, HS, FakeDev::mouse());
    // Lo que hace el endpoint de cambios de estado al traer su mapa de bits.
    x.hubs[0].scan_due = true;
    x.scan_hubs_if_due();

    let hijo = x
        .devs
        .iter()
        .find(|d| d.topo.parent_slot == 1)
        .expect("el ratón que llegó después");
    assert_eq!(hijo.topo.parent_port, 4);
    assert_eq!(hijo.topo.route, 4);
    assert_eq!(x.hids.len(), 1, "el ratón no se ha armado");
}

/// Y uno que se va: el barrido tiene que liberar su slot, no sólo quitarlo de
/// la lista. Un slot que no se devuelve no se recupera hasta reiniciar.
#[test]
fn un_raton_que_se_va_libera_su_slot() {
    let (hw, mut x) = with_hub();
    hw.plug_behind(1, 2, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");
    let hijo = x
        .devs
        .iter()
        .find(|d| d.topo.parent_slot == 1)
        .map(|d| d.slot)
        .expect("el ratón detrás del hub");

    hw.unplug_behind(1, 2);
    x.hubs[0].scan_due = true;
    x.scan_hubs_if_due();

    assert_eq!(
        x.devs.len(),
        1,
        "el ratón sigue en la lista: {:?}",
        x.devs.len()
    );
    assert!(x.hids.is_empty(), "ha quedado un endpoint HID armado");
    assert!(
        hw.commands().iter().any(|c| c.ty == 10 && c.slot == hijo),
        "el slot {} no se ha devuelto con Disable Slot: {:?}",
        hijo,
        hw.commands()
    );
}

/// Hay hubs que señalan el puerto habilitado mientras su reset sigue puesto.
/// Un puerto en reset no se puede usar todavía: hay que seguir preguntando
/// hasta que el reset se va, y sólo entonces leer la velocidad.
#[test]
fn un_puerto_no_se_usa_mientras_su_reset_sigue_puesto() {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::hub(2).with_slow_reset(2));
    hw.plug_behind(1, 1, HS, FakeDev::mouse());

    x.setup_device(DevTopo::root(1), HS)
        .expect("el hub enumera");

    assert_eq!(
        x.devs.len(),
        2,
        "el ratón detrás del hub acaba enumerando igual"
    );
    // Una consulta antes del reset y una por vuelta hasta que el reset se fue:
    // si se diera por bueno el primer «habilitado», serían dos.
    let consultas = hw
        .requests(1)
        .iter()
        .filter(|s| s.bmrt == 0xa3 && s.breq == 0x00 && s.windex == 1)
        .count();
    assert!(
        consultas >= 4,
        "sólo {} consultas de estado: no se ha esperado a que el reset se fuera",
        consultas
    );
}
