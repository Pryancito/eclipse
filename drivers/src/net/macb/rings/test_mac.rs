//! El lado del MAC, para los tests: lo que el hardware haria con los
//! descriptores y el anfitrion no.
//!
//! Esta en su modulo, y no dentro de [`rings_tests`](super::rings_tests),
//! porque los tests del pegamento ([`glue`](crate::net::macb::glue)) necesitan
//! lo mismo: meter una trama en un hueco de recepcion y dar una transmision
//! por terminada. Copiarlo seria tener dos modelos del MAC que se pueden
//! separar sin que nadie lo note.

use super::super::macb_fake::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// La direccion que un descriptor declara, juntando las dos mitades.
pub(super) fn descriptor_addr(ring: &Ring, slot: usize, addr64: bool) -> usize {
    let lo = ring.read(slot, DESC_ADDR) & !(RX_USED | RX_WRAP);
    let hi = if addr64 {
        ring.read(slot, DESC_ADDRH) as usize
    } else {
        0
    };
    (hi << 32) | lo as usize
}

/// Unos anillos recien arrancados sobre el GEM falso de este hilo.
pub(in crate::net::macb) fn anillos(hw: &Hw) -> MacbRings {
    let mac = hw.mac();
    mac.reset_hw();
    mac.init_config(&MacbConfig::default());
    MacbRings::new(mac).expect("los anillos arrancan")
}

/// Mete una trama en un hueco de recepcion como lo haria el MAC: la copia en
/// el buffer del descriptor dejando delante los dos bytes de alineacion,
/// declara la longitud **con** esos dos bytes, y sube el `RX_USED`.
pub(in crate::net::macb) fn deliver_frame(rings: &MacbRings, slot: usize, frame: &[u8]) {
    deliver_frame_with_ctrl(rings, slot, frame, RX_SOF | RX_EOF);
}

/// Lo mismo, pero con los bits de control que el MAC quiera poner.
pub(in crate::net::macb) fn deliver_frame_with_ctrl(
    rings: &MacbRings,
    slot: usize,
    frame: &[u8],
    ctrl: u32,
) {
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
pub(in crate::net::macb) fn complete_tx(rings: &MacbRings, slot: usize, estado: u32) {
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
pub(in crate::net::macb) fn sent_frame(rings: &MacbRings, slot: usize) -> Vec<u8> {
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
