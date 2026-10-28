//! Los anillos DMA del GENET: el reparto de descriptores, el arranque de las
//! dos colas y el camino de una trama.

use super::super::genet_fake::Hw;
use super::*;

/// Los anillos de un GENET recien reseteado.
fn anillos(hw: &Hw) -> GenetRings {
    let mac = hw.mac();
    mac.reset_umac();
    GenetRings::new(mac).expect("hay memoria para los buffers")
}

const TRAMA: &[u8] = &[
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // destino: broadcast
    0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33, // origen
    0x08, 0x06, // ARP
    1, 2, 3, 4, 5, 6, 7, 8,
];

// ---------------------------------------------------------------------------
// El mapa
// ---------------------------------------------------------------------------

#[test]
fn the_ring_registers_sit_behind_the_descriptors() {
    // Los 256 descriptores de 12 bytes ocupan 0xC00, y los registros de las
    // colas empiezan justo detras. Una base calculada sin contarlos caeria
    // encima de los ultimos descriptores y escribir un registro de cola
    // corromperia una trama.
    assert_eq!(GENET_RDMA_REG_OFF, GENET_RX_OFF + 0xC00);
    assert_eq!(GENET_TDMA_REG_OFF, GENET_TX_OFF + 0xC00);
    assert_eq!(TOTAL_DESCS * DMA_DESC_SIZE, 0xC00);
}

#[test]
fn the_default_queue_is_the_seventeenth_and_its_registers_the_last() {
    // La cola 16 es la ultima de las diecisiete, y los registros del DMA van
    // detras de todas ellas.
    assert_eq!(RDMA_RING_REG_BASE, GENET_RDMA_REG_OFF + 16 * 0x40);
    assert_eq!(RDMA_REG_BASE, GENET_RDMA_REG_OFF + 17 * 0x40);
    assert_eq!(TDMA_RING_REG_BASE, GENET_TDMA_REG_OFF + 16 * 0x40);
    assert_eq!(TDMA_REG_BASE, GENET_TDMA_REG_OFF + 17 * 0x40);
}

#[test]
fn the_producer_and_the_consumer_are_swapped_between_rx_and_tx() {
    // Quien produce no es el mismo: en recepcion produce el MAC y consume el
    // driver, en transmision al reves. Cambiarlos de sitio deja al driver
    // leyendo su propio indice y creyendo que no llega nada nunca.
    assert_eq!(RDMA_PROD_INDEX - RDMA_RING_REG_BASE, 0x08);
    assert_eq!(RDMA_CONS_INDEX - RDMA_RING_REG_BASE, 0x0C);
    assert_eq!(TDMA_CONS_INDEX - TDMA_RING_REG_BASE, 0x08);
    assert_eq!(TDMA_PROD_INDEX - TDMA_RING_REG_BASE, 0x0C);
}

#[test]
fn every_ring_register_lands_inside_the_window_the_driver_maps() {
    // La ventana que el device tree mapea tiene que dar para el registro mas
    // alto que el driver toca.
    let mas_alto = [
        RDMA_REG_BASE + DMA_SCB_BURST_SIZE,
        TDMA_REG_BASE + DMA_SCB_BURST_SIZE,
        TDMA_PROD_INDEX,
        TDMA_FLOW_PERIOD,
        TDMA_RING_REG_BASE + DMA_RING_READ_PTR,
    ]
    .iter()
    .copied()
    .max()
    .unwrap();
    assert!(mas_alto < 0x1_0000, "{:#x} se sale de la ventana", mas_alto);
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

#[test]
fn every_rx_descriptor_points_at_its_own_buffer_and_belongs_to_the_mac() {
    let hw = Hw::new();
    let anillos = anillos(&hw);
    let esperado = ((RX_BUF_LENGTH as u32) << DMA_BUFLENGTH_SHIFT) | DMA_OWN;
    let mut vistas = alloc::collections::BTreeSet::new();
    for slot in 0..TOTAL_DESCS {
        let desc = GENET_RX_OFF + slot * DMA_DESC_SIZE;
        let lo = hw.peek(desc + DMA_DESC_ADDRESS_LO) as usize;
        let hi = hw.peek(desc + DMA_DESC_ADDRESS_HI) as usize;
        let paddr = lo | (hi << 32);
        assert_ne!(paddr, 0, "el descriptor {} no apunta a nada", slot);
        assert!(
            vistas.insert(paddr),
            "el descriptor {} comparte buffer con otro",
            slot
        );
        assert_eq!(
            hw.rx_len_stat(slot),
            esperado,
            "el descriptor {slot} no es un hueco del MAC"
        );
    }
    // Los 256 buffers son consecutivos y de 2 KiB.
    let primero = *vistas.first().unwrap();
    let ultimo = *vistas.last().unwrap();
    assert_eq!(ultimo - primero, (TOTAL_DESCS - 1) * RX_BUF_LENGTH);
    drop(anillos);
}

#[test]
fn the_rings_declare_their_size_in_words_and_the_last_index_not_the_count() {
    // `DMA_END_ADDR` es el ultimo indice de palabra de 32 bits, no la cuenta
    // de bytes: pasarle los bytes le daria al MAC cuatro veces el anillo.
    let hw = Hw::new();
    let anillos = anillos(&hw);
    let fin = (TOTAL_DESCS * DMA_DESC_SIZE / 4 - 1) as u32;
    assert_eq!(fin, 767);
    for base in [RDMA_RING_REG_BASE, TDMA_RING_REG_BASE] {
        assert_eq!(hw.peek(base + DMA_START_ADDR), 0);
        assert_eq!(hw.peek(base + DMA_END_ADDR), fin);
        assert_eq!(
            hw.peek(base + DMA_RING_BUF_SIZE),
            ((TOTAL_DESCS as u32) << DMA_RING_SIZE_SHIFT) | RX_BUF_LENGTH as u32
        );
    }
    drop(anillos);
}

#[test]
fn only_the_default_queue_is_turned_on() {
    // Encender una cola que no se atiende es dejar tramas en un anillo que
    // nadie lee.
    let hw = Hw::new();
    let anillos = anillos(&hw);
    assert_eq!(hw.peek(RDMA_REG_BASE + DMA_RING_CFG), 1 << DEFAULT_Q);
    assert_eq!(hw.peek(TDMA_REG_BASE + DMA_RING_CFG), 1 << DEFAULT_Q);
    drop(anillos);
}

#[test]
fn the_dma_comes_up_enabled_for_the_default_queue() {
    let hw = Hw::new();
    let anillos = anillos(&hw);
    let esperado = (1 << (DEFAULT_Q as u32 + DMA_RING_BUF_EN_SHIFT)) | DMA_EN;
    assert_eq!(hw.peek(TDMA_REG_BASE + DMA_CTRL), esperado);
    assert_eq!(hw.peek(RDMA_REG_BASE + DMA_CTRL) & esperado, esperado);
    drop(anillos);
}

#[test]
fn the_consumer_is_aligned_on_the_producer_the_mac_already_had() {
    // El indice del productor es del MAC y de solo lectura: no se puede poner
    // a cero. Arrancar en el cero con un productor heredado de un arranque
    // anterior seria leer como tramas los 256 descriptores que el MAC no ha
    // tocado.
    let hw = Hw::new();
    hw.poke(RDMA_PROD_INDEX, 0x1234);
    hw.poke(TDMA_CONS_INDEX, 0x4321);
    let anillos = anillos(&hw);
    assert_eq!(hw.peek(RDMA_CONS_INDEX), 0x1234);
    assert_eq!(hw.peek(TDMA_PROD_INDEX), 0x4321);
    // Y no hay nada pendiente ni en vuelo.
    assert_eq!(anillos.rx_pending(), 0);
    assert_eq!(anillos.tx_in_flight(), 0);
}

#[test]
fn the_slot_the_driver_starts_at_is_the_one_the_producer_points_to() {
    // El indice es de 16 bits y los descriptores 256: el hueco es el resto.
    // Empezar por el cero con un productor en 0x1234 leeria el descriptor
    // equivocado en la primera trama.
    let hw = Hw::new();
    hw.poke(RDMA_PROD_INDEX, 0x1234);
    let mut anillos = anillos(&hw);
    assert_eq!(0x1234 % TOTAL_DESCS, 0x34);
    hw.deliver_frame(0x34, TRAMA);
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
}

#[test]
fn the_tx_done_threshold_is_one_frame() {
    // Un umbral mas alto deja la ultima trama de una rafaga sin aviso.
    let hw = Hw::new();
    let anillos = anillos(&hw);
    assert_eq!(hw.peek(TDMA_RING_REG_BASE + DMA_MBUF_DONE_THRESH), 1);
    drop(anillos);
}

#[test]
fn no_memory_for_the_buffers_is_an_error_not_a_ring_pointing_at_zero() {
    use crate::utils::host_hooks::FAIL_ALLOC;
    use core::sync::atomic::Ordering;
    let hw = Hw::new();
    let mac = hw.mac();
    mac.reset_umac();
    FAIL_ALLOC.store(true, Ordering::Relaxed);
    let r = GenetRings::new(mac);
    FAIL_ALLOC.store(false, Ordering::Relaxed);
    assert_eq!(r.err(), Some(DeviceError::DmaError));
    // Y el DMA no se ha quedado encendido sobre descriptores sin buffer.
    assert_eq!(hw.peek(RDMA_REG_BASE + DMA_CTRL) & DMA_EN, 0);
}

#[test]
fn running_out_of_memory_on_the_second_region_is_also_an_error() {
    // La de recepcion se consigue y la de transmision no: el camino que deja
    // media tarjeta montada.
    use crate::utils::host_hooks::{ALLOC_CALLS, FAIL_ALLOC_AFTER};
    use core::sync::atomic::Ordering;
    let hw = Hw::new();
    let mac = hw.mac();
    mac.reset_umac();
    FAIL_ALLOC_AFTER.store(ALLOC_CALLS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    let r = GenetRings::new(mac);
    FAIL_ALLOC_AFTER.store(usize::MAX, Ordering::Relaxed);
    assert_eq!(r.err(), Some(DeviceError::DmaError));
}

// ---------------------------------------------------------------------------
// Recepcion
// ---------------------------------------------------------------------------

#[test]
fn a_frame_comes_out_without_the_two_alignment_bytes() {
    // El `RBUF_ALIGN_2B` del reset hace que el MAC deje dos bytes delante, y
    // los cuenta en la longitud. Devolverlos haria que toda trama llegara
    // arriba con dos bytes de basura por delante y dos de largo.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame(0, TRAMA);
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
}

#[test]
fn an_empty_ring_is_not_ready_and_not_a_zero_length_frame() {
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    assert!(!anillos.can_recv());
    assert_eq!(anillos.recv(&mut [0u8; 128]), Err(DeviceError::NotReady));
}

#[test]
fn taking_a_frame_hands_the_slot_back_with_its_whole_buffer() {
    // Dejar el descriptor con la longitud de la trama que acaba de llegar lo
    // convertiria en un hueco de ese tamaño para siempre, y la siguiente
    // trama larga se perderia o se cortaria.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame(0, TRAMA);
    assert_eq!(anillos.recv(&mut [0u8; 128]), Ok(TRAMA.len()));
    assert_eq!(
        hw.rx_len_stat(0),
        ((RX_BUF_LENGTH as u32) << DMA_BUFLENGTH_SHIFT) | DMA_OWN
    );
    assert_eq!(hw.peek(RDMA_CONS_INDEX), 1);
}

#[test]
fn the_slots_are_walked_in_order_and_wrap_around() {
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    let mut buf = [0u8; 64];
    // Una vuelta entera mas una: el hueco 0 se usa dos veces.
    for i in 0..=TOTAL_DESCS {
        let slot = i % TOTAL_DESCS;
        let trama = [slot as u8; 32];
        hw.deliver_frame(slot, &trama);
        assert_eq!(anillos.recv(&mut buf), Ok(32), "en la vuelta {i}");
        assert_eq!(buf[0], slot as u8, "la trama {i} salio del hueco de otra");
    }
}

#[test]
fn a_buffer_too_small_does_not_eat_the_frame() {
    // La trama sigue ahi para el que venga con sitio: descartarla seria
    // perder un paquete por un buffer del llamante.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame(0, TRAMA);
    assert_eq!(
        anillos.recv(&mut [0u8; 4]),
        Err(DeviceError::BufferTooSmall)
    );
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
}

#[test]
fn a_descriptor_with_an_impossible_length_is_dropped_not_spun_on() {
    // Una longitud por debajo de los dos bytes de alineacion, restada sin
    // mirar, da un numero gigante y una copia fuera del buffer. Y el hueco
    // tiene que soltarse: quedarse en el es dejar la recepcion parada para
    // siempre por un descriptor.
    for malo in [0u32, 1, 2, (RX_BUF_LENGTH + 1) as u32] {
        let hw = Hw::new();
        let mut anillos = anillos(&hw);
        hw.deliver_frame(0, TRAMA);
        hw.poke(
            GENET_RX_OFF + DMA_DESC_LENGTH_STATUS,
            malo << DMA_BUFLENGTH_SHIFT,
        );
        assert_eq!(
            anillos.recv(&mut [0u8; 2048]),
            Err(DeviceError::IoError),
            "la longitud {malo} tenia que ser un error"
        );
        // Y la recepcion sigue viva: la trama siguiente entra.
        hw.deliver_frame(1, TRAMA);
        let mut buf = [0u8; 128];
        assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    }
}

#[test]
fn the_pending_count_survives_the_sixteen_bit_wrap() {
    // Los dos indices son contadores libres que dan la vuelta en 0x10000, que
    // no es multiplo de 256. El driver arranca con su consumidor en 0xFFFF y
    // la trama siguiente hace dar la vuelta al productor: una resta sin los 16
    // bits da 0xFFFF0001 huecos, y comparar `cons < prod` --que es lo que hace
    // U-Boot-- deja de ver tramas en esa vuelta entera.
    let hw = Hw::new();
    hw.poke(RDMA_PROD_INDEX, 0xFFFF);
    let mut anillos = anillos(&hw);
    assert_eq!(anillos.rx_pending(), 0, "arranca alineado");

    hw.deliver_frame(0xFFFF % TOTAL_DESCS, TRAMA);
    assert_eq!(hw.peek(RDMA_PROD_INDEX), 0, "el productor dio la vuelta");
    assert_eq!(anillos.rx_pending(), 1);

    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
    assert_eq!(anillos.rx_pending(), 0);
    // Y el consumidor que se le confirma al MAC tambien dio la vuelta, no se
    // fue a 0x10000.
    assert_eq!(hw.peek(RDMA_CONS_INDEX), 0);
}

// ---------------------------------------------------------------------------
// Transmision
// ---------------------------------------------------------------------------

#[test]
fn a_sent_frame_lands_in_the_descriptor_with_sop_eop_and_the_crc_asked_for() {
    // Sin `SOP`/`EOP` el MAC espera mas trozos y no transmite nada; sin
    // `APPEND_CRC` la trama sale sin FCS y todo el mundo la tira.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    assert_eq!(anillos.send(TRAMA), Ok(TRAMA.len()));
    assert_eq!(hw.sent_frame(0), TRAMA);
    let ls = hw.tx_len_stat(0);
    assert_ne!(ls & DMA_SOP, 0);
    assert_ne!(ls & DMA_EOP, 0);
    assert_ne!(ls & DMA_TX_APPEND_CRC, 0);
    assert_eq!(
        (ls >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK,
        TRAMA.len() as u32
    );
    assert_eq!(
        (ls >> DMA_TX_QTAG_SHIFT) & DMA_TX_QTAG_ALL,
        DMA_TX_QTAG_ALL,
        "la etiqueta de cola que el UMAC espera"
    );
}

#[test]
fn the_bell_is_rung_once_per_frame_and_counts_the_frame_as_in_flight() {
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    assert_eq!(anillos.tx_in_flight(), 0);
    anillos.send(TRAMA).expect("cabe");
    assert_eq!(hw.peek(TDMA_PROD_INDEX), 1);
    assert_eq!(anillos.tx_in_flight(), 1);
    // El MAC la termina y deja de estar en vuelo.
    hw.complete_tx(1);
    assert_eq!(anillos.tx_in_flight(), 0);
}

#[test]
fn send_does_not_wait_for_the_mac() {
    // U-Boot se queda girando sobre el indice del consumidor hasta que la
    // trama sale; aqui el que la escribio no tiene por que quedarse mirando el
    // cable. El falso no termina ninguna transmision por su cuenta, asi que si
    // el driver esperase, esto no volveria.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    for _ in 0..8 {
        anillos.send(TRAMA).expect("cabe");
    }
    assert_eq!(anillos.tx_in_flight(), 8);
}

#[test]
fn a_full_ring_refuses_instead_of_overwriting_a_frame_in_flight() {
    // Las 256 que caben entran; la 257 pisaria el descriptor de la primera,
    // que el MAC todavia no ha leido.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    for i in 0..TOTAL_DESCS {
        assert!(
            anillos.send(TRAMA).is_ok(),
            "la trama {} tenia que caber",
            i
        );
    }
    assert!(!anillos.can_send());
    assert_eq!(anillos.send(TRAMA), Err(DeviceError::NotReady));
    // El MAC termina una y vuelve a haber sitio para exactamente una.
    hw.complete_tx(1);
    assert!(anillos.can_send());
    assert!(anillos.send(TRAMA).is_ok());
    assert_eq!(anillos.send(TRAMA), Err(DeviceError::NotReady));
}

#[test]
fn the_tx_slots_wrap_around_too() {
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    for i in 0..=TOTAL_DESCS {
        let trama = [i as u8; 60];
        anillos.send(&trama).expect("cabe");
        hw.complete_tx(1);
        assert_eq!(
            hw.sent_frame(i % TOTAL_DESCS)[0],
            i as u8,
            "la trama {i} fue al descriptor de otra"
        );
    }
}

#[test]
fn a_frame_that_is_too_long_or_empty_is_refused() {
    // Mas de 1514 no cabe en una trama Ethernet sin la FCS, que la pone el
    // MAC; y una trama vacia es un descriptor con longitud cero, que el MAC no
    // transmite y deja el indice del productor adelantado para siempre.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    assert_eq!(anillos.send(&[]), Err(DeviceError::InvalidParam));
    assert_eq!(
        anillos.send(&[0u8; MAX_TX_FRAME + 1]),
        Err(DeviceError::InvalidParam)
    );
    assert_eq!(hw.peek(TDMA_PROD_INDEX), 0, "no se toco el timbre");
    // Y la mas larga que si cabe, cabe.
    assert_eq!(anillos.send(&[0u8; MAX_TX_FRAME]), Ok(MAX_TX_FRAME));
}

#[test]
fn the_in_flight_count_survives_the_sixteen_bit_wrap() {
    let hw = Hw::new();
    hw.poke(TDMA_CONS_INDEX, 0xFFFF);
    let mut anillos = anillos(&hw);
    // El productor arranca alineado en 0xFFFF y la primera trama lo hace dar
    // la vuelta.
    assert_eq!(anillos.tx_in_flight(), 0);
    anillos.send(TRAMA).expect("cabe");
    assert_eq!(hw.peek(TDMA_PROD_INDEX), 0, "el productor dio la vuelta");
    assert_eq!(
        anillos.tx_in_flight(),
        1,
        "una resta sin los 16 bits daria 0 aqui y el anillo pareceria vacio"
    );
    hw.complete_tx(1);
    assert_eq!(anillos.tx_in_flight(), 0);
}

#[test]
fn the_descriptor_is_completed_after_its_address_not_before() {
    // El estado y la longitud son lo que le dice al MAC que el descriptor esta
    // listo. Escribirlos antes de la direccion le ofreceria una trama que
    // apunta a donde apuntaba la anterior, que es el paquete de otro saliendo
    // por el cable. En un registro en memoria eso no deja rastro --solo queda
    // el ultimo valor--, asi que lo que se mira es el orden de las escrituras.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    let marca = hw.write_mark();
    anillos.send(TRAMA).expect("cabe");

    let desc = GENET_TX_OFF;
    let secuencia = hw.writes_since(marca);
    let pos = |off: usize| {
        secuencia
            .iter()
            .position(|o| *o == off)
            .unwrap_or_else(|| panic!("no se escribio {:#x}", off))
    };
    let lo = pos(desc + DMA_DESC_ADDRESS_LO);
    let hi = pos(desc + DMA_DESC_ADDRESS_HI);
    let estado = pos(desc + DMA_DESC_LENGTH_STATUS);
    let timbre = pos(TDMA_PROD_INDEX);
    assert!(
        lo < estado && hi < estado,
        "la direccion va antes del estado: lo={} hi={} estado={}",
        lo,
        hi,
        estado
    );
    assert!(
        estado < timbre,
        "el timbre va despues del descriptor completo: estado={} timbre={}",
        estado,
        timbre
    );
}

#[test]
fn each_tx_slot_uses_its_own_buffer() {
    // Transmitir siempre desde el buffer del hueco 0 haria que la segunda
    // trama pisara la primera, que el MAC todavia no ha leido.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    anillos.send(TRAMA).expect("cabe");
    anillos.send(&[0xAB; 64]).expect("cabe");
    let primera = hw.peek(GENET_TX_OFF + DMA_DESC_ADDRESS_LO);
    let segunda = hw.peek(GENET_TX_OFF + DMA_DESC_SIZE + DMA_DESC_ADDRESS_LO);
    assert_ne!(
        primera, segunda,
        "cada descriptor apunta a su propio buffer de transmision"
    );
    assert_eq!(hw.sent_frame(0), TRAMA);
    assert_eq!(hw.sent_frame(1), [0xAB; 64]);
}

#[test]
fn rx_and_tx_do_not_share_a_buffer() {
    // Un solo `DmaRegion` para los dos anillos haria que una trama que sale
    // pisara una que entra.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    anillos.send(TRAMA).expect("cabe");
    let tx = hw.peek(GENET_TX_OFF + DMA_DESC_ADDRESS_LO);
    let rx = hw.peek(GENET_RX_OFF + DMA_DESC_ADDRESS_LO);
    assert_ne!(tx, rx);
}

#[test]
fn a_frame_in_and_a_frame_out_do_not_disturb_each_other() {
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame(0, TRAMA);
    anillos.send(&[0xCD; 100]).expect("cabe");
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
    assert_eq!(hw.sent_frame(0), [0xCD; 100]);
}

// ---------------------------------------------------------------------------
// Los bits de error del descriptor de recepcion
// ---------------------------------------------------------------------------

#[test]
fn a_frame_the_mac_marked_as_bad_is_not_handed_up() {
    // Cada bit, por separado. El MAC pone aqui lo que le paso a la trama, y
    // una con el CRC malo o cortada por un desbordamiento llega con una
    // longitud perfectamente creible: si no se miran estos bits, la pila IP
    // recibe basura que el hardware ya habia dicho que estaba mal, y el
    // sintoma no es un contador de errores sino conexiones que se caen.
    for (bit, nombre) in [
        (DMA_RX_LG, "DMA_RX_LG"),
        (DMA_RX_NO, "DMA_RX_NO"),
        (DMA_RX_RXER, "DMA_RX_RXER"),
        (DMA_RX_CRC_ERROR, "DMA_RX_CRC_ERROR"),
        (DMA_RX_OV, "DMA_RX_OV"),
    ] {
        let hw = Hw::new();
        let mut anillos = anillos(&hw);
        hw.deliver_frame_with_status(0, TRAMA, bit);
        assert_eq!(
            anillos.recv(&mut [0u8; 128]),
            Err(DeviceError::IoError),
            "{} tenia que rechazar la trama",
            nombre
        );
        assert_eq!(anillos.rx_errors(), 1, "{} no se conto", nombre);
    }
}

#[test]
fn a_bad_frame_releases_its_slot_and_the_next_one_still_arrives() {
    // Quedarse en el descriptor malo es dejar la recepcion parada para
    // siempre por una trama con el CRC roto, que es justo la que llega sola
    // con un cable marginal.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame_with_status(0, TRAMA, DMA_RX_CRC_ERROR);
    assert_eq!(anillos.recv(&mut [0u8; 128]), Err(DeviceError::IoError));
    // El hueco vuelve al MAC con su buffer entero y el consumidor avanza.
    assert_eq!(
        hw.rx_len_stat(0),
        ((RX_BUF_LENGTH as u32) << DMA_BUFLENGTH_SHIFT) | DMA_OWN
    );
    assert_eq!(hw.peek(RDMA_CONS_INDEX), 1);
    assert_eq!(anillos.rx_pending(), 0);

    hw.deliver_frame(1, TRAMA);
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(&buf[..TRAMA.len()], TRAMA);
}

#[test]
fn several_bad_bits_at_once_count_as_one_frame() {
    // El MAC puede marcar varias cosas de la misma trama. Es una trama
    // perdida, no cinco.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame_with_status(0, TRAMA, DMA_RX_CRC_ERROR | DMA_RX_OV | DMA_RX_RXER);
    assert_eq!(anillos.recv(&mut [0u8; 128]), Err(DeviceError::IoError));
    assert_eq!(anillos.rx_errors(), 1);
}

#[test]
fn a_broadcast_or_multicast_frame_is_not_an_error() {
    // Estos dos bits dicen de que clase era la trama, no que este mal.
    // Meterlos en `DMA_RX_ERRORS` tira todo el ARP y todo el DHCP: la tarjeta
    // parece viva, cuenta errores y no habla con nadie.
    for (bit, nombre) in [
        (DMA_RX_BRDCAST, "DMA_RX_BRDCAST"),
        (DMA_RX_MULT, "DMA_RX_MULT"),
        (DMA_RX_BRDCAST | DMA_RX_MULT, "los dos"),
    ] {
        let hw = Hw::new();
        let mut anillos = anillos(&hw);
        hw.deliver_frame_with_status(0, TRAMA, bit);
        let mut buf = [0u8; 128];
        assert_eq!(
            anillos.recv(&mut buf),
            Ok(TRAMA.len()),
            "{} se trato como un error",
            nombre
        );
        assert_eq!(&buf[..TRAMA.len()], TRAMA);
        assert_eq!(anillos.rx_errors(), 0, "{} se conto como error", nombre);
    }
}

#[test]
fn a_bad_frame_does_not_need_room_in_the_callers_buffer() {
    // Un buffer pequeño no descarta una trama buena (eso lo fija
    // `a_buffer_too_small_does_not_eat_the_frame`), pero una trama que el MAC
    // ya dio por mala hay que soltarla aunque el llamante no tenga sitio: si
    // no, un buffer corto la deja atascada en el anillo.
    //
    // Este es tambien el unico test que fija el ORDEN de la comprobacion: si
    // los bits de error se miraran despues del tamaño del buffer, esto
    // devolveria `BufferTooSmall` y la trama mala se quedaria en el anillo.
    // Respecto a la validacion de la longitud el orden no se nota desde
    // fuera, y por eso no hay test que lo pida.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame_with_status(0, TRAMA, DMA_RX_CRC_ERROR);
    assert_eq!(anillos.recv(&mut [0u8; 4]), Err(DeviceError::IoError));
    assert_eq!(
        anillos.rx_pending(),
        0,
        "la trama mala se quedo en el anillo"
    );
}

#[test]
fn the_discard_counter_does_not_inflate_the_pending_count() {
    // La mitad de arriba de `RDMA_PROD_INDEX` es `DMA_P_INDEX_DISCARD_CNT`:
    // las tramas que el MAC tiro por no tener hueco. Leer el registro entero
    // como indice da miles de tramas pendientes que no existen y el driver se
    // pone a entregar descriptores vacios.
    let hw = Hw::new();
    let mut anillos = anillos(&hw);
    hw.deliver_frame(0, TRAMA);
    // 7 descartes contados por el MAC, en la mitad de arriba.
    hw.poke(RDMA_PROD_INDEX, hw.peek(RDMA_PROD_INDEX) | (7 << 16));
    assert_eq!(anillos.rx_pending(), 1);
    assert_eq!(anillos.rx_discarded(), 7);
    let mut buf = [0u8; 128];
    assert_eq!(anillos.recv(&mut buf), Ok(TRAMA.len()));
    assert_eq!(anillos.rx_pending(), 0);
}
