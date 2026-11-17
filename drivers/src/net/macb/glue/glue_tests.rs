//! El GEM visto desde arriba: el arranque, la interrupcion y el camino de una
//! trama a traves de smoltcp.
//!
//! Los anillos y las ventanas ya estan probados contra el MAC falso; lo que se
//! prueba aqui es **lo que decide cuando llamarlos**, que es la mitad donde
//! los drivers viejos de la casa no tenian un solo test.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::super::macb_fake::Hw;
use super::super::rings::test_mac::*;
use super::super::rings::*;
use super::*;
use crate::utils::deferred_job::{drain_deferred_jobs, pending_deferred_jobs};

const IRQ: usize = 6;

/// La interfaz sobre el GEM falso de este hilo.
fn interfaz(hw: &Hw) -> MacbInterface {
    unsafe {
        macb_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            MacbConfig::default(),
            None,
        )
    }
    .expect("un GEM sano tiene que arrancar")
}

/// Una peticion de ARP, que es la trama mas corta que smoltcp mira de verdad.
const TRAMA: &[u8] = &[
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33, 0x08, 0x06, 1, 2, 3, 4,
];

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

/// El arranque deja la tarjeta encendida, con el PHY encontrado **por
/// barrido** y las interrupciones armadas.
///
/// El barrido no es el camino de respaldo: el nodo del RP1 no dice en ningun
/// sitio donde esta el PHY, asi que es el unico camino.
#[test]
fn a_healthy_gem_comes_up_with_the_phy_found_by_scanning_and_the_interrupts_armed() {
    let hw = Hw::new();
    let iface = interfaz(&hw);

    assert_eq!(hw.peek(NCR) & (NCR_RE | NCR_TE), NCR_RE | NCR_TE);
    assert_eq!(hw.peek(NCR) & NCR_MPE, NCR_MPE, "el MDIO quedo apagado");
    assert_eq!(
        hw.peek(IMR) & MACB_IRQ_WANTED,
        0,
        "quedo algo sin armar de lo que el manejador espera"
    );
    assert_eq!(iface.get_mtu(), 1500);
    assert_eq!(iface.ifname(), "eth0");
    assert!(!iface.is_loopback());
}

/// **La tarjeta no se enciende hasta que hay anillos.**
///
/// Encender la recepcion sin huecos donde poner tramas es pedirle al MAC que
/// desborde su FIFO, y eso no da un error: da tramas perdidas en el arranque
/// de cada placa. Lo que se mira es el orden: la escritura que sube
/// `NCR_RE`/`NCR_TE` tiene que venir **despues** de publicar las bases de los
/// dos anillos.
#[test]
fn the_card_is_not_turned_on_until_the_rings_exist() {
    let hw = Hw::new();
    let _iface = interfaz(&hw);

    let escrituras = hw.writes();
    let enciende = escrituras
        .iter()
        .position(|(off, val)| *off == NCR && val & (NCR_RE | NCR_TE) != 0)
        .expect("nadie encendio la tarjeta");
    for base in [RBQP, TBQP] {
        let publica = escrituras
            .iter()
            .position(|(off, _)| *off == base)
            .unwrap_or_else(|| panic!("nadie publico la base {:#x}", base));
        assert!(
            publica < enciende,
            "la tarjeta se encendio antes de publicar la base {:#x}",
            base
        );
    }
}

/// Y la MAC elegida **acaba en los registros**.
///
/// Sin esta escritura el GEM se queda con lo que el firmware dejara --- o con
/// ceros --- y el filtro de recepcion tira todo lo que no sea broadcast: una
/// tarjeta que ve llegar los ARP y no su propia respuesta.
#[test]
fn the_address_that_was_chosen_ends_up_in_the_registers() {
    let hw = Hw::new();
    let elegida = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    let iface = unsafe {
        macb_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            MacbConfig::default(),
            Some(elegida),
        )
    }
    .expect("arranca");

    assert_eq!(iface.driver.mac.read_mac_addr_set(0), elegida);
    assert_eq!(iface.get_mac().as_bytes(), &elegida);
}

/// Una ventana que no contesta se rechaza antes de escribir nada en ella.
#[test]
fn a_window_with_nothing_behind_it_does_not_bring_an_interface_up() {
    let hw = Hw::new();
    hw.faults(|f| f.dead_window = true);
    let r = unsafe {
        macb_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            MacbConfig::default(),
            None,
        )
    };
    assert!(r.is_err(), "arranco sobre una ventana muerta");
}

/// Un bus MDIO sin ningun PHY tampoco: un GEM sin PHY detras es una tarjeta
/// sin enlace, y encenderla seria anunciar una interfaz que no puede
/// transmitir.
#[test]
fn a_bus_with_no_phy_on_it_does_not_bring_an_interface_up() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_mute = (0..32).collect());
    let r = unsafe {
        macb_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            MacbConfig::default(),
            None,
        )
    };
    assert!(r.is_err(), "arranco sin PHY");
}

/// La MAC que trae el arbol manda sobre la que dejo el firmware.
#[test]
fn the_address_the_tree_carries_wins_over_the_one_in_the_registers() {
    let hw = Hw::new();
    // El firmware dejo una MAC **valida y distinta** en los registros. Sin
    // esto las dos fuentes dan lo mismo y el orden no se puede ver.
    hw.poke(GEM_SA1B, 0x1267_cf2c);
    hw.poke(GEM_SA1T, 0x0000_5634);
    let del_firmware = [0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56];
    assert_eq!(
        hw.mac().hw_mac_addr(),
        Some(del_firmware),
        "el falso no quedo con la MAC del firmware"
    );
    let del_arbol = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    let iface = unsafe {
        macb_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            MacbConfig::default(),
            Some(del_arbol),
        )
    }
    .expect("arranca");
    assert_eq!(iface.get_mac().as_bytes(), &del_arbol);
    assert_ne!(
        iface.get_mac().as_bytes(),
        &del_firmware,
        "se cogio la del firmware teniendo una del arbol"
    );
}

/// Y sin ninguna de las dos se inventa una **administrada localmente**.
///
/// Una direccion global inventada puede chocar con una de verdad en la misma
/// red; una local no sale nunca de este segmento.
#[test]
fn with_no_address_anywhere_the_invented_one_is_locally_administered() {
    let hw = Hw::new();
    // El firmware no dejo ninguna: los cuatro juegos a cero.
    for set in 0..4 {
        hw.poke(GEM_SA1B + set * SA_STRIDE, 0);
        hw.poke(GEM_SA1T + set * SA_STRIDE, 0);
    }
    let iface = interfaz(&hw);
    let mac = iface.get_mac();
    let bytes = mac.as_bytes();
    assert_eq!(bytes[0] & 0b10, 0b10, "la inventada no es local");
    assert_eq!(bytes[0] & 1, 0, "la inventada es de grupo");
}

/// El arranque **no deja ninguna direccion IPv4 puesta**.
///
/// La pone el DHCP o `ifconfig`. Clavar una aqui es lo que hacen los drivers
/// viejos de la casa y es como una placa acaba respondiendo en una direccion
/// que nadie le dio.
#[test]
fn the_card_comes_up_without_an_address_nobody_gave_it() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    for cidr in iface.get_ip_address() {
        if let smoltcp::wire::IpCidr::Ipv4(v4) = cidr {
            assert!(
                v4.address().is_unspecified(),
                "arranco con la direccion {:?} puesta",
                v4
            );
        }
    }
}

// ---------------------------------------------------------------------------
// La interrupcion
// ---------------------------------------------------------------------------

/// Una interrupcion de recepcion: se reconoce lo leido, se desarma la tarjeta
/// y se encola **un** trabajo.
///
/// La tarjeta se queda desarmada hasta que el trabajo corre: armar antes
/// dejaria que los flancos siguientes reconozcan causas que ese trabajo no va
/// a ver.
#[test]
fn a_receive_interrupt_acknowledges_disarms_and_queues_one_job() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let antes = pending_deferred_jobs();
    hw.raise_irq(ISR_RCOMP);
    let marca_r = hw.read_mark();

    iface.handle_irq(IRQ);

    assert_eq!(hw.peek(ISR) & ISR_RCOMP, 0, "no se reconocio");
    // **Una sola lectura del `ISR`.** Con `GEM_IRQCOR` puesto, leerlo es lo
    // que lo limpia: una segunda lectura decide sobre un cero y la causa se
    // pierde en ese silicio y no en el otro.
    assert_eq!(
        hw.reads_since(marca_r, ISR),
        1,
        "el manejador leyo el estado mas de una vez"
    );
    assert_eq!(
        hw.peek(IMR) & MACB_IRQ_WANTED,
        MACB_IRQ_WANTED,
        "la tarjeta quedo armada con un trabajo pendiente"
    );
    assert_eq!(pending_deferred_jobs(), antes + 1);

    drain_deferred_jobs();
    assert_eq!(
        hw.peek(IMR) & MACB_IRQ_WANTED,
        0,
        "el trabajo corrio y no volvio a armar la tarjeta"
    );
}

/// Una interrupcion que no es de esta tarjeta **no reconoce nada**.
///
/// Lo que se reconoce sin atender es un evento perdido.
#[test]
fn an_interrupt_with_nothing_pending_acknowledges_nothing() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let antes = pending_deferred_jobs();
    let marca = hw.write_mark();

    iface.handle_irq(IRQ);

    assert!(
        !hw.writes_since(marca).contains(&ISR),
        "se reconocio sin tener nada pendiente"
    );
    assert_eq!(pending_deferred_jobs(), antes, "se encolo un trabajo vacio");
}

/// Y una interrupcion de **otra** linea se ignora del todo, sin tocar la
/// ventana: dos tarjetas pueden compartir manejador.
#[test]
fn an_interrupt_of_another_line_is_not_this_cards() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.raise_irq(ISR_RCOMP);
    let marca = hw.write_mark();

    iface.handle_irq(IRQ + 1);

    assert!(
        hw.writes_since(marca).is_empty(),
        "una interrupcion ajena toco la ventana"
    );
    assert_eq!(hw.peek(ISR) & ISR_RCOMP, ISR_RCOMP, "reconocio lo ajeno");
}

/// Dos flancos seguidos encolan **un solo** trabajo.
///
/// El segundo encuentra `poll_pending` puesto, reconoce su causa y se va: el
/// trabajo que ya esta encolado vaciara el anillo entero.
#[test]
fn a_second_edge_while_a_job_is_queued_does_not_queue_another() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let antes = pending_deferred_jobs();

    hw.raise_irq(ISR_RCOMP);
    iface.handle_irq(IRQ);
    hw.raise_irq(ISR_RCOMP);
    iface.handle_irq(IRQ);

    assert_eq!(pending_deferred_jobs(), antes + 1);
    drain_deferred_jobs();
}

/// Un error de bus se cuenta y se dice, y **no** se confunde con un problema
/// de red.
///
/// El `HRESP` es el DMA leyendo o escribiendo donde no debe. Linux reinicia la
/// tarjeta entera; aqui no hay con que, asi que lo unico honrado es contarlo.
#[test]
fn a_bus_error_is_counted_rather_than_passed_over_in_silence() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let antes = iface.get_stats().rx_errors;
    hw.raise_irq(ISR_HRESP);

    iface.handle_irq(IRQ);

    assert!(
        iface.get_stats().rx_errors > antes,
        "el error de bus no se conto"
    );
    drain_deferred_jobs();
}

/// Y la mascara vuelve a su sitio **tambien si el trabajo se descarta sin
/// correr**.
///
/// `deferred_job` limita su cola y echa entradas cuando hay presion. Bajar la
/// bandera en el cuerpo del cierre, y no en un guardia, dejaria la tarjeta
/// desarmada y `poll_pending` puesto para siempre: muda sin dar ningun error.
#[test]
fn the_mask_comes_back_even_if_the_job_is_dropped_without_running() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.raise_irq(ISR_RCOMP);
    iface.handle_irq(IRQ);
    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, MACB_IRQ_WANTED);

    // Vaciar la cola es lo que ejecuta o descarta el trabajo; en los dos casos
    // el guardia se suelta.
    drain_deferred_jobs();

    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, 0);
    // Y con la bandera bajada, el flanco siguiente si encola.
    let antes = pending_deferred_jobs();
    hw.raise_irq(ISR_RCOMP);
    iface.handle_irq(IRQ);
    assert_eq!(pending_deferred_jobs(), antes + 1);
    drain_deferred_jobs();
}

// ---------------------------------------------------------------------------
// El camino de una trama
// ---------------------------------------------------------------------------

/// A lent card gives its frames to the borrower: its interrupt disarms it
/// and wakes the borrower, with no bottom half, and the borrower's look
/// hands the frames over and arms it again.
#[test]
fn a_lent_card_gives_its_frames_to_the_borrower_and_not_to_the_kernels_stack() {
    use crate::net::lent::tests::Kept;
    let hw = Hw::new();
    let iface = interfaz(&hw);
    assert!(iface.can_lend());
    let kept = Arc::new(Kept::default());
    assert_eq!(iface.lend(Some(kept.clone())), Ok(()));

    {
        let rings = iface.driver.rings.lock();
        deliver_frame(&rings, 0, TRAMA);
    }
    hw.raise_irq(ISR_RCOMP);
    let antes = pending_deferred_jobs();
    iface.handle_irq(IRQ);

    assert_eq!(hw.peek(ISR) & ISR_RCOMP, 0, "no se reconocio");
    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, MACB_IRQ_WANTED);
    assert_eq!(pending_deferred_jobs(), antes);
    assert_eq!(*kept.interrupts.lock(), 1);
    assert!(kept.frames.lock().is_empty());

    iface.poll().unwrap();
    assert_eq!(*kept.frames.lock(), vec![TRAMA.to_vec()]);
    assert_eq!(*kept.dones.lock(), 1);
    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, 0, "la tarjeta no se armo");

    // Given back, the next interrupt has its bottom half again.
    assert_eq!(iface.lend(None), Ok(()));
    hw.raise_irq(ISR_RCOMP);
    iface.handle_irq(IRQ);
    assert_eq!(pending_deferred_jobs(), antes + 1);
    assert_eq!(*kept.interrupts.lock(), 1);
    drain_deferred_jobs();
}

// ---------------------------------------------------------------------------
// Recoger los descriptores
// ---------------------------------------------------------------------------

/// Los descriptores que el MAC suelta **se recogen**, y la cola vuelve a tener
/// hueco.
///
/// Esto no existe en el GENET, donde el anillo de transmision se recoge por un
/// indice del hardware. Aqui cada descriptor lleva su bit, y si nadie los
/// recoge la cola se llena y la tarjeta se queda muda sin dar ningun error.
#[test]
fn descriptors_the_mac_released_are_reclaimed_and_the_queue_has_room_again() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let driver = iface.driver.clone();
    for _ in 0..TX_RING_SIZE - 1 {
        driver.rings.lock().send(TRAMA).expect("cabe");
    }
    assert!(!iface.can_send(), "la cola no esta llena");

    // El MAC termina las dos primeras.
    {
        let rings = driver.rings.lock();
        complete_tx(&rings, 0, 0);
        complete_tx(&rings, 1, 0);
    }

    assert!(iface.can_send(), "no se recogio nada");
}

/// Y se recogen **tambien desde el camino de la interrupcion**, que es el
/// unico que corre cuando nadie llama a `can_send`.
#[test]
fn the_interrupt_path_reclaims_the_transmit_descriptors_too() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let driver = iface.driver.clone();
    for _ in 0..TX_RING_SIZE - 1 {
        driver.rings.lock().send(TRAMA).expect("cabe");
    }
    {
        let rings = driver.rings.lock();
        complete_tx(&rings, 0, 0);
    }

    hw.raise_irq(ISR_TCOMP);
    iface.handle_irq(IRQ);
    drain_deferred_jobs();

    assert_eq!(
        driver.rings.lock().tx_in_flight(),
        TX_RING_SIZE - 2,
        "el camino de la interrupcion no recogio"
    );
}

/// Una transmision que el MAC dice haber fallado se cuenta **y se recoge
/// igual**: su descriptor es un hueco como cualquier otro.
#[test]
fn a_transmission_the_mac_says_failed_is_counted_and_still_frees_its_slot() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let driver = iface.driver.clone();
    driver.rings.lock().send(TRAMA).expect("cabe");
    {
        let rings = driver.rings.lock();
        complete_tx(&rings, 0, TX_BUF_EXHAUSTED);
    }

    let antes = iface.get_stats().tx_errors;
    assert_eq!(driver.rings.lock().reclaim_tx(), 1);
    assert!(
        iface.get_stats().tx_errors > antes,
        "la transmision fallida no se conto"
    );
    assert_eq!(driver.rings.lock().tx_in_flight(), 0);
}

// ---------------------------------------------------------------------------
// El enlace
// ---------------------------------------------------------------------------

/// **El enlace se mira en el `poll`**, no en la interrupcion.
///
/// El GENET levanta `UMAC_IRQ_LINK_EVENT` y reajusta el MAC en el camino de la
/// interrupcion. El GEM no: el `ISR_LINK` existe en el registro pero este MAC
/// no lo levanta, porque el enlace lo lleva el PHY por MDIO. Si el `poll` no
/// lo mirara, una placa cuyo enlace sube despues del arranque se quedaria con
/// el MAC a la velocidad de antes.
#[test]
fn the_link_is_looked_at_on_every_poll_because_nothing_else_will() {
    let hw = Hw::new();
    hw.set_link_down(1);
    let iface = interfaz(&hw);
    // Arranco con el enlace caido, asi que el MAC no tiene la velocidad.
    assert_eq!(hw.peek(NCFGR) & NCFGR_GBE, 0);

    // El enlace sube mas tarde, y nadie avisa.
    hw.set_link_1000_full(1);
    let _ = iface.poll();

    assert_eq!(
        hw.peek(NCFGR) & NCFGR_GBE,
        NCFGR_GBE,
        "el poll no recogio el enlace que subio"
    );
}

/// Y el `poll` vuelve a armar la tarjeta: es el camino que lleva la mascara el
/// mismo, sin el guardia de la interrupcion.
#[test]
fn the_periodic_poll_arms_the_card_itself() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    iface.driver.mac.irq_disable(MACB_IRQ_WANTED);
    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, MACB_IRQ_WANTED);

    let _ = iface.poll();

    assert_eq!(hw.peek(IMR) & MACB_IRQ_WANTED, 0);
}

// ---------------------------------------------------------------------------
// Lo que anuncia a smoltcp
// ---------------------------------------------------------------------------
