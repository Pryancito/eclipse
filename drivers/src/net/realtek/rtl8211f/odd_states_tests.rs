//! The states a healthy card never shows.
//!
//! This file's parent `mod tests` covers the card that works: a partner that
//! offers every speed, a ring with a frame in it, a management bus that
//! answers. What is here is the other side of each of those branches -- an
//! interface mode `open` does not accept, a partner offering gigabit in half
//! duplex only, a receive sweep over an empty ring and over a descriptor whose
//! buffer was never mapped, a transmit slot still in flight, a transmit DMA
//! suspended, a descriptor the DMA finished with an error and one that is not
//! the end of its frame, a PHY interrupt with no traffic behind it, a poll that
//! spends its whole budget, pause in one direction only, and a management bus
//! left busy by the transaction before.
//!
//! No new seam is needed: all of it is reached through [`super::fake`], which
//! already owns the four register windows and the PHY behind the MDIO bus. The
//! comment at the end of the file names what is left uncovered here and why
//! none of it is reachable.

use super::fake::{self, driver, stage_bad_rx, stage_rx_frame, Phy};
use super::*;

// --------------------------------------------------------- bringing it up

/// `open` accepts three of the four interface modes the rest of the driver
/// knows how to clock, and rewrites anything else to `MII` without telling
/// anyone but the log. `GMII` is the mode that makes the consequence visible:
/// `clk_enable` has a branch for it -- the parent module's
/// `each_phy_interface_gets_its_own_clock_setting` programs `0x02 | 0x0C00`
/// when it is called with GMII directly -- and `open` rewrites the field two
/// lines before calling `clk_enable`, so that branch cannot be reached from
/// `open` at all and a GMII board is clocked as MII.
#[test]
fn an_interface_mode_open_does_not_accept_is_rewritten_before_the_clock_is_set() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    nic.phy_mode = GMII;

    nic.open()
        .expect("a healthy GMAC comes up whatever mode it was asked for");

    assert_eq!(
        nic.phy_mode, MII,
        "the field is not left as it was: every later read of it has to see \
         the mode the driver fell back to, not the one it refused"
    );
    assert_eq!(
        fake::read(SYS_CFG_BASE + EMAC_EPHY_CLK_REG0),
        0x0C00,
        "and MII's clock setting is what landed, not GMII's 0x02 | 0x0C00: \
         the rewrite happens first, so clk_enable's GMII branch is dead on \
         this path"
    );
}

/// A partner that offers 1000BASE-T in half duplex and nothing else at
/// gigabit. The driver advertises `ADVERTISE_1000FULL` on its own and clears
/// `ADVERTISE_1000HALF` out of `MII_CTRL1000`, so there is no gigabit setting
/// both ends hold: the negotiation has to fall through to the hundred-megabit
/// ladder instead of bringing the link up at a speed only one end offered.
#[test]
fn a_partner_offering_only_half_duplex_gigabit_comes_up_at_a_hundred() {
    let mut phy = Phy::gigabit_partner();
    phy.regs[MII_STAT1000 as usize] = LPA_1000HALF;
    let _dev = fake::with_phy(phy);
    let mut nic = driver();

    nic.adjust_link()
        .expect("the link is up, just not at gigabit");

    assert_eq!(
        fake::phy_reg(MII_CTRL1000) & ADVERTISE_1000HALF,
        0,
        "the driver never asks for half-duplex gigabit, which is what makes \
         the partner's offer of it unusable"
    );
    assert_eq!(
        fake::read(GMAC_BASE + GETH_BASIC_CTL0) & 0x0f,
        CTL0_DM | 0x08 | 0x04,
        "so the link comes up at a hundred full duplex off MII_LPA, and not \
         at gigabit half on an offer the MAC never made"
    );
}

// ------------------------------------------------------- the receive sweep

/// A receive sweep with nothing in the ring. The slot the read cursor is on
/// still belongs to the DMA, so it ends the sweep where it starts, and the
/// whole reporting block below it is skipped. A poll that found nothing must
/// not move the cursor: the frame that lands in that slot next is the one it
/// would lose.
#[test]
fn a_receive_sweep_over_an_empty_ring_takes_nothing_and_moves_nothing() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    desc_set_own(&mut nic.recv_ring[nic.rx_dirty]);
    assert!(!nic.can_recv(), "the ring is empty to start with");
    let before = nic.rx_dirty;

    let (frame, count) = nic.geth_recv(1);

    assert!(frame.is_empty(), "there was no frame to hand back");
    assert_eq!(count, 0, "and no descriptor was examined");
    assert_eq!(
        nic.rx_dirty, before,
        "the read cursor stayed where it was: walking past a descriptor the \
         DMA still owns loses whatever lands in it"
    );
    assert_eq!(
        nic.rx_clean,
        DMA_DESC_RX - 1,
        "the refill at the end of the sweep runs either way, which is the one \
         thing an empty sweep does do: every slot the driver is not reading \
         goes back to the DMA"
    );
}

/// A descriptor with a frame in it and no buffer behind it. The sweep has
/// already counted the descriptor and moved the cursor past it by the time it
/// looks at the buffer, so the frame is gone either way -- what the check buys
/// is that the driver stops instead of reading from address zero and taking
/// the kernel with it.
#[test]
fn a_receive_descriptor_whose_buffer_was_never_mapped_ends_the_sweep() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    stage_rx_frame(&mut nic, 0, &[0x5au8; 64]);
    nic.recv_buffers[0] = 0;

    let (frame, count) = nic.geth_recv(1);

    assert!(
        frame.is_empty(),
        "nothing can be handed back: there is no buffer to copy out of"
    );
    assert_eq!(
        count, 1,
        "the descriptor was counted before the buffer was looked at, which is \
         what makes the frame unrecoverable rather than retried"
    );
    assert_eq!(
        nic.rx_dirty, 1,
        "and the cursor moved past it, so the sweep after this one starts on \
         the next slot and not on this dead end"
    );
}

/// A poll that spends its whole budget. `geth_recv` counts a descriptor it
/// examined even when the frame in it is discarded, so a run of frames the
/// GMAC rejected is what reaches `BUDGET` -- and at the budget the handler
/// leaves the interrupt line down on purpose, because the work is not finished
/// and the poll has to be re-entered rather than wait for an interrupt that
/// will not come.
#[test]
fn a_poll_that_spends_its_whole_budget_leaves_the_line_down() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    nic.int_enable();
    for slot in 0..BUDGET {
        stage_bad_rx(&mut nic, slot, &[0xdeu8; 64]);
    }
    fake::raise_rx_interrupt();

    assert_eq!(nic.interrupt_handle(0, &0), Ok(1), "the handler answers");
    assert_eq!(
        fake::interrupts_armed(),
        0,
        "the budget was spent, so the line stays down for the poll that has \
         to follow it; arming it here is how a handler like this livelocks"
    );
}

// ------------------------------------------------------ the transmit side

/// `can_send` counts the slots between the two cursors, and that count is not
/// the whole answer: the slot it would write into can still be in flight.
#[test]
fn a_transmit_slot_the_dma_still_owns_is_not_a_slot_to_send_into() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    assert!(nic.can_send(), "an untouched ring has room");

    desc_set_own(&mut nic.send_ring[nic.tx_dirty]);

    assert!(
        !nic.can_send(),
        "the ring has room and the next slot is still the DMA's: writing into \
         it overwrites a descriptor the hardware is reading"
    );
}

/// `can_send` also asks the transmit DMA what it is doing, and accepts two of
/// the eight states: idle (`000`) and suspended for want of a descriptor
/// (`110`). The second one is the state a ring that has just run dry leaves
/// the DMA in, so refusing it would stall the card exactly when there is
/// something to send.
#[test]
fn a_transmit_dma_suspended_for_want_of_a_descriptor_can_still_be_given_one() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();

    fake::write(GMAC_BASE + GETH_TX_DMA_STA, 0b110);
    assert!(
        nic.can_send(),
        "suspended is where the DMA sits between frames"
    );

    fake::write(GMAC_BASE + GETH_TX_DMA_STA, 0b001);
    assert!(
        !nic.can_send(),
        "but a DMA in the middle of fetching a descriptor is not handed \
         another one on top of it"
    );
}

/// A descriptor the DMA finished with an error. The counters the driver keeps
/// here never leave the function -- they go into a `debug!`, and the log macros
/// in this subtree consume their arguments and emit nothing -- so what a test
/// can pin is the reclaim: an errored descriptor has to come back like any
/// other, or one bad frame costs the ring a slot for good.
///
/// Not pinned, for the same reason: `tx_stat` is declared outside the sweep's
/// loop and never reset, so every descriptor behind this one is counted as an
/// error too.
#[test]
fn a_descriptor_the_dma_finished_with_an_error_is_still_reclaimed() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();

    fake::fill_tx_ring(&mut nic);
    fake::complete_whole_tx_ring(&mut nic);
    // Bit 1 is the transmit underflow; bits 10 and 11 are loss of carrier and
    // loss of collision. Those three are what the driver reads as a frame the
    // wire did not take.
    nic.send_ring[0].desc0 |= 0b1 << 1;

    nic.tx_complete();

    assert_eq!(
        fake::tx_cursors(&nic),
        (DMA_DESC_TX - 1, DMA_DESC_TX - 1),
        "the sweep walked past the errored descriptor to the end of the ring"
    );
    assert!(nic.can_send(), "and the ring takes a frame again");
}

/// Only the last descriptor of a frame carries the frame's status; the ones
/// before it have no verdict on them to read. They still have to be handed
/// back, or a frame that needed two descriptors leaks the first one.
#[test]
fn a_descriptor_that_is_not_the_end_of_its_frame_is_reclaimed_without_a_verdict() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();

    fake::fill_tx_ring(&mut nic);
    fake::complete_whole_tx_ring(&mut nic);
    // Bit 30 of desc1 is the last-segment bit `desc_tx_close` sets on the end
    // of a frame. Without it, this is what a frame's first half looks like.
    nic.send_ring[0].desc1 &= !(0b1 << 30);

    nic.tx_complete();

    let desc1 = nic.send_ring[0].desc1;
    assert_eq!(
        desc1,
        1 << 24,
        "the descriptor with no status on it was reinitialised like every \
         other one, which is what hands the slot back"
    );
    assert_eq!(
        fake::tx_cursors(&nic),
        (DMA_DESC_TX - 1, DMA_DESC_TX - 1),
        "and the sweep did not stop on it"
    );
}

// ------------------------------------------------------------ the two buses

/// The PHY's own interrupt arrives on `GETH_RGMII_STA`, not on the DMA's
/// status register, and reading it is the whole of what the driver does with
/// it. What matters is that it is not mistaken for traffic: a link change that
/// came back as a work code would run the receive path over an empty ring on
/// every cable event.
#[test]
fn a_phy_interrupt_is_read_back_and_not_taken_for_traffic() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();

    fake::write(GMAC_BASE + GETH_RGMII_STA, RGMII_IRQ);

    assert_eq!(
        nic.interrupt_status(),
        0,
        "a PHY interrupt is no work for the DMA half of the driver"
    );
    assert_eq!(
        fake::read(GMAC_BASE + GETH_RGMII_STA),
        RGMII_IRQ,
        "and the driver does not write to it: this register clears on a read, \
         and writing the bit back -- which is how the DMA status register is \
         acknowledged -- would be a write to a read-only one"
    );
}

/// `flow_ctrl` is told which directions pause was agreed in, and the two
/// directions live in two different registers. The ladder above it only ever
/// asks for symmetric or receive-only pause, so transmit-only is the arm with
/// nobody to reach it -- and the one a change to that ladder reaches first.
#[test]
fn pause_in_the_transmit_direction_alone_leaves_the_receive_register_alone() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    fake::write(GMAC_BASE + GETH_RX_CTL0, 0);
    fake::write(GMAC_BASE + GETH_TX_FLOW_CTL, 0);

    nic.flow_ctrl(DUPLEX_FULL, FLOW_TX, PAUSE_TIME);

    assert_eq!(
        fake::read(GMAC_BASE + GETH_RX_CTL0) & 0x1_0000,
        0,
        "honouring the partner's pause frames was not what was agreed, and \
         turning it on anyway stops the card on a frame nobody asked for"
    );
    assert_eq!(
        fake::read(GMAC_BASE + GETH_TX_FLOW_CTL),
        0x1 | (PAUSE_TIME << 4),
        "and the transmit side carries both the enable and the pause time"
    );
}

/// The management bus is waited on twice per transaction: once before the
/// command is posted, in case what came before it has not finished, and once
/// after. Only the second wait had a test, because a transaction that starts
/// with the bus idle never enters the first one. The first is the wait that
/// matters on a bus with another master on it, and it has to be bounded for
/// the same reason the second one is: `adjust_link` runs before anything else
/// on this card, so a wait with no bound there is a boot that never ends.
#[test]
fn a_management_bus_left_busy_is_waited_out_before_a_read_is_posted() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    // What the hardware leaves in the register while a transaction runs.
    fake::write(GMAC_BASE + GETH_MDIO_ADDR, MII_BUSY);

    let got = nic.mdio_read(0, MII_BMSR);

    assert_eq!(
        got,
        fake::phy_reg(MII_BMSR),
        "the wait gave up on its bound and the read it was guarding still \
         happened, with the register's real contents"
    );
}

#[test]
fn a_management_bus_left_busy_is_waited_out_before_a_write_is_posted() {
    let _dev = fake::with_phy(Phy::gigabit_partner());
    let mut nic = driver();
    fake::write(GMAC_BASE + GETH_MDIO_ADDR, MII_BUSY);

    nic.mdio_write(0, MII_BMCR, BMCR_ANRESTART);

    assert_eq!(
        fake::phy_reg(MII_BMCR),
        BMCR_ANRESTART,
        "and the write it was guarding still reached the PHY afterwards"
    );
}

// What is left uncovered in this driver, and why none of it is a test:
//
// * The forced-link ladder in `adjust_link` (the `AUTONEG_DISABLE` arm) reads
//   `MII_BMCR` back to decide the speed and the duplex -- two lines after
//   writing `BMCR_SPEED100 | BMCR_FULLDPLX` into that same register under a
//   mask that clears everything except `BMCR_LOOPBACK`, `BMCR_ISOLATE` and
//   `BMCR_PDOWN`. The answer is therefore always a hundred full duplex, and
//   the half-duplex, gigabit and ten-megabit arms of the ladder cannot be
//   reached. It is `genphy_read_status()` copied from Linux, where `BMCR` has
//   not just been written. The same shape as the pause advertisement the
//   parent module's comment describes: a register asked a question whose
//   answer the driver has just written into it.
//
// * The half-duplex gigabit arm of the negotiated path, for the reason
//   `a_partner_offering_only_half_duplex_gigabit_comes_up_at_a_hundred`
//   asserts: the driver clears `ADVERTISE_1000HALF`, so `advgb` can never
//   carry it and the arm behind that `&&` is unreachable. Correct for this
//   MAC -- 1000BASE-T half duplex was standardised and never built -- but it
//   means the condition is two operands where one would do.
//
// * `RxFrameStatus::LlcSnap`. `geth_recv` computes a status that is only ever
//   `DiscardFrame` or `GoodFrame`, so the one comparison against `LlcSnap`
//   has no second answer and the FCS strip behind it always runs.
//
// * The multi-descriptor split in `geth_send`, which sits behind its own
//   oversize guard: `len > MAX_BUF_SZ` returns an error before the loop, so
//   the loop cannot iterate twice. The guard's own comment says the split
//   never fills the extra descriptors' buffers anyway.
