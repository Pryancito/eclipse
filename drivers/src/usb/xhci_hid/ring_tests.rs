use super::*;

/// A transfer ring of `n` TRBs: `n - 1` usable slots and a Link TRB in the
/// last one. `XferRing::new(8)` is the smallest size worth testing and
/// `XferRing::new(64)` is what an interrupt endpoint actually gets.
fn a_ring(n: usize) -> XferRing {
    XferRing::new(n).expect("un anillo de transferencia")
}

/// Fill `count` slots with TRBs whose buffer pointers say which slot they
/// are, advancing the dequeue head so the ring never refuses one.
fn fill(r: &mut XferRing, count: usize) {
    for i in 0..count {
        if r.is_full() {
            r.advance_dequeue(1);
        }
        r.push(trb_normal(buf_of(i), 8, true)).expect("un push");
    }
}

fn buf_of(i: usize) -> u64 {
    0xdead_0000 + (i as u64) * 0x1000
}

/// The bug. `prev_trb_phys` exists so a Transfer Event that points at the
/// TRB *after* the completed one (VirtualBox does this) can still be traced
/// back to the buffer that was filled. At the first slot it answered the
/// Link TRB, whose pointer field is the ring's own base address.
#[test]
fn the_trb_before_the_first_one_is_the_last_transfer_trb_and_not_the_link() {
    let mut r = a_ring(8);
    let base = r.buf.phys as u64;
    let last_data = r.cap - 1;
    let cap = r.cap;
    fill(&mut r, cap);
    let prev = r.prev_trb_phys(base);
    assert_eq!(
        (prev - base) / 16,
        last_data as u64,
        "el TRB anterior al primero es el indice {}, no el ultimo de datos ({})",
        (prev - base) / 16,
        last_data
    );
    assert_eq!(
        r.trb_buffer_at(prev),
        Some(buf_of(last_data)),
        "el TRB anterior al primero no lleva el buffer del ultimo de datos"
    );
}

#[test]
fn the_link_trb_is_never_answered_as_if_it_carried_a_report_buffer() {
    let mut r = a_ring(8);
    let base = r.buf.phys as u64;
    let cap = r.cap;
    fill(&mut r, cap);
    for slot in 0..=r.cap {
        let phys = base + (slot as u64) * 16;
        let prev = r.prev_trb_phys(phys);
        assert!(
            (prev - base) / 16 < r.cap as u64,
            "prev_trb_phys del indice {} contesta el indice {}, que es el TRB LINK",
            slot,
            (prev - base) / 16
        );
        assert_ne!(
            r.trb_buffer_at(prev),
            Some(base),
            "prev_trb_phys del indice {} contesta la base del anillo como buffer",
            slot
        );
    }
}

/// xHCI 1.2 section 4.9.2.2: the Link TRB's Cycle bit belongs to the pass
/// that just ended, and its Toggle Cycle bit has to stay set or the
/// controller never flips its own cycle state.
#[test]
fn a_ring_that_laps_publishes_the_link_cycle_of_the_pass_that_just_ended() {
    let mut r = a_ring(8);
    let link_ctrl = r.cap * 16 + 12;
    assert!(
        r.cycle,
        "el anillo no empieza con el ciclo del productor a 1"
    );
    let cap = r.cap;
    fill(&mut r, cap);
    assert_eq!(r.enq, 0, "el anillo no ha dado la vuelta");
    assert_eq!(
        r.buf.read_u32(link_ctrl) & 1,
        1,
        "el TRB LINK no lleva el ciclo de la vuelta que acaba de terminar"
    );
    assert_eq!(
        r.buf.read_u32(link_ctrl) & (1 << 1),
        1 << 1,
        "el TRB LINK ha perdido su Toggle Cycle"
    );
    assert!(
        !r.cycle,
        "el ciclo del productor no ha cambiado en la vuelta"
    );
    let cap = r.cap;
    fill(&mut r, cap);
    assert_eq!(
        r.buf.read_u32(link_ctrl) & 1,
        0,
        "el TRB LINK no lleva el ciclo de la segunda vuelta"
    );
    assert!(r.cycle, "el ciclo del productor no ha vuelto a cambiar");
}

#[test]
fn a_full_ring_refuses_a_trb_instead_of_overwriting_one_not_yet_consumed() {
    let mut r = a_ring(8);
    let mut pushed = 0usize;
    while r.push(trb_normal(buf_of(pushed), 8, true)).is_ok() {
        pushed += 1;
        assert!(pushed <= r.cap, "el anillo acepta mas TRB que huecos");
    }
    assert_eq!(
        pushed,
        r.cap - 1,
        "un anillo de {} huecos acepta {} TRB sin que nadie consuma",
        r.cap,
        pushed
    );
    let enq = r.enq;
    assert!(r.push(trb_normal(0x1000, 8, true)).is_err());
    assert_eq!(
        r.enq, enq,
        "un push rechazado ha movido la cabeza de encolado"
    );
}

/// The DCS field of Set TR Dequeue Pointer has to be the cycle the
/// controller will find on the TRB at the dequeue head, which after a lap
/// is the previous pass's, not the producer's current one.
#[test]
fn the_dequeue_head_carries_the_cycle_stamped_on_its_own_trb() {
    let mut r = a_ring(8);
    let base = r.buf.phys as u64;
    let cap = r.cap;
    fill(&mut r, cap);
    // The dequeue head was dragged along by `fill`; put it back on a TRB of
    // the first pass and check the cycle comes from the TRB, not from `r`.
    r.xfer_deq = 1;
    assert!(!r.cycle, "el productor deberia ir por la segunda vuelta");
    assert!(
        r.deq_cycle(),
        "el ciclo de la cabeza de desencolado sale del productor y no de su TRB"
    );
    assert_eq!(r.deq_phys(), base + 16, "deq_phys no apunta a su hueco");
    // Empty ring: there is no TRB to read, so the producer's cycle is the
    // only answer there is.
    r.xfer_deq = r.enq;
    assert_eq!(
        r.deq_cycle(),
        r.cycle,
        "un anillo vacio no contesta el ciclo del productor"
    );
}

#[test]
fn an_address_off_the_ring_or_off_a_trb_boundary_is_not_one_of_its_trbs() {
    let mut r = a_ring(8);
    let base = r.buf.phys as u64;
    fill(&mut r, 1);
    assert_eq!(r.trb_buffer_at(base), Some(buf_of(0)));
    assert_eq!(
        r.trb_buffer_at(base - 16),
        None,
        "una direccion antes del anillo"
    );
    assert_eq!(
        r.trb_buffer_at(base + ((r.cap as u64) + 1) * 16),
        None,
        "una direccion despues del anillo"
    );
    assert_eq!(
        r.trb_buffer_at(base + 8),
        None,
        "media TRB dentro del anillo"
    );
}

/// The event ring has no Link TRB: the consumer flips its own cycle state
/// when the dequeue pointer wraps, and a TRB whose Cycle bit no longer
/// matches is one the controller has not written yet.
#[test]
fn an_event_ring_flips_its_consumer_cycle_once_a_lap() {
    let n = 16;
    let mut ev = EventRing::new(n).expect("un anillo de eventos");
    // The controller stamps its producer cycle on each event it posts.
    for i in 0..n {
        ev.seg.write_u64(i * 16, 0x4000 + i as u64);
        ev.seg.write_u32(i * 16 + 12, TRB_EVT_TRANSFER | 1);
    }
    for i in 0..n {
        let t = ev.pop().expect("un evento");
        assert_eq!(t.p, 0x4000 + i as u64, "los eventos no salen en orden");
    }
    assert_eq!(ev.deq, 0, "la cabeza de desencolado no ha dado la vuelta");
    assert!(
        !ev.cycle,
        "el ciclo del consumidor no ha cambiado en la vuelta"
    );
    assert!(
        ev.pop().is_none(),
        "el consumidor vuelve a leer los eventos de la vuelta anterior"
    );
    assert_eq!(
        ev.erdp_phys(),
        ev.seg.phys as u64,
        "ERDP no apunta a la cabeza"
    );
}
