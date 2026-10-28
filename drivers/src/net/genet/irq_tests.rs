//! Los dos bloques `INTRL2`.

use alloc::vec::Vec;

use super::genet_fake::Hw;
use super::*;

/// El bloque 0 y el bloque 1 estan donde los pone `bcmgenet.h`, y los seis
/// registros de cada uno en el orden que declara: `CPU_STAT`, `CPU_SET`,
/// `CPU_CLEAR`, `CPU_MASK_STATUS`, `CPU_MASK_SET`, `CPU_MASK_CLEAR`.
#[test]
fn the_two_interrupt_blocks_are_where_the_header_puts_them() {
    assert_eq!(GENET_INTRL2_0_OFF, 0x0200);
    assert_eq!(GENET_INTRL2_1_OFF, 0x0240);
    assert_eq!(INTRL2_CPU_STAT, 0x00);
    assert_eq!(INTRL2_CPU_SET, 0x04);
    assert_eq!(INTRL2_CPU_CLEAR, 0x08);
    assert_eq!(INTRL2_CPU_MASK_STATUS, 0x0C);
    assert_eq!(INTRL2_CPU_MASK_SET, 0x10);
    assert_eq!(INTRL2_CPU_MASK_CLEAR, 0x14);
    // Que los seis registros de cada bloque caben antes del bloque siguiente
    // lo comprueba `irq.rs` en tiempo de compilacion y en las cuatro
    // arquitecturas, no aqui.
}

/// Los bits del `INTRL2_0`, uno a uno contra el nombre con el que los publica
/// Linux.
#[test]
fn the_interrupt_bits_are_the_published_ones() {
    assert_eq!(UMAC_IRQ_SCB, 1 << 0);
    assert_eq!(UMAC_IRQ_EPHY, 1 << 1);
    assert_eq!(UMAC_IRQ_PHY_DET_R, 1 << 2);
    assert_eq!(UMAC_IRQ_PHY_DET_F, 1 << 3);
    assert_eq!(UMAC_IRQ_LINK_UP, 1 << 4);
    assert_eq!(UMAC_IRQ_LINK_DOWN, 1 << 5);
    assert_eq!(UMAC_IRQ_LINK_EVENT, (1 << 4) | (1 << 5));
    assert_eq!(UMAC_IRQ_UMAC, 1 << 6);
    assert_eq!(UMAC_IRQ_UMAC_TSV, 1 << 7);
    assert_eq!(UMAC_IRQ_TBUF_UNDERRUN, 1 << 8);
    assert_eq!(UMAC_IRQ_RBUF_OVERFLOW, 1 << 9);
    assert_eq!(UMAC_IRQ_HFB_SM, 1 << 10);
    assert_eq!(UMAC_IRQ_HFB_MM, 1 << 11);
    assert_eq!(UMAC_IRQ_MPD_R, 1 << 12);
    assert_eq!(UMAC_IRQ_RXDMA_MBDONE, 1 << 13);
    assert_eq!(UMAC_IRQ_RXDMA_PDONE, 1 << 14);
    assert_eq!(UMAC_IRQ_RXDMA_BDONE, 1 << 15);
    assert_eq!(UMAC_IRQ_TXDMA_MBDONE, 1 << 16);
    assert_eq!(UMAC_IRQ_TXDMA_PDONE, 1 << 17);
    assert_eq!(UMAC_IRQ_TXDMA_BDONE, 1 << 18);
    assert_eq!(UMAC_IRQ_MDIO_DONE, 1 << 23);
    assert_eq!(UMAC_IRQ_MDIO_ERROR, 1 << 24);
    // `DONE` es el del modo por lotes, no el por paquete ni el por byte: el
    // driver arma los anillos en modo lote (`MBUF_DONE_THRESH`), asi que
    // esperar el `PDONE` seria esperar una interrupcion que no llega.
    assert_eq!(UMAC_IRQ_RXDMA_DONE, UMAC_IRQ_RXDMA_MBDONE);
    assert_eq!(UMAC_IRQ_TXDMA_DONE, UMAC_IRQ_TXDMA_MBDONE);
}

/// Lo que el driver arma: las dos de DMA y las dos del enlace, y nada mas.
#[test]
fn the_armed_set_is_the_two_dmas_and_the_link() {
    assert_eq!(
        GENET_IRQ_WANTED,
        UMAC_IRQ_RXDMA_MBDONE | UMAC_IRQ_TXDMA_MBDONE | UMAC_IRQ_LINK_UP | UMAC_IRQ_LINK_DOWN
    );
    // Y en particular NO los de error del MAC ni el del MDIO, que se atienden
    // de otra manera.
    assert_eq!(
        GENET_IRQ_WANTED & (UMAC_IRQ_TBUF_UNDERRUN | UMAC_IRQ_RBUF_OVERFLOW),
        0
    );
    assert_eq!(
        GENET_IRQ_WANTED & (UMAC_IRQ_MDIO_DONE | UMAC_IRQ_MDIO_ERROR),
        0
    );
}

#[test]
fn taking_the_card_masks_and_acks_both_blocks() {
    // El GENET puede venir del cargador de arranque con interrupciones
    // armadas y pendientes: la linea estaria levantada antes de que exista el
    // manejador. Y hay que RECONOCER, no solo enmascarar: un pendiente sin
    // reconocer salta en cuanto se desenmascare cualquier otra cosa.
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT, UMAC_IRQ_ALL);
    hw.poke(GENET_INTRL2_1_OFF + INTRL2_CPU_STAT, UMAC_IRQ_ALL);
    hw.poke(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS, 0);
    hw.poke(GENET_INTRL2_1_OFF + INTRL2_CPU_MASK_STATUS, 0);

    mac.irq_disable_all();

    assert_eq!(hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT), 0);
    assert_eq!(hw.peek(GENET_INTRL2_1_OFF + INTRL2_CPU_STAT), 0);
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        UMAC_IRQ_ALL
    );
    assert_eq!(
        hw.peek(GENET_INTRL2_1_OFF + INTRL2_CPU_MASK_STATUS),
        UMAC_IRQ_ALL,
        "el bloque de las colas comparte la linea: dejarlo armado la cuelga"
    );
    assert_eq!(mac.irq_status(), 0);
}

#[test]
fn unmasking_lets_go_of_the_asked_bits_and_only_those() {
    // Es un registro de set/clear: escribir la mascara que uno quiere en
    // `MASK_SET` desenmascara todo lo que NO esta en ella.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_disable_all();
    mac.irq_unmask(GENET_IRQ_WANTED);
    assert_eq!(
        hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS),
        !GENET_IRQ_WANTED
    );

    // Y volver a enmascarar una sola no toca las otras.
    mac.irq_mask(UMAC_IRQ_TXDMA_DONE);
    let mask = hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_MASK_STATUS);
    assert_ne!(
        mask & UMAC_IRQ_TXDMA_DONE,
        0,
        "la de transmision no se enmascaro"
    );
    assert_eq!(
        mask & UMAC_IRQ_RXDMA_DONE,
        0,
        "se enmascaro la de recepcion de paso"
    );
    assert_eq!(
        mask & UMAC_IRQ_LINK_EVENT,
        0,
        "se enmascaro el enlace de paso"
    );
}

#[test]
fn a_masked_cause_is_not_part_of_this_interrupt() {
    // El `CPU_STAT` tiene puesto lo que ha pasado, armado o no. Leerlo a secas
    // atribuye a esta interrupcion causas que nadie armo, y el manejador las
    // reconoce: el dia que se armen, el primer evento ya viene reconocido.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_disable_all();
    mac.irq_unmask(GENET_IRQ_WANTED);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE | UMAC_IRQ_RBUF_OVERFLOW);

    assert_eq!(mac.irq_status(), UMAC_IRQ_RXDMA_DONE);
}

#[test]
fn acking_clears_what_was_asked_and_leaves_the_rest_pending() {
    // Se reconoce lo que se va a atender. Un bit reconocido y no atendido es
    // un evento perdido, y en el `RXDMA_DONE` es una trama que se queda en el
    // anillo hasta que llegue la siguiente.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_disable_all();
    mac.irq_unmask(GENET_IRQ_WANTED);
    hw.raise_irq(UMAC_IRQ_RXDMA_DONE | UMAC_IRQ_TXDMA_DONE | UMAC_IRQ_LINK_UP);

    mac.irq_ack(UMAC_IRQ_RXDMA_DONE | UMAC_IRQ_TXDMA_DONE);
    assert_eq!(mac.irq_status(), UMAC_IRQ_LINK_UP);

    mac.irq_ack(UMAC_IRQ_LINK_UP);
    assert_eq!(mac.irq_status(), 0);
}

#[test]
fn acking_nothing_does_not_touch_the_register() {
    // Un manejador que no reconoce nada es un manejador de una linea
    // compartida que no era suya: escribir cero en `CPU_CLEAR` no hace daño,
    // pero la escritura en si misma es un acceso MMIO en el camino del
    // interrupt de otro.
    let hw = Hw::new();
    let mac = hw.mac();
    let marca = hw.write_mark();
    mac.irq_ack(0);
    mac.irq1_ack(0);
    assert_eq!(hw.writes_since(marca), Vec::<usize>::new());
}

#[test]
fn the_priority_queues_can_be_acked_on_their_own() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(GENET_INTRL2_1_OFF + INTRL2_CPU_STAT, 0x8000_0001);
    assert_eq!(mac.irq1_status(), 0x8000_0001);
    mac.irq1_ack(0x8000_0001);
    assert_eq!(mac.irq1_status(), 0);
    // Y el bloque 0 no se ha movido.
    assert_eq!(hw.peek(GENET_INTRL2_0_OFF + INTRL2_CPU_STAT), 0);
}
