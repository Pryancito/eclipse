//! El e1000 heredado (el NIC por defecto de QEMU) contra una ventana de
//! registros en memoria del anfitrion.
//!
//! Aqui no hace falta ninguna costura en el driver: bajo test
//! `drivers_phys_to_virt` es la identidad, asi que una direccion del monton
//! hace de BAR0 y el driver lee y escribe ahi. Y nada de lo que `E1000::new`
//! hace espera una reaccion del hardware -- el reset se espera por reloj, no
//! por el bit --, asi que el arranque entero, la recepcion y la transmision
//! se pueden recorrer tal cual.
//!
//! Lo que estos tests cubren es justo lo que no tenia test: el arranque (que
//! es el que decide si el anillo queda armado o la tarjeta nace muda), la
//! entrega de un marco y el hueco de guarda del anillo de transmision, que es
//! el bug que dejaba la transmision muerta hasta reiniciar.

use super::*;
extern crate std;

/// Una ventana de registros y el driver encima, con el reloj virtual
/// avanzando solo para que las esperas del arranque acaben.
fn tarjeta() -> (usize, E1000) {
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
    let regs = Box::leak(vec![0u32; 0x4000].into_boxed_slice());
    let base = regs.as_ptr() as usize;
    let mac = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    let dev = E1000::new(base, 0x1_0000, mac).expect("la tarjeta arranca");
    (base, dev)
}

fn reg(base: usize, r: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + r * 4) as *const u32) }
}

fn poke(base: usize, r: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + r * 4) as *mut u32, val) };
}

/// Hace de hardware: mete `data` en el buffer del hueco `slot`, pone su
/// longitud y lo marca listo.
fn entregar(dev: &E1000, slot: usize, data: &[u8]) {
    let ring = dev.rx_ring.as_ptr::<E1000RecvDesc>();
    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr(),
            dev.rx_bufs[slot].vaddr() as *mut u8,
            data.len(),
        );
        let d = &mut *ring.add(slot);
        d.len = data.len() as u16;
        d.status = 1;
        d.errors = 0;
    }
}

/// Hace de hardware: dice que ya transmitio el hueco `slot`.
fn completar(dev: &E1000, slot: usize) {
    let ring = dev.tx_ring.as_ptr::<E1000SendDesc>();
    unsafe { core::ptr::write_volatile(&mut (*ring.add(slot)).status, 1) };
}

/// Hace de hardware: dice que el hueco `slot` sigue siendo suyo.
fn ocupar(dev: &E1000, slot: usize) {
    let ring = dev.tx_ring.as_ptr::<E1000SendDesc>();
    unsafe { core::ptr::write_volatile(&mut (*ring.add(slot)).status, 0) };
}

fn marco(semilla: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| semilla.wrapping_add(i as u8)).collect()
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

#[test]
fn el_arranque_programa_los_dos_anillos() {
    let (base, dev) = tarjeta();
    assert_eq!(reg(base, E1000_TDBAL), dev.tx_ring.paddr() as u32);
    assert_eq!(reg(base, E1000_TDBAH), (dev.tx_ring.paddr() >> 32) as u32);
    assert_eq!(reg(base, E1000_TDLEN), 4096);
    assert_eq!(reg(base, E1000_TDH), 0);
    assert_eq!(reg(base, E1000_TDT), 0);
    assert_eq!(reg(base, E1000_RDBAL), dev.rx_ring.paddr() as u32);
    assert_eq!(reg(base, E1000_RDBAH), (dev.rx_ring.paddr() >> 32) as u32);
    assert_eq!(reg(base, E1000_RDLEN), 4096);
    assert_eq!(reg(base, E1000_RDH), 0);
    assert_eq!(
        reg(base, E1000_RDT),
        (NUM_DESC - 1) as u32,
        "el anillo de recepcion nace sin descriptores para el hardware"
    );
}

#[test]
fn el_arranque_enciende_la_recepcion_y_la_transmision() {
    let (base, _dev) = tarjeta();
    let tctl = reg(base, E1000_TCTL);
    assert_ne!(tctl & (1 << 1), 0, "la transmision sigue apagada");
    assert_ne!(tctl & (1 << 3), 0, "sin relleno de marcos cortos");
    let rctl = reg(base, E1000_RCTL);
    assert_ne!(rctl & (1 << 1), 0, "la recepcion sigue apagada");
    // Promiscuo de unicast y multicast: el DHCPOFFER llega a veces unicast a
    // nuestro MAC y algunos puentes le cambian el destino, asi que sin esto
    // el marco no sale de la tarjeta.
    assert_ne!(rctl & (1 << 3), 0, "sin promiscuo de unicast");
    assert_ne!(rctl & (1 << 4), 0, "sin promiscuo de multicast");
    assert_ne!(rctl & (1 << 15), 0, "sin aceptar difusion");
    assert_ne!(rctl & (1 << 26), 0, "el CRC se queda en el marco entregado");
}

#[test]
fn el_arranque_sube_el_enlace_a_duplex_completo() {
    let (base, _dev) = tarjeta();
    let ctrl = reg(base, E1000_CTRL);
    assert_ne!(ctrl & (1 << 6), 0, "no pidio subir el enlace");
    assert_ne!(ctrl & (1 << 5), 0, "no pidio deteccion automatica");
    assert_ne!(ctrl & (1 << 0), 0, "no pidio duplex completo");
    assert_ne!(ctrl & (1 << 26), 0, "el falso guarda el reset que se pidio");
}

#[test]
fn el_arranque_deja_las_interrupciones_que_quiere_y_no_mas() {
    let (base, _dev) = tarjeta();
    // El orden importa: se enmascara todo al principio y la mascara que
    // queda es la ultima escritura, con la recepcion y el cambio de enlace.
    assert_eq!(
        reg(base, E1000_IMS),
        (1 << 7) | (1 << 2),
        "la mascara final no es la de recepcion y enlace"
    );
    assert_eq!(reg(base, E1000_IMC), 0xffff_ffff);
}

#[test]
fn el_arranque_programa_el_mac_con_su_bit_de_valido() {
    let (base, _dev) = tarjeta();
    assert_eq!(reg(base, E1000_RAL), 0x1200_5452);
    let rah = reg(base, E1000_RAH);
    assert_eq!(rah & 0xFFFF, 0x5634);
    assert_ne!(
        rah & (1 << 31),
        0,
        "sin el bit de valido el filtro ignora la direccion entera"
    );
}

#[test]
fn el_arranque_limpia_la_tabla_de_multicast() {
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
    let regs = Box::leak(vec![0u32; 0x4000].into_boxed_slice());
    let base = regs.as_ptr() as usize;
    // Lo que dejo la firmware o el sistema anterior: filtros de multicast que
    // no son nuestros.
    for i in E1000_MTA..E1000_RAL {
        poke(base, i, 0xFFFF_FFFF);
    }
    let _dev = E1000::new(base, 0x1_0000, EthernetAddress([0x52, 0x54, 0, 0, 0, 1]))
        .expect("la tarjeta arranca");
    for i in E1000_MTA..E1000_RAL {
        assert_eq!(
            reg(base, i),
            0,
            "la entrada {i:#x} de la MTA quedo sin limpiar"
        );
    }
}

#[test]
fn cada_descriptor_de_recepcion_apunta_a_su_buffer() {
    let (_base, dev) = tarjeta();
    let ring = dev.rx_ring.as_ptr::<E1000RecvDesc>();
    for i in 0..NUM_DESC {
        let d = unsafe { &*ring.add(i) };
        assert_eq!(d.addr, dev.rx_bufs[i].paddr() as u64, "hueco {i}");
        assert_eq!(d.status, 0, "el hueco {i} nace diciendo que trae un marco");
    }
}

#[test]
fn cada_descriptor_de_transmision_nace_libre() {
    let (_base, dev) = tarjeta();
    let ring = dev.tx_ring.as_ptr::<E1000SendDesc>();
    for i in 0..NUM_DESC {
        let d = unsafe { &*ring.add(i) };
        assert_eq!(d.addr, dev.tx_bufs[i].paddr() as u64, "hueco {i}");
        // Un hueco sin DD parece propiedad del hardware para siempre, y antes
        // eso se tapaba con una bandera que se saltaba la comprobacion la
        // primera vuelta entera del anillo.
        assert_eq!(d.status & 1, 1, "el hueco {i} nace ocupado");
    }
}

// ---------------------------------------------------------------------------
// Recepcion
// ---------------------------------------------------------------------------

#[test]
fn un_marco_recibido_se_entrega_entero_y_devuelve_su_hueco() {
    let (base, mut dev) = tarjeta();
    let pkt = marco(0x11, 128);
    entregar(&dev, 0, &pkt);
    assert_eq!(dev.receive().as_deref(), Some(pkt.as_slice()));
    assert_eq!(
        reg(base, E1000_RDT),
        0,
        "no le devolvio el hueco al hardware: el anillo se agota"
    );
    assert_eq!(dev.rx_next_to_clean, 1);
    // Y el descriptor queda limpio, para que la vuelta siguiente no lo tome
    // por un marco nuevo.
    let d = unsafe { &*dev.rx_ring.as_ptr::<E1000RecvDesc>() };
    assert_eq!(d.status, 0);
    assert_eq!(d.errors, 0);
}

#[test]
fn sin_descriptor_listo_no_hay_marco() {
    let (_base, mut dev) = tarjeta();
    assert!(dev.receive().is_none());
    assert_eq!(dev.rx_next_to_clean, 0, "avanzo sin haber recibido nada");
}

#[test]
fn una_longitud_del_dispositivo_mas_larga_que_el_buffer_se_recorta() {
    let (_base, mut dev) = tarjeta();
    entregar(&dev, 0, &marco(0x22, 64));
    // La longitud la escribe el DISPOSITIVO: una mas larga que el buffer
    // haria un slice sobre memoria que el descriptor nunca tuvo, y eso sube
    // tal cual a la pila de red.
    let ring = dev.rx_ring.as_ptr::<E1000RecvDesc>();
    unsafe { core::ptr::write_volatile(&mut (*ring).len, 0xFFFF) };
    let pkt = dev.receive().expect("entrega el marco");
    assert_eq!(pkt.len(), RX_BUF_SIZE);
}

#[test]
fn los_marcos_se_entregan_en_orden_y_el_anillo_da_la_vuelta() {
    let (base, mut dev) = tarjeta();
    // Empieza por el ultimo hueco para que la vuelta entre en juego.
    dev.rx_next_to_clean = NUM_DESC - 1;
    let a = marco(0x33, 100);
    let b = marco(0x44, 200);
    entregar(&dev, NUM_DESC - 1, &a);
    entregar(&dev, 0, &b);
    assert_eq!(dev.receive().as_deref(), Some(a.as_slice()));
    assert_eq!(reg(base, E1000_RDT), (NUM_DESC - 1) as u32);
    assert_eq!(dev.rx_next_to_clean, 0, "no dio la vuelta");
    assert_eq!(dev.receive().as_deref(), Some(b.as_slice()));
    assert_eq!(reg(base, E1000_RDT), 0);
}

// ---------------------------------------------------------------------------
// Transmision
// ---------------------------------------------------------------------------

#[test]
fn un_marco_se_copia_a_su_buffer_y_se_somete() {
    let (base, mut dev) = tarjeta();
    let pkt = marco(0x55, 300);
    assert!(dev.send(&pkt));
    let d = unsafe { &*dev.tx_ring.as_ptr::<E1000SendDesc>() };
    assert_eq!(d.len as usize, pkt.len());
    assert_eq!(d.status & 1, 0, "lo dejo marcado como ya transmitido");
    // RS | IFCS | EOP: sin RS el hardware no devuelve el DD y el hueco no se
    // libera nunca; sin EOP el marco no acaba.
    assert_eq!(d.cmd, (1 << 3) | (1 << 1) | (1 << 0));
    let enviado =
        unsafe { core::slice::from_raw_parts(dev.tx_bufs[0].vaddr() as *const u8, pkt.len()) };
    assert_eq!(enviado, pkt.as_slice());
    assert_eq!(reg(base, E1000_TDT), 1, "no toco el timbre");
}

#[test]
fn un_marco_mas_largo_que_el_buffer_se_rechaza_sin_tocar_el_anillo() {
    let (base, mut dev) = tarjeta();
    // Recortarlo ponia medio marco corrupto en el cable mientras
    // `NetScheme::send` decia que habia escrito la longitud entera.
    assert!(!dev.send(&marco(0x66, RX_BUF_SIZE + 1)));
    assert_eq!(reg(base, E1000_TDT), 0);
    assert_eq!(dev.tx_next_to_use, 0);
}

#[test]
fn un_marco_vacio_se_rechaza() {
    let (_base, mut dev) = tarjeta();
    assert!(!dev.send(&[]));
    assert_eq!(dev.tx_next_to_use, 0);
}

#[test]
fn un_hueco_que_el_hardware_todavia_tiene_no_se_sobreescribe() {
    let (base, mut dev) = tarjeta();
    ocupar(&dev, 0);
    assert!(!dev.can_send());
    assert!(!dev.send(&marco(0x77, 64)));
    assert_eq!(reg(base, E1000_TDT), 0);
}

#[test]
fn el_hueco_de_guarda_tambien_tiene_que_estar_libre() {
    let (_base, dev) = tarjeta();
    // El hueco de la cola libre y el siguiente ocupado: el hardware lee los
    // descriptores de TDH hasta TDT sin incluirlo, asi que llenar el ultimo
    // hueco deja TDT igual a TDH y para el la cola esta VACIA.
    ocupar(&dev, 1);
    assert!(
        !dev.can_send(),
        "dio por libre una cola sin su hueco de guarda"
    );
}

#[test]
fn el_anillo_de_transmision_nunca_junta_la_cola_con_la_cabeza() {
    let (base, mut dev) = tarjeta();
    // El hardware no coge ninguno: todos los huecos nacen con DD, asi que sin
    // el hueco de guarda entrarian los 256 y TDT acabaria igual a TDH. Desde
    // el lado de la tarjeta la cola estaria vacia, los marcos no se recogen
    // nunca y la transmision queda muerta hasta reiniciar.
    let mut puestos = 0;
    while dev.send(&marco(0x88, 64)) {
        puestos += 1;
        assert!(puestos <= NUM_DESC, "entraron mas marcos que huecos");
    }
    assert_eq!(
        puestos,
        NUM_DESC - 1,
        "el anillo no guardo exactamente un hueco"
    );
    assert_ne!(
        reg(base, E1000_TDT),
        reg(base, E1000_TDH),
        "la cola acabo en la cabeza: para el hardware la cola esta vacia"
    );
}

#[test]
fn el_anillo_de_transmision_da_la_vuelta_a_medida_que_el_hardware_libera() {
    let (base, mut dev) = tarjeta();
    dev.tx_next_to_use = NUM_DESC - 1;
    assert!(dev.send(&marco(0x99, 64)));
    assert_eq!(dev.tx_next_to_use, 0, "no dio la vuelta");
    assert_eq!(reg(base, E1000_TDT), 0);
    // El hueco de antes sigue en vuelo; el hardware lo libera y se puede
    // seguir poniendo.
    completar(&dev, NUM_DESC - 1);
    assert!(dev.send(&marco(0xAA, 64)));
    assert_eq!(reg(base, E1000_TDT), 1);
}

// ---------------------------------------------------------------------------
// Interrupciones
// ---------------------------------------------------------------------------

#[test]
fn una_interrupcion_sin_causa_no_se_atiende() {
    let (base, mut dev) = tarjeta();
    poke(base, E1000_ICR, 0);
    assert!(
        !dev.handle_interrupt(),
        "atendio una interrupcion que no era suya"
    );
}

#[test]
fn una_interrupcion_de_recepcion_se_atiende() {
    let (base, mut dev) = tarjeta();
    poke(base, E1000_ICR, 1 << 7);
    assert!(dev.handle_interrupt());
}

#[test]
fn un_cambio_de_enlace_se_atiende_y_mira_el_estado() {
    let (base, mut dev) = tarjeta();
    poke(base, E1000_ICR, 1 << 2);
    poke(base, E1000_STATUS, 1 << 1);
    assert!(dev.handle_interrupt());
}
