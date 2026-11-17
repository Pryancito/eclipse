//! El GENET visto desde arriba: el arranque, la interrupcion y el camino de
//! una trama a traves de smoltcp.
//!
//! El driver de los anillos ya esta probado contra el MAC falso; lo que se
//! prueba aqui es **lo que decide cuando llamarlo**, que es la mitad que en
//! los drivers de al lado no tenia un solo test.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::super::genet_fake::{Hw, DEFAULT_PHY_ADDR};
use super::super::rings::*;
use super::*;
use crate::utils::deferred_job::{drain_deferred_jobs, pending_deferred_jobs};
use crate::utils::devicetree::Node;

const IRQ: usize = 29;

/// La interfaz sobre el GENET falso de este hilo.
fn interfaz(hw: &Hw) -> GenetInterface {
    unsafe {
        genet_init(
            String::from("eth0"),
            hw.base(),
            IRQ,
            Some(DEFAULT_PHY_ADDR),
            true,
            None,
        )
    }
    .expect("un GENET sano tiene que arrancar")
}

const TRAMA: &[u8] = &[
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33, 0x08, 0x06, 1, 2, 3, 4,
];

/// Donde esta, en la lista de escrituras, la primera que toca `off`.
fn primera(hw: &Hw, off: usize) -> Option<usize> {
    hw.writes().iter().position(|(o, _)| *o == off)
}

// ---------------------------------------------------------------------------
// La MAC de la placa
// ---------------------------------------------------------------------------

#[test]
fn only_a_mac_that_can_be_a_source_address_is_kept() {
    // Todo ceros y todo unos son las dos que deja un firmware que no puso
    // ninguna. La del bit de multicast es peor: parece una MAC, y una trama
    // con ella de origen la tira el primer conmutador, asi que la tarjeta
    // transmitiria al vacio.
    assert!(mac_is_usable(&[0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33]));
    assert!(!mac_is_usable(&[0; 6]));
    assert!(!mac_is_usable(&[0xFF; 6]));
    assert!(!mac_is_usable(&[0x01, 0, 0x5E, 0, 0, 1]));
    // Y la que se inventa tiene que valer de origen y ser administrada
    // localmente, o puede chocar con una Broadcom de verdad.
    assert!(mac_is_usable(&MAC_FALLBACK));
    assert_eq!(
        MAC_FALLBACK[0] & 0x02,
        0x02,
        "no es administrada localmente"
    );
}

#[test]
fn the_interface_comes_up_with_the_mac_the_firmware_left() {
    // En la Pi es la direccion impresa en la caja: la pone el firmware en los
    // registros del UMAC antes de entregar el control.
    let hw = Hw::new();
    let placa = [0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33];
    hw.mac().write_mac_addr(&placa);
    let iface = interfaz(&hw);
    assert_eq!(iface.get_mac().as_bytes(), &placa);
    // Y sigue en los registros: el reset del UMAC va en medio del arranque.
    assert_eq!(hw.mac().read_mac_addr(), placa);
}

#[test]
fn a_firmware_that_left_no_mac_does_not_leave_the_card_at_zero() {
    // Arrancar con la MAC a ceros es una tarjeta que no recibe nada dirigido a
    // ella y cuyas tramas tira el primer conmutador.
    let hw = Hw::new();
    hw.mac().write_mac_addr(&[0; 6]);
    let iface = interfaz(&hw);
    assert_eq!(iface.get_mac().as_bytes(), &MAC_FALLBACK);
    assert_eq!(hw.mac().read_mac_addr(), MAC_FALLBACK);
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

#[test]
fn the_card_is_silenced_before_anything_else_is_touched() {
    // El GENET puede venir del cargador de arranque con interrupciones
    // armadas y pendientes: la linea estaria levantada antes de que exista el
    // manejador, y en este kernel eso es una tormenta de interrupciones que
    // nadie reconoce.
    let hw = Hw::new();
    hw.poke(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT, UMAC_IRQ_ALL);
    let _iface = interfaz(&hw);

    let mascara = primera(&hw, GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_SET)
        .expect("el arranque tiene que enmascarar");
    let reset = primera(&hw, UMAC_CMD).expect("el arranque tiene que resetear el UMAC");
    assert!(
        mascara < reset,
        "se toco el UMAC antes de callar la tarjeta ({} vs {})",
        mascara,
        reset
    );
}

#[test]
fn the_rings_are_up_before_the_mac_starts_receiving() {
    // Encender la recepcion sin huecos donde poner tramas es pedirle al MAC
    // que desborde su RBUF: las tramas se pierden dentro del MAC y lo unico
    // que queda es un contador.
    let hw = Hw::new();
    let _iface = interfaz(&hw);

    let dma = primera(&hw, RDMA_REG_BASE + DMA_CTRL).expect("el DMA tiene que arrancar");
    // La ultima escritura de `UMAC_CMD` es la que enciende rx/tx.
    // La PRIMERA escritura de `UMAC_CMD` que deja rx/tx encendidos. La ultima
    // no sirve: `adjust_link` modifica el registro despues y conserva esos
    // bits, asi que mirar la ultima daria por bueno un encendido anterior al
    // DMA.
    let encendido = hw
        .writes()
        .iter()
        .position(|(o, v)| *o == UMAC_CMD && v & (CMD_TX_EN | CMD_RX_EN) == (CMD_TX_EN | CMD_RX_EN))
        .expect("el arranque tiene que encender rx/tx");
    assert!(
        dma < encendido,
        "se encendio la recepcion antes del DMA ({} vs {})",
        dma,
        encendido
    );
    assert_eq!(
        hw.peek(UMAC_CMD) & (CMD_TX_EN | CMD_RX_EN),
        CMD_TX_EN | CMD_RX_EN
    );
}

#[test]
fn the_wanted_interrupts_are_the_last_thing_armed() {
    // Y solo esas: el registro es de set/clear, asi que lo que queda
    // enmascarado es todo lo demas.
    let hw = Hw::new();
    let _iface = interfaz(&hw);
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        !GENET_IRQ_WANTED
    );
    assert_eq!(
        hw.peek(GENET_INTRL2_1_OFF + INTRL2_CPU_MASK_STATUS),
        UMAC_IRQ_ALL,
        "las colas de prioridad no se usan y comparten la linea"
    );
}

#[test]
fn a_genet_that_is_not_a_v5_does_not_come_up() {
    // Un mapa de registros equivocado no se nota al escribirlo: se nota al
    // usarlo. La generacion es lo unico que se puede comprobar antes.
    let hw = Hw::new();
    hw.poke(SYS_REV_CTRL, 3 << 24);
    let r = unsafe { genet_init(String::from("eth0"), hw.base(), IRQ, Some(1), true, None) };
    assert!(
        r.is_err(),
        "un GENET de otra generacion tiene que rechazarse"
    );
}

#[test]
fn a_phy_that_cannot_be_read_stops_the_bring_up() {
    // Sin PHY no hay enlace, y seguir deja una interfaz que acepta tramas y no
    // las pone en ningun cable.
    let hw = Hw::new();
    hw.faults(|f| f.mdio_read_fail = true);
    let r = unsafe { genet_init(String::from("eth0"), hw.base(), IRQ, Some(1), true, None) };
    assert_eq!(r.err(), Some(DeviceError::NotReady));
}

#[test]
fn the_bus_is_scanned_when_the_device_tree_declares_no_address() {
    let hw = Hw::new();
    hw.set_phy_present(7);
    hw.faults(|f| f.mdio_mute = vec![DEFAULT_PHY_ADDR]);
    let iface = unsafe { genet_init(String::from("eth0"), hw.base(), IRQ, None, true, None) }
        .expect("un PHY en otra direccion tiene que encontrarse");
    assert_eq!(iface.driver.phy_addr, 7);
}

#[test]
fn the_link_the_phy_already_reports_is_put_into_the_mac() {
    // El enlace puede estar arriba antes de que el driver exista (el cargador
    // de arranque ya lo negocio): esperar el `LINK_EVENT` que ya paso es
    // quedarse con el MAC a la velocidad de reset.
    let hw = Hw::new();
    hw.set_link_100(DEFAULT_PHY_ADDR, false);
    let _iface = interfaz(&hw);
    let cmd = hw.peek(UMAC_CMD);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_100);
    assert_ne!(
        cmd & CMD_HD_EN,
        0,
        "un enlace a half duplex deja el MAC a full"
    );
}

// ---------------------------------------------------------------------------
// La interrupcion
// ---------------------------------------------------------------------------

#[test]
fn an_interrupt_on_another_line_is_not_answered() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);
    let marca = hw.write_mark();
    iface.handle_irq(IRQ + 1);
    assert_eq!(hw.writes_since(marca), Vec::<usize>::new());
    assert_eq!(pending_deferred_jobs(), 0);
}

#[test]
fn a_spurious_interrupt_acknowledges_nothing() {
    // Reconocer sin atender es perder un evento: el dia que la causa sea de
    // verdad, ya viene reconocida y nadie la atiende.
    let hw = Hw::new();
    let iface = interfaz(&hw);
    let marca = hw.write_mark();
    iface.handle_irq(IRQ);
    assert_eq!(hw.writes_since(marca), Vec::<usize>::new());
    assert_eq!(pending_deferred_jobs(), 0);
}

#[test]
fn an_interrupt_acks_masks_and_queues_the_work() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);

    iface.handle_irq(IRQ);

    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT),
        0,
        "sin reconocer"
    );
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        UMAC_IRQ_ALL,
        "la tarjeta tiene que quedarse callada hasta que corra el trabajo"
    );
    assert_eq!(pending_deferred_jobs(), 1);

    drain_deferred_jobs();
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        !GENET_IRQ_WANTED,
        "la mascara tiene que volver al correr el trabajo"
    );
}

#[test]
fn a_second_interrupt_does_not_queue_a_second_job() {
    // La tarjeta sigue enmascarada hasta que corra el trabajo encolado:
    // desenmascarar o encolar otro dejaria que los flancos siguientes
    // reconozcan causas que ese trabajo no va a ver.
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);
    iface.handle_irq(IRQ);
    hw.raise_irq(UMAC_IRQ_TXDMA_DONE);
    iface.handle_irq(IRQ);
    assert_eq!(pending_deferred_jobs(), 1);

    // Y la carrera de verdad, la que el `poll_pending` guarda: dos CPUs que
    // ven la misma causa antes de que ninguna haya enmascarado. Aqui se imita
    // desenmascarando a mano entre las dos entradas, porque mientras la
    // tarjeta esta enmascarada `irq_status` da cero y el manejador se va por
    // la puerta de la interrupcion espuria.
    hw.mac().irq_unmask(GENET_IRQ_WANTED);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);
    iface.handle_irq(IRQ);
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "dos CPUs en la misma causa encolaron dos trabajos"
    );

    // Y despues de correr, la siguiente si encola.
    drain_deferred_jobs();
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);
    iface.handle_irq(IRQ);
    assert_eq!(pending_deferred_jobs(), 1);
    drain_deferred_jobs();
}

#[test]
fn a_link_event_readjusts_the_mac_before_anything_is_transmitted() {
    // El enlace del GENET avisa por interrupcion. Si la velocidad cambio y el
    // MAC no se entera, lo que sale al cable sale a la velocidad vieja: el
    // enlace esta arriba, el contador de errores a cero y no pasa un paquete.
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.set_link_100(DEFAULT_PHY_ADDR, true);
    hw.raise_irq(UMAC_IRQ_LINK_UP);

    iface.handle_irq(IRQ);
    drain_deferred_jobs();

    let cmd = hw.peek(UMAC_CMD);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_100);
    assert_eq!(cmd & CMD_HD_EN, 0);
    // Y sin apagar el MAC de paso, que es lo que haria reescribir `UMAC_CMD`.
    assert_eq!(cmd & (CMD_TX_EN | CMD_RX_EN), CMD_TX_EN | CMD_RX_EN);
}

#[test]
fn a_link_that_went_down_does_not_take_the_card_down_with_it() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    hw.set_link_down(DEFAULT_PHY_ADDR);
    hw.raise_irq(UMAC_IRQ_LINK_DOWN);
    iface.handle_irq(IRQ);
    drain_deferred_jobs();
    // Un cable desenchufado no es motivo para apagar el MAC: cuando se vuelva
    // a enchufar, el `LINK_UP` lo reajusta.
    assert_eq!(
        hw.peek(UMAC_CMD) & (CMD_TX_EN | CMD_RX_EN),
        CMD_TX_EN | CMD_RX_EN
    );
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        !GENET_IRQ_WANTED
    );
}

// ---------------------------------------------------------------------------
// El camino de las tramas
// ---------------------------------------------------------------------------

#[test]
fn a_lent_card_gives_its_frames_to_the_borrower_and_not_to_the_kernels_stack() {
    use crate::net::lent::tests::Kept;
    let hw = Hw::new();
    let iface = interfaz(&hw);
    assert!(iface.can_lend());
    let kept = Arc::new(Kept::default());
    assert_eq!(iface.lend(Some(kept.clone())), Ok(()));

    hw.deliver_frame(0, TRAMA);
    hw.set_link_100(DEFAULT_PHY_ADDR, true);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE | UMAC_IRQ_LINK_UP);
    let antes = pending_deferred_jobs();
    iface.handle_irq(IRQ);

    // Acknowledged, masked, the borrower woken, and no bottom half.
    assert_eq!(hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT), 0);
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        UMAC_IRQ_ALL
    );
    assert_eq!(pending_deferred_jobs(), antes);
    assert_eq!(*kept.interrupts.lock(), 1);
    assert!(kept.frames.lock().is_empty());

    // The borrower's look: the link the interrupt spoke of, then the
    // frame, whole, and the card unmasked.
    iface.poll().unwrap();
    let cmd = hw.peek(UMAC_CMD);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_100);
    assert_eq!(*kept.frames.lock(), vec![TRAMA.to_vec()]);
    assert_eq!(*kept.dones.lock(), 1);
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        !GENET_IRQ_WANTED
    );

    // Given back, the next interrupt has its bottom half again.
    assert_eq!(iface.lend(None), Ok(()));
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE);
    iface.handle_irq(IRQ);
    assert_eq!(pending_deferred_jobs(), antes + 1);
    assert_eq!(*kept.interrupts.lock(), 1);
    drain_deferred_jobs();
}

#[test]
fn a_transmitted_frame_reaches_the_descriptor_and_is_counted() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    assert_eq!(iface.send(TRAMA), Ok(TRAMA.len()));
    assert_eq!(hw.sent_frame(0), TRAMA);
    // `NetScheme::send` es el camino de AF_PACKET, que no pasa por el TxToken:
    // los contadores de smoltcp no se mueven desde aqui.
    assert_eq!(iface.get_stats().tx_packets, 0);
}

#[test]
fn the_interface_answers_with_its_own_name() {
    let hw = Hw::new();
    let iface = interfaz(&hw);
    assert_eq!(iface.ifname(), "eth0");
    assert!(!iface.is_loopback());
    assert_eq!(Scheme::name(&iface), "genet");
}

// ---------------------------------------------------------------------------
// El arranque desde el device tree
// ---------------------------------------------------------------------------

/// El nodo del GENET de la Pi 4, con su bus MDIO y su PHY colgando.
fn nodo_de_la_pi(phy: u32, mac: Option<Vec<u8>>, modo: Option<&str>) -> Node {
    use alloc::string::ToString;
    let cells = |v: u32| v.to_be_bytes().to_vec();
    let strs = |s: &str| {
        let mut out = s.as_bytes().to_vec();
        out.push(0);
        out
    };
    let mut props = vec![("compatible".to_string(), strs("brcm,bcm2711-genet-v5"))];
    if let Some(modo) = modo {
        props.push(("phy-mode".to_string(), strs(modo)));
    }
    if let Some(mac) = mac {
        props.push(("local-mac-address".to_string(), mac));
    }
    Node {
        name: "ethernet@7d580000".to_string(),
        props,
        children: vec![Node {
            name: "mdio@e14".to_string(),
            props: vec![],
            children: vec![Node {
                name: "ethernet-phy@1".to_string(),
                props: vec![("reg".to_string(), cells(phy))],
                children: vec![],
            }],
        }],
    }
}

#[test]
fn the_tree_says_which_phy_and_which_mac() {
    // Es el camino que corre en el arranque de verdad: la sonda no sabe nada
    // del GENET, solo le pasa el nodo.
    let hw = Hw::new();
    hw.set_phy_present(1);
    let placa = vec![0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33];
    let nodo = nodo_de_la_pi(1, Some(placa.clone()), Some("rgmii-rxid"));
    let iface = unsafe { genet_from_dt(String::from("eth0"), &nodo, hw.base(), IRQ) }
        .expect("el nodo de la Pi 4 tiene que arrancar");

    assert_eq!(iface.driver.phy_addr, 1);
    assert_eq!(iface.get_mac().as_bytes(), &placa[..]);
    assert!(
        iface.driver.id_mode_dis,
        "rgmii-rxid deja el retardo al PHY"
    );
}

#[test]
fn the_mac_from_the_tree_wins_over_the_one_in_the_registers() {
    // En la Pi el firmware pone la misma en los dos sitios. Cuando no
    // coinciden, la del arbol es la que Linux usa, y es la que el usuario ve
    // en su configuracion de red.
    let hw = Hw::new();
    hw.mac().write_mac_addr(&[0xDC, 0xA6, 0x32, 0, 0, 0x11]);
    let del_arbol = vec![0xDC, 0xA6, 0x32, 0, 0, 0x22];
    let nodo = nodo_de_la_pi(1, Some(del_arbol.clone()), None);
    let iface = unsafe { genet_from_dt(String::from("eth0"), &nodo, hw.base(), IRQ) }.unwrap();
    assert_eq!(iface.get_mac().as_bytes(), &del_arbol[..]);
    // Y queda escrita en los registros, que es lo que filtra las tramas.
    assert_eq!(hw.mac().read_mac_addr(), [0xDC, 0xA6, 0x32, 0, 0, 0x22]);
}

#[test]
fn a_tree_mac_that_cannot_be_a_source_address_falls_back_to_the_registers() {
    // Un arbol con la MAC a ceros --o con el bit de multicast puesto-- no es
    // motivo para tirar la que el firmware dejo en los registros.
    let hw = Hw::new();
    let buena = [0xDC, 0xA6, 0x32, 0, 0, 0x33];
    hw.mac().write_mac_addr(&buena);
    let nodo = nodo_de_la_pi(1, Some(vec![0; 6]), None);
    let iface = unsafe { genet_from_dt(String::from("eth0"), &nodo, hw.base(), IRQ) }.unwrap();
    assert_eq!(iface.get_mac().as_bytes(), &buena);
}

#[test]
fn a_tree_that_declares_no_phy_makes_the_driver_scan_the_bus() {
    use alloc::string::ToString;
    let hw = Hw::new();
    hw.set_phy_present(7);
    hw.faults(|f| f.mdio_mute = vec![DEFAULT_PHY_ADDR]);
    let nodo = Node {
        name: "ethernet@7d580000".to_string(),
        props: vec![],
        children: vec![],
    };
    let iface = unsafe { genet_from_dt(String::from("eth0"), &nodo, hw.base(), IRQ) }
        .expect("sin PHY en el arbol se barre el bus");
    assert_eq!(iface.driver.phy_addr, 7);
}

#[test]
fn the_rgmii_mode_of_the_tree_reaches_the_mac() {
    // `rgmii-id` es el unico de los cuatro en el que el retardo lo pone el
    // MAC, y eso es un bit del `EXT_RGMII_OOB_CTRL` que se escribe al ajustar
    // el enlace. Equivocarlo da un enlace que sube y pierde tramas.
    for (modo, espera_id_mode_dis) in [("rgmii-id", false), ("rgmii-rxid", true)] {
        let hw = Hw::new();
        let nodo = nodo_de_la_pi(1, None, Some(modo));
        let iface = unsafe { genet_from_dt(String::from("eth0"), &nodo, hw.base(), IRQ) }.unwrap();
        assert_eq!(
            iface.driver.id_mode_dis, espera_id_mode_dis,
            "modo {}",
            modo
        );
        let oob = hw.peek(EXT_RGMII_OOB_CTRL);
        assert_eq!(
            oob & ID_MODE_DIS != 0,
            espera_id_mode_dis,
            "el modo {} no llego al EXT_RGMII_OOB_CTRL",
            modo
        );
    }
}
