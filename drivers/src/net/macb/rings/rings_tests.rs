//! Los anillos del GEM: el formato de los descriptores, los ordenes de
//! escritura y el camino de la trama.
//!
//! El MAC falso de [`macb_fake`](super::super::macb_fake) no toca los
//! descriptores, porque **no son suyos**: viven en memoria que pidio
//! [`MacbRings`], no en la ventana de registros. Lo que hace el MAC con ellos
//! lo hacen aqui [`deliver_frame`] y [`complete_tx`], que son las dos unicas
//! cosas que el hardware haria y el anfitrion no.

use super::super::macb_fake::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// La direccion que un descriptor declara, juntando las dos mitades.
fn descriptor_addr(ring: &Ring, slot: usize, addr64: bool) -> usize {
    let lo = ring.read(slot, DESC_ADDR) & !(RX_USED | RX_WRAP);
    let hi = if addr64 {
        ring.read(slot, DESC_ADDRH) as usize
    } else {
        0
    };
    (hi << 32) | lo as usize
}

/// Unos anillos recien arrancados sobre el GEM falso de este hilo.
fn anillos(hw: &Hw) -> MacbRings {
    let mac = hw.mac();
    mac.reset_hw();
    mac.init_config(&MacbConfig::default());
    MacbRings::new(mac).expect("los anillos arrancan")
}

/// Mete una trama en un hueco de recepcion como lo haria el MAC: la copia en
/// el buffer del descriptor dejando delante los dos bytes de alineacion,
/// declara la longitud **con** esos dos bytes, y sube el `RX_USED`.
fn deliver_frame(rings: &MacbRings, slot: usize, frame: &[u8]) {
    deliver_frame_with_ctrl(rings, slot, frame, RX_SOF | RX_EOF);
}

/// Lo mismo, pero con los bits de control que el MAC quiera poner.
fn deliver_frame_with_ctrl(rings: &MacbRings, slot: usize, frame: &[u8], ctrl: u32) {
    let (vaddr, _) = rings.rx_buf_at(slot);
    unsafe {
        core::ptr::write_bytes(vaddr as *mut u8, 0, RX_IP_ALIGN as usize + frame.len());
        core::ptr::copy_nonoverlapping(
            frame.as_ptr(),
            (vaddr + RX_IP_ALIGN as usize) as *mut u8,
            frame.len(),
        );
    }
    let declarada = frame.len() as u32 + RX_IP_ALIGN;
    rings
        .rx
        .write(slot, DESC_CTRL, (declarada & RX_FRMLEN_MASK) | ctrl);
    let addr = rings.rx.read(slot, DESC_ADDR);
    rings.rx.write(slot, DESC_ADDR, addr | RX_USED);
}

/// Da por terminada la transmision de un hueco, con los bits de estado que el
/// MAC quiera poner.
fn complete_tx(rings: &MacbRings, slot: usize, estado: u32) {
    let ctrl = rings.tx.read(slot, DESC_CTRL);
    rings.tx.write(slot, DESC_CTRL, ctrl | TX_USED | estado);
}

/// La trama que el driver dejo en un hueco de transmision, con la longitud
/// que declara el descriptor.
///
/// Se comprueba de paso que el descriptor apunta **al buffer de ese hueco**:
/// leer por la direccion del descriptor seria mas fiel al MAC, pero esa es una
/// direccion fisica y en el anfitrion no se puede desreferenciar (y la palabra
/// baja del descriptor solo lleva sus 32 bits de abajo).
fn sent_frame(rings: &MacbRings, slot: usize) -> Vec<u8> {
    let ctrl = rings.tx.read(slot, DESC_CTRL);
    let len = (ctrl & TX_FRMLEN_MASK) as usize;
    let (vaddr, paddr) = rings.tx_buf_at(slot);
    assert_eq!(
        descriptor_addr(&rings.tx, slot, rings.addr64),
        paddr,
        "el hueco {slot} no apunta a su buffer"
    );
    if len == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; len];
    unsafe { core::ptr::copy_nonoverlapping(vaddr as *const u8, out.as_mut_ptr(), len) };
    out
}

// ---------------------------------------------------------------------------
// El formato del descriptor
// ---------------------------------------------------------------------------

/// Los bits de los descriptores, con el nombre de la constante de Linux.
#[test]
fn the_descriptor_bits_are_the_ones_linux_publishes() {
    // macb.h: RX_USED 0, RX_WRAP 1, RX_WADDR 2/30.
    assert_eq!(RX_USED, 1 << 0, "MACB_RX_USED_OFFSET");
    assert_eq!(RX_WRAP, 1 << 1, "MACB_RX_WRAP_OFFSET");
    assert_eq!(RX_ADDR_MASK, 0xFFFF_FFFC, "MACB_RX_WADDR 2/30");
    // RX_FRMLEN 0/12, RX_SOF 14, RX_EOF 15.
    assert_eq!(RX_FRMLEN_MASK, 0x0FFF, "MACB_RX_FRMLEN_MASK");
    assert_eq!(RX_SOF, 1 << 14, "MACB_RX_SOF_OFFSET");
    assert_eq!(RX_EOF, 1 << 15, "MACB_RX_EOF_OFFSET");
    // GEM_TX_FRMLEN 0/14, TX_LAST 15, TX_NOCRC 16, TX_WRAP 30, TX_USED 31.
    assert_eq!(TX_FRMLEN_MASK, 0x3FFF, "GEM_TX_FRMLEN 0/14");
    assert_eq!(TX_LAST, 1 << 15, "MACB_TX_LAST_OFFSET");
    assert_eq!(TX_NOCRC, 1 << 16, "MACB_TX_NOCRC_OFFSET");
    assert_eq!(TX_BUF_EXHAUSTED, 1 << 27, "MACB_TX_BUF_EXHAUSTED_OFFSET");
    assert_eq!(TX_UNDERRUN, 1 << 28, "MACB_TX_UNDERRUN_OFFSET");
    assert_eq!(TX_ERROR, 1 << 29, "MACB_TX_ERROR_OFFSET");
    assert_eq!(TX_WRAP, 1 << 30, "MACB_TX_WRAP_OFFSET");
    assert_eq!(TX_USED, 1 << 31, "MACB_TX_USED_OFFSET");

    // La longitud de recepcion y la de transmision **no miden lo mismo**: 12
    // bits contra 14. Enmascarar con la del otro lado trunca la trama.
    assert_ne!(RX_FRMLEN_MASK, TX_FRMLEN_MASK);
    // La mascara de la direccion de recepcion es justo lo que no son los dos
    // bits de estado: si se solaparan, dar un hueco al MAC le daria tambien
    // una direccion a medias.
    assert_eq!(RX_ADDR_MASK & (RX_USED | RX_WRAP), 0);
    assert_eq!(RX_ADDR_MASK | RX_USED | RX_WRAP, u32::MAX);
    // Y los errores de transmision no pisan ni la longitud ni las dos marcas.
    assert_eq!(
        TX_ERRORS & (TX_FRMLEN_MASK | TX_LAST | TX_WRAP | TX_USED),
        0
    );
}

/// Los campos de `GEM_DMACFG`.
#[test]
fn the_dma_config_fields_are_the_ones_linux_publishes() {
    assert_eq!((DMACFG_FBLDO_SHIFT, DMACFG_FBLDO_MASK), (0, 0x1F));
    assert_eq!(DMACFG_ENDIA_DESC, 1 << 6, "GEM_ENDIA_DESC_OFFSET 6");
    assert_eq!(DMACFG_ENDIA_PKT, 1 << 7, "GEM_ENDIA_PKT_OFFSET 7");
    assert_eq!((DMACFG_RXBMS_SHIFT, DMACFG_RXBMS_MASK), (8, 0x3));
    assert_eq!(DMACFG_TXPBMS, 1 << 10, "GEM_TXPBMS_OFFSET 10");
    assert_eq!(DMACFG_TXCOEN, 1 << 11, "GEM_TXCOEN_OFFSET 11");
    assert_eq!((DMACFG_RXBS_SHIFT, DMACFG_RXBS_MASK), (16, 0xFF));
    assert_eq!(DMACFG_ADDR64, 1 << 30, "GEM_ADDR64_OFFSET 30");
    // Y el tamaño del buffer tiene que caber en su campo contado en multiplos
    // de 64: con un buffer de 16 KiB no cabria y el MAC leeria otro tamaño.
    let en_multiplos = (RX_BUF_LENGTH / RX_BUFFER_MULTIPLE) as u32;
    assert!(en_multiplos <= DMACFG_RXBS_MASK);
    assert_eq!(en_multiplos, 32);
}

/// El descriptor mide el doble en modo de 64 bits, porque detras lleva la
/// mitad alta de la direccion.
#[test]
fn a_descriptor_is_twice_as_long_when_the_addresses_are_sixty_four_bit() {
    assert_eq!(DESC_SIZE_32, 8);
    assert_eq!(DESC_SIZE_64, 16);
    // Y la palabra de la mitad alta cae justo detras de las dos primeras.
    assert_eq!(DESC_ADDRH * 4, DESC_SIZE_32);
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

#[test]
fn the_dma_is_configured_with_the_buffer_size_the_ring_really_has() {
    let hw = Hw::new();
    let _rings = anillos(&hw);
    let dmacfg = hw.peek(GEM_DMACFG);

    assert_eq!(
        (dmacfg >> DMACFG_RXBS_SHIFT) & DMACFG_RXBS_MASK,
        (RX_BUF_LENGTH / RX_BUFFER_MULTIPLE) as u32,
        "el MAC cree que el buffer mide otra cosa"
    );
    assert_eq!(
        (dmacfg >> DMACFG_FBLDO_SHIFT) & DMACFG_FBLDO_MASK,
        DMA_BURST_LENGTH
    );
    assert_ne!(dmacfg & DMACFG_TXPBMS, 0);
    assert_eq!(
        (dmacfg >> DMACFG_RXBMS_SHIFT) & DMACFG_RXBMS_MASK,
        DMACFG_RXBMS_MASK
    );
    // Los dos bits de orden de bytes son para un anfitrion big endian: con
    // uno puesto el MAC lee la direccion de un descriptor del reves.
    assert_eq!(dmacfg & (DMACFG_ENDIA_DESC | DMACFG_ENDIA_PKT), 0);
    // El falso dice 64 bits de direccion, asi que `ADDR64` va puesto: sin el,
    // el MAC leeria los descriptores anchos como si midieran ocho bytes.
    assert_ne!(dmacfg & DMACFG_ADDR64, 0);
}

/// `GEM_DMACFG` se modifica, no se reescribe: los bits que este driver no
/// nombra son del silicio.
#[test]
fn configuring_the_dma_leaves_the_bits_nobody_here_names_alone() {
    let hw = Hw::new();
    // Un bit de los de las marcas de tiempo del 1588 (`GEM_RXEXT`, bit 28).
    let ajeno = 1 << 28;
    hw.poke(GEM_DMACFG, ajeno);
    let _rings = anillos(&hw);
    assert_ne!(
        hw.peek(GEM_DMACFG) & ajeno,
        0,
        "se llevo por delante un bit que no es suyo"
    );
}

#[test]
fn the_mac_is_told_where_both_rings_are() {
    let hw = Hw::new();
    let rings = anillos(&hw);
    assert_eq!(hw.peek(RBQP), rings.rx.paddr() as u32);
    assert_eq!(hw.peek(TBQP), rings.tx.paddr() as u32);
    // Y con direcciones de 64 bits, tambien las mitades altas.
    assert!(rings.addr64);
    assert_eq!(hw.peek(RBQPH), (rings.rx.paddr() >> 32) as u32);
    assert_eq!(hw.peek(TBQPH), (rings.tx.paddr() >> 32) as u32);
}

/// Con un GEM que direcciona a 64 bits, el descriptor mide el doble, se pide
/// `ADDR64`, y las mitades altas de las bases se escriben **antes** que las
/// bajas, por lo mismo que en un descriptor.
#[test]
fn a_sixty_four_bit_gem_gets_the_wide_descriptors_and_the_high_halves_first() {
    let hw = Hw::new();
    let marca = hw.write_mark();
    let rings = anillos(&hw);

    assert!(rings.addr64);
    assert_eq!(rings.rx.stride, DESC_SIZE_64);
    assert_ne!(hw.peek(GEM_DMACFG) & DMACFG_ADDR64, 0);

    let offsets = hw.writes_since(marca);
    let alta = offsets.iter().position(|off| *off == RBQPH);
    let baja = offsets.iter().position(|off| *off == RBQP);
    assert!(
        alta.is_some() && baja.is_some(),
        "falta una de las dos bases"
    );
    assert!(
        alta.unwrap() < baja.unwrap(),
        "escribio la mitad baja de la base antes que la alta"
    );
}

/// Todos los huecos de recepcion arrancan en manos del MAC y apuntando a su
/// propio buffer, y **solo el ultimo** lleva la marca de la vuelta: ponersela
/// a otro es un anillo que da la vuelta antes de tiempo y se queda con los
/// huecos de delante sin usar.
#[test]
fn every_receive_slot_starts_in_the_macs_hands_and_only_the_last_one_wraps() {
    let hw = Hw::new();
    let rings = anillos(&hw);
    for slot in 0..RX_RING_SIZE {
        let addr = rings.rx.read(slot, DESC_ADDR);
        assert_eq!(addr & RX_USED, 0, "el hueco {slot} no es del MAC");
        let (_, paddr) = rings.rx_buf_at(slot);
        assert_eq!(
            addr & RX_ADDR_MASK,
            paddr as u32 & RX_ADDR_MASK,
            "el hueco {slot} no apunta a su buffer"
        );
        assert_eq!(rings.rx.read(slot, DESC_CTRL), 0);
        let wrap = addr & RX_WRAP != 0;
        assert_eq!(
            wrap,
            slot == RX_RING_SIZE - 1,
            "la vuelta en el hueco {slot}"
        );
    }
}

/// Y la cola de transmision arranca entera en manos del driver, con la vuelta
/// en el ultimo: un descriptor sin `TX_USED` al arrancar es una trama de
/// basura que el MAC transmite en cuanto se le toca el timbre.
#[test]
fn every_transmit_slot_starts_in_the_drivers_hands() {
    let hw = Hw::new();
    let rings = anillos(&hw);
    for slot in 0..TX_RING_SIZE {
        let ctrl = rings.tx.read(slot, DESC_CTRL);
        assert_ne!(ctrl & TX_USED, 0, "el hueco {slot} es del MAC");
        assert_eq!(rings.tx.read(slot, DESC_ADDR), 0);
        let wrap = ctrl & TX_WRAP != 0;
        assert_eq!(
            wrap,
            slot == TX_RING_SIZE - 1,
            "la vuelta en el hueco {slot}"
        );
    }
}

/// Los buffers de recepcion tienen que estar alineados a cuatro: los dos bits
/// de abajo de esa palabra **no son direccion**, son el estado.
#[test]
fn every_receive_buffer_is_aligned_enough_that_its_address_survives() {
    let hw = Hw::new();
    let rings = anillos(&hw);
    for slot in 0..RX_RING_SIZE {
        let (_, paddr) = rings.rx_buf_at(slot);
        assert_eq!(paddr % 4, 0, "el buffer del hueco {slot} pierde dos bits");
        assert_eq!(
            paddr as u32 & RX_ADDR_MASK,
            paddr as u32,
            "la direccion del hueco {slot} no cabe en su campo"
        );
    }
}

/// **El orden del relleno**: el control a cero antes de la direccion, porque
/// escribir la direccion es lo que baja el `RX_USED` y abre el hueco.
#[test]
fn giving_a_slot_back_clears_the_control_word_before_it_opens_the_slot() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    deliver_frame(&rings, 0, &[0xAA; 64]);

    let marca = hw.desc_mark();
    let mut buf = [0u8; 128];
    rings.recv(&mut buf).unwrap();

    let escrituras: Vec<_> = hw
        .desc_writes_since(marca)
        .into_iter()
        .filter(|(kind, slot, _, _)| *kind == RingKind::Rx && *slot == 0)
        .map(|(_, _, word, _)| word)
        .collect();
    assert_eq!(
        escrituras,
        vec![DESC_CTRL, DESC_ADDRH, DESC_ADDR],
        "el hueco se abrio antes de dejarlo listo"
    );
}

/// Y con direcciones de 64 bits, la mitad alta tambien va antes que la baja.
#[test]
fn the_high_half_of_a_slots_address_is_written_before_the_half_that_opens_it() {
    let hw = Hw::new();
    let marca = hw.desc_mark();
    let _rings = anillos(&hw);

    let primeras: Vec<_> = hw
        .desc_writes_since(marca)
        .into_iter()
        .filter(|(kind, slot, _, _)| *kind == RingKind::Rx && *slot == 0)
        .map(|(_, _, word, _)| word)
        .collect();
    assert_eq!(primeras, vec![DESC_CTRL, DESC_ADDRH, DESC_ADDR]);
}

// ---------------------------------------------------------------------------
// Recepcion
// ---------------------------------------------------------------------------

/// Lo unico que dice que hay trama es el `RX_USED` del descriptor: no hay
/// ningun indice al que preguntar.
#[test]
fn a_frame_arrives_and_comes_out_whole() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    assert!(!rings.can_recv());

    let trama: Vec<u8> = (0..100u8).collect();
    deliver_frame(&rings, 0, &trama);
    assert!(rings.can_recv());

    let mut buf = [0u8; 256];
    let n = rings.recv(&mut buf).unwrap();
    assert_eq!(n, trama.len(), "la longitud se llevo los dos de alineacion");
    assert_eq!(&buf[..n], &trama[..]);
    assert!(!rings.can_recv());
}

/// Los dos bytes de alineacion que el MAC mete delante **estan dentro** de la
/// longitud que declara el descriptor: no restarlos entrega dos bytes de
/// basura al principio de cada trama y una longitud de mas.
#[test]
fn the_two_alignment_bytes_the_mac_prepends_are_not_part_of_the_frame() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let trama = [0x11, 0x22, 0x33, 0x44];
    deliver_frame(&rings, 0, &trama);

    let declarada = rings.rx.read(0, DESC_CTRL) & RX_FRMLEN_MASK;
    assert_eq!(declarada, trama.len() as u32 + RX_IP_ALIGN);

    let mut buf = [0u8; 16];
    let n = rings.recv(&mut buf).unwrap();
    assert_eq!(n, trama.len());
    assert_eq!(&buf[..n], &trama);
}

/// Una trama que se sale del buffer del llamante **no se descarta**: sigue ahi
/// para el que venga con sitio.
#[test]
fn a_frame_that_does_not_fit_is_left_where_it_is() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    deliver_frame(&rings, 0, &[0x5A; 100]);

    let mut pequeno = [0u8; 50];
    assert_eq!(
        rings.recv(&mut pequeno).err(),
        Some(DeviceError::BufferTooSmall)
    );
    assert!(rings.can_recv(), "se perdio la trama");

    let mut grande = [0u8; 200];
    assert_eq!(rings.recv(&mut grande).unwrap(), 100);
    assert_eq!(&grande[..100], &[0x5A; 100][..]);
}

/// Una trama partida entre varios descriptores se descarta y se cuenta: este
/// driver no las junta, y el buffer da para cualquier trama que el MAC acepte.
#[test]
fn a_frame_split_across_descriptors_is_dropped_and_counted() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    // Principio sin fin: el MAC dice que la trama sigue en el siguiente.
    deliver_frame_with_ctrl(&rings, 0, &[0x5A; 64], RX_SOF);

    let mut buf = [0u8; 256];
    assert_eq!(rings.recv(&mut buf).err(), Some(DeviceError::IoError));
    assert_eq!(rings.rx_errors(), 1);
    // Y el hueco vuelve al MAC en vez de quedarse atascado.
    assert_eq!(rings.rx.read(0, DESC_ADDR) & RX_USED, 0);
    assert!(!rings.can_recv());
}

/// Y el orden de las dos comprobaciones importa: una trama partida que
/// **tampoco cabe** en el buffer del llamante se descarta, no se reporta como
/// "no cabe". Al contrario se quedaria atascada en el anillo para siempre,
/// porque ningun buffer la va a aceptar nunca.
#[test]
fn a_split_frame_that_does_not_fit_either_is_still_dropped_not_held() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    deliver_frame_with_ctrl(&rings, 0, &[0x5A; 200], RX_SOF);

    let mut pequeno = [0u8; 8];
    assert_eq!(rings.recv(&mut pequeno).err(), Some(DeviceError::IoError));
    assert_eq!(rings.rx_errors(), 1);
    assert!(!rings.can_recv(), "se quedo atascada en el anillo");
}

/// Una longitud que no puede ser tampoco se entrega: ni una que no da ni para
/// los dos bytes de alineacion, ni una que se sale del buffer.
#[test]
fn a_length_the_buffer_cannot_hold_is_refused_rather_than_copied() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let mut buf = [0u8; 4096];

    // Mas larga que el buffer: el MAC no puede haber escrito tanto.
    rings.rx.write(0, DESC_CTRL, RX_SOF | RX_EOF | 0x0FFF);
    let addr = rings.rx.read(0, DESC_ADDR);
    rings.rx.write(0, DESC_ADDR, addr | RX_USED);
    assert_eq!(rings.recv(&mut buf).err(), Some(DeviceError::IoError));

    // Y una que no llega ni a los dos bytes de alineacion, que restados
    // darian la vuelta.
    rings.rx.write(1, DESC_CTRL, RX_SOF | RX_EOF | 1);
    let addr = rings.rx.read(1, DESC_ADDR);
    rings.rx.write(1, DESC_ADDR, addr | RX_USED);
    assert_eq!(rings.recv(&mut buf).err(), Some(DeviceError::IoError));

    assert_eq!(rings.rx_errors(), 2);
}

/// El anillo da la vuelta y vuelve al primer hueco, con los descriptores
/// devueltos al MAC por el camino.
#[test]
fn the_receive_ring_goes_round_and_comes_back_to_the_first_slot() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let mut buf = [0u8; 256];

    for vuelta in 0..3 {
        for slot in 0..RX_RING_SIZE {
            let marca = [vuelta as u8, slot as u8, 0xEE, 0xFF];
            deliver_frame(&rings, slot, &marca);
            let n = rings.recv(&mut buf).unwrap();
            assert_eq!(&buf[..n], &marca, "vuelta {vuelta}, hueco {slot}");
        }
        assert_eq!(rings.rx_tail, 0, "no volvio al primer hueco");
    }
    assert_eq!(rings.rx_errors(), 0);
}

// ---------------------------------------------------------------------------
// Transmision
// ---------------------------------------------------------------------------

#[test]
fn a_frame_goes_out_through_the_descriptor_and_the_bell_is_rung() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let trama: Vec<u8> = (0..60u8).collect();

    let marca = hw.write_mark();
    assert_eq!(rings.send(&trama).unwrap(), trama.len());

    let ctrl = rings.tx.read(0, DESC_CTRL);
    assert_eq!(ctrl & TX_FRMLEN_MASK, trama.len() as u32);
    assert_ne!(ctrl & TX_LAST, 0, "no es el ultimo buffer de la trama");
    assert_eq!(ctrl & TX_USED, 0, "no se solto el descriptor");
    // La FCS la pone el MAC: la trama que llega de arriba no la lleva.
    assert_eq!(ctrl & TX_NOCRC, 0);
    assert_eq!(sent_frame(&rings, 0), trama);

    // Y el timbre, que es `TSTART` de `NCR`.
    assert!(
        hw.writes()[marca..]
            .iter()
            .any(|(off, val)| *off == NCR && val & NCR_TSTART != 0),
        "nadie toco el timbre"
    );
}

/// El timbre es read-modify-write: `NCR` lleva tambien las dos habilitaciones
/// y el puerto de mantenimiento, y un `write` pelado apagaria la tarjeta justo
/// al poner una trama en la cola.
#[test]
fn ringing_the_bell_does_not_turn_the_card_off() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    rings.mac().set_mdio_enabled(true);
    rings.mac().set_rx_tx_enabled(true);

    rings.send(&[0xAB; 64]).unwrap();

    let ncr = hw.peek(NCR);
    assert_eq!(ncr & (NCR_RE | NCR_TE), NCR_RE | NCR_TE, "apago la tarjeta");
    assert_ne!(ncr & NCR_MPE, 0, "apago el puerto de mantenimiento");
}

/// **El orden de la transmision**: la direccion antes del control, porque
/// escribir el control es lo que baja el `TX_USED` y suelta el descriptor; y
/// el centinela del hueco siguiente antes que los dos, porque el MAC tira del
/// anillo hasta encontrar uno marcado.
#[test]
fn the_sentinel_is_planted_before_the_descriptor_is_released() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let marca = hw.desc_mark();
    rings.send(&[0xAB; 64]).unwrap();

    let escrituras: Vec<_> = hw
        .desc_writes_since(marca)
        .into_iter()
        .filter(|(kind, _, _, _)| *kind == RingKind::Tx)
        .map(|(_, slot, word, _)| (slot, word))
        .collect();
    assert_eq!(
        escrituras,
        vec![
            (1, DESC_CTRL),
            (0, DESC_ADDRH),
            (0, DESC_ADDR),
            (0, DESC_CTRL)
        ],
        "el orden de la transmision no es el que el MAC necesita"
    );
    // Y el centinela lleva `TX_USED`, que es donde el MAC para.
    assert_ne!(rings.tx.read(1, DESC_CTRL) & TX_USED, 0);
}

#[test]
fn a_frame_nobody_can_transmit_is_refused_without_touching_the_ring() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let marca = hw.desc_mark();

    assert_eq!(rings.send(&[]).err(), Some(DeviceError::InvalidParam));
    assert_eq!(
        rings.send(&[0; MAX_TX_FRAME + 1]).err(),
        Some(DeviceError::InvalidParam)
    );
    assert_eq!(hw.desc_mark(), marca, "toco el anillo igualmente");
    // Y la mas larga que si vale, entra.
    assert!(rings.send(&[0; MAX_TX_FRAME]).is_ok());
}

/// El MAC devuelve los descriptores poniendo `TX_USED`, y el driver los
/// recoge. Uno que el MAC todavia no ha soltado no se recoge.
#[test]
fn only_the_descriptors_the_mac_has_given_back_are_reclaimed() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    rings.send(&[1; 64]).unwrap();
    rings.send(&[2; 64]).unwrap();
    assert_eq!(rings.tx_in_flight(), 2);

    assert_eq!(rings.reclaim_tx(), 0, "recogio lo que el MAC no ha soltado");
    complete_tx(&rings, 0, 0);
    assert_eq!(rings.reclaim_tx(), 1);
    assert_eq!(rings.tx_in_flight(), 1);
    complete_tx(&rings, 1, 0);
    assert_eq!(rings.reclaim_tx(), 1);
    assert_eq!(rings.tx_in_flight(), 0);
    assert_eq!(rings.tx_errors(), 0);
}

/// Una transmision que el MAC marca como fallida se cuenta: el descriptor se
/// recoge igual, porque el MAC ya ha terminado con el.
#[test]
fn a_transmission_the_mac_says_failed_is_counted_and_still_reclaimed() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    rings.send(&[1; 64]).unwrap();
    complete_tx(&rings, 0, TX_UNDERRUN);
    assert_eq!(rings.reclaim_tx(), 1);
    assert_eq!(rings.tx_errors(), 1);
    assert_eq!(rings.tx_in_flight(), 0);
}

/// El anillo lleno se dice, no se desborda, y **siempre queda un hueco
/// libre**: el del centinela donde el MAC para.
#[test]
fn a_full_ring_is_reported_instead_of_overrun() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);
    let mut puestas = 0;
    while rings.can_send() {
        rings.send(&[0xCD; 64]).unwrap();
        puestas += 1;
        assert!(puestas <= TX_RING_SIZE, "no se lleno nunca");
    }
    assert_eq!(puestas, TX_RING_SIZE - 1, "se comio el hueco del centinela");
    assert_eq!(rings.send(&[0xCD; 64]).err(), Some(DeviceError::NotReady));

    // Y en cuanto el MAC suelta uno, cabe otra.
    complete_tx(&rings, 0, 0);
    assert!(rings.send(&[0xCD; 64]).is_ok());
}

/// El anillo de transmision da la vuelta, y la marca de la vuelta se mantiene
/// en el ultimo hueco: perderla al soltar un descriptor es un MAC que se va a
/// leer descriptores de detras del anillo.
#[test]
fn the_transmit_ring_goes_round_without_losing_the_wrap_mark() {
    let hw = Hw::new();
    let mut rings = anillos(&hw);

    for vuelta in 0..3u8 {
        for slot in 0..TX_RING_SIZE {
            let trama = [vuelta, slot as u8, 0xEE, 0xFF];
            rings.send(&trama).unwrap();
            assert_eq!(sent_frame(&rings, slot), trama, "vuelta {vuelta}");
            complete_tx(&rings, slot, 0);
            assert_eq!(rings.reclaim_tx(), 1);
        }
        assert_eq!(rings.tx_head, 0, "no volvio al primer hueco");
    }

    assert_ne!(
        rings.tx.read(TX_RING_SIZE - 1, DESC_CTRL) & TX_WRAP,
        0,
        "perdio la marca de la vuelta"
    );
}

/// Con `ADDR64` apagado la direccion de un descriptor son 32 bits y no hay
/// ninguna otra: una region por encima de los 4 GiB **se truncaria sin
/// avisar**, y el MAC escribiria las tramas donde no debe.
#[test]
fn a_region_a_thirty_two_bit_dma_cannot_reach_is_refused_rather_than_truncated() {
    // Justo debajo del techo, y justo pegada a el, valen.
    assert!(reachable_by_32bit_dma(0, 1 << 32));
    assert!(reachable_by_32bit_dma((1 << 32) - 4096, 4096));
    // Un byte mas, no.
    assert!(!reachable_by_32bit_dma((1 << 32) - 4096, 4097));
    assert!(!reachable_by_32bit_dma(1 << 32, 1));
    // Y una suma que da la vuelta tampoco pasa por buena.
    assert!(!reachable_by_32bit_dma(usize::MAX, 1));

    // Y un GEM que diga 32 bits aqui **no arranca**, porque una region de DMA
    // del anfitrion esta muy por encima de los 4 GiB. Eso es justo lo que se
    // quiere: el aviso lo da el arranque, no una trama escrita en la direccion
    // equivocada. En una placa con memoria baja disponible arrancaria igual.
    let hw = Hw::new();
    hw.poke(GEM_DCFG6, 0);
    let mac = hw.mac();
    mac.reset_hw();
    mac.init_config(&MacbConfig::default());
    assert_eq!(MacbRings::new(mac).err(), Some(DeviceError::DmaError));
}
