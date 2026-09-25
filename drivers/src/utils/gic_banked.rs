//! Which GICv2 distributor registers are private to a core, and the set a
//! core has to re-assert for itself when it comes online.
//!
//! Interrupt ids `0..32` — the sixteen SGIs and the sixteen PPIs — are
//! *private* to each core, and the distributor registers that control them are
//! **banked per CPU interface**: the same address, read and written through
//! the one distributor mapping, reaches a different copy depending on which
//! core issues the access. Linux says the same thing at the top of
//! `drivers/irqchip/irq-gic.c` ("IRQs 0-31 are special — they are local to each
//! CPU. As such, the enable set/clear, pending set/clear and active bit
//! registers are banked per-cpu for these sources") and acts on it in
//! `gic_cpu_config`, which every core runs for itself from `gic_cpu_init`.
//!
//! So a boot core that enables the timer PPI and the IPI SGI has enabled them
//! **only for itself**. Every other core wakes with its own copy of
//! `GICD_ISENABLER0` at its reset value, which for PPIs is zero. The
//! interrupt still becomes pending at the core — the generic timer keeps
//! counting, an SGI still arrives — and the distributor forwards none of it.
//!
//! Neither half of that is loud. A core with no timer PPI takes no scheduler
//! tick, so it runs whatever it is given until that task yields of its own
//! accord, and reports itself busy the rest of the time. A core with no IPI
//! SGI acknowledges no TLB shootdown, and the initiator's wait for the
//! acknowledgement has no timeout.
//!
//! `BankedEnables` is the boot core's record of what it enabled among those
//! ids, so a core coming up can write the same word into its own bank.

/// Interrupt ids below this are private to a core: SGIs `0..16`, PPIs
/// `16..32`. Everything from 32 up is an SPI, which the distributor holds once
/// for the whole system.
pub const BANKED_IRQ_COUNT: u32 = 32;

/// `true` if this id is one of the private ones, i.e. if enabling it on one
/// core says nothing about any other core.
pub const fn is_banked(irq: u32) -> bool {
    irq < BANKED_IRQ_COUNT
}

/// Byte offset from the distributor base and the bit within the word, for a
/// register that holds one bit per interrupt (`GICD_ISENABLER`,
/// `GICD_ICENABLER`, `GICD_ISPENDR`, ...).
///
/// Returned together because they are two halves of one answer and have been
/// spelled out at each call site: an offset computed from `irq / 32` with a bit
/// from a different `irq` addresses a real register and sets a real interrupt,
/// just not the one asked for.
pub const fn bitmap_slot(reg_base: u32, irq: u32) -> (u32, u32) {
    (reg_base + 4 * (irq / 32), 1 << (irq % 32))
}

/// The set of private interrupt ids a core has enabled, as the word its
/// `GICD_ISENABLER0` bank wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BankedEnables(u32);

impl BankedEnables {
    /// An empty set, which is what a core that has enabled nothing private
    /// should write: no bit set, so the write is a no-op on the hardware.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Record an id if it is private. A shared id is dropped rather than
    /// rejected: the caller enables both kinds through one entry point, and
    /// only the private half is its business to replay.
    ///
    /// Returns whether the id was recorded, so a caller can tell the two
    /// cases apart when it cares.
    pub fn record(&mut self, irq: u32) -> bool {
        if is_banked(irq) {
            self.0 |= 1 << irq;
            true
        } else {
            false
        }
    }

    /// Forget an id, for the caller that disables one.
    pub fn forget(&mut self, irq: u32) -> bool {
        if is_banked(irq) {
            self.0 &= !(1 << irq);
            true
        } else {
            false
        }
    }

    /// The word to write into this core's `GICD_ISENABLER0`.
    pub const fn mask(self) -> u32 {
        self.0
    }

    /// Rebuild from a previously taken [`mask`](Self::mask), for a caller that
    /// stores the set in an atomic rather than in this type.
    pub const fn from_mask(mask: u32) -> Self {
        Self(mask)
    }

    /// Whether this id is in the set.
    pub const fn contains(self, irq: u32) -> bool {
        is_banked(irq) && self.0 & (1 << irq) != 0
    }
}

/// The set is one `u32` handed to one register write, which covers the private
/// range exactly as long as the range is one word wide. Asserted here rather
/// than masked at every use: a `BANKED_IRQ_COUNT` that outgrew a word would
/// silently drop ids from the replay, and this stops the build instead.
const _: () = assert!(
    BANKED_IRQ_COUNT == 32,
    "the private range must be exactly one word of GICD_ISENABLER"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ids this actually exists for on the `virt` board.
    const TIMER_PPI: u32 = 30;
    const IPI_SGI: u32 = 0;
    const UART_SPI: u32 = 33;

    #[test]
    fn the_private_ids_are_the_sgis_and_the_ppis_and_nothing_else() {
        for irq in 0..32 {
            assert!(is_banked(irq), "id {} is private", irq);
        }
        for irq in 32..1024 {
            assert!(!is_banked(irq), "id {} is shared", irq);
        }
    }

    #[test]
    fn the_timer_and_the_ipi_are_private_and_the_uart_is_not() {
        // The whole point, in the three ids the aarch64 port enables: the boot
        // core's writes for the first two reached only its own bank, and the
        // third is the one that genuinely did not need repeating.
        assert!(is_banked(TIMER_PPI), "the generic timer is a PPI");
        assert!(is_banked(IPI_SGI), "the shootdown IPI is an SGI");
        assert!(!is_banked(UART_SPI), "the pl011 is an SPI");
    }

    #[test]
    fn a_bitmap_slot_is_the_word_the_id_lives_in_and_the_bit_inside_it() {
        const ISENABLER: u32 = 0x100;
        assert_eq!(bitmap_slot(ISENABLER, 0), (0x100, 1));
        assert_eq!(bitmap_slot(ISENABLER, 30), (0x100, 1 << 30));
        assert_eq!(bitmap_slot(ISENABLER, 31), (0x100, 1 << 31));
        // The first id of the second word: a new register, bit 0 again.
        assert_eq!(bitmap_slot(ISENABLER, 32), (0x104, 1));
        assert_eq!(bitmap_slot(ISENABLER, 33), (0x104, 1 << 1));
        assert_eq!(bitmap_slot(ISENABLER, 1019), (0x100 + 4 * 31, 1 << 27));
    }

    #[test]
    fn every_id_lands_in_its_own_word_and_bit() {
        // A slot that repeats for two different ids is a write that enables
        // the wrong interrupt, which is the failure this arithmetic has.
        let mut seen = alloc::vec::Vec::new();
        for irq in 0..1024u32 {
            let slot = bitmap_slot(0x100, irq);
            assert!(!seen.contains(&slot), "id {} collides", irq);
            seen.push(slot);
        }
    }

    #[test]
    fn a_core_replays_exactly_what_the_boot_core_made_private() {
        let mut set = BankedEnables::new();
        assert_eq!(set.mask(), 0, "a core that enabled nothing writes nothing");
        assert!(set.record(TIMER_PPI));
        assert!(set.record(UART_SPI) == false, "an SPI is not replayed");
        assert!(set.record(IPI_SGI));
        assert_eq!(set.mask(), (1 << TIMER_PPI) | (1 << IPI_SGI));
        assert!(set.contains(TIMER_PPI) && set.contains(IPI_SGI));
        assert!(!set.contains(UART_SPI));
        // And the word goes into GICD_ISENABLER0, the first word, because
        // every private id lives there.
        assert_eq!(bitmap_slot(0x100, TIMER_PPI).0, 0x100);
        assert_eq!(bitmap_slot(0x100, IPI_SGI).0, 0x100);
    }

    #[test]
    fn forgetting_an_id_takes_it_out_and_leaves_the_rest() {
        let mut set = BankedEnables::new();
        set.record(TIMER_PPI);
        set.record(IPI_SGI);
        assert!(set.forget(TIMER_PPI));
        assert_eq!(set.mask(), 1 << IPI_SGI);
        assert!(!set.forget(UART_SPI), "an SPI was never in the set");
        assert_eq!(set.mask(), 1 << IPI_SGI);
    }

    #[test]
    fn a_set_survives_the_trip_through_storage() {
        // The driver keeps the set in an atomic, not in this type, so every
        // read of it is a `from_mask` of something a `mask` produced.
        let mut set = BankedEnables::new();
        set.record(TIMER_PPI);
        set.record(IPI_SGI);
        set.record(UART_SPI);
        let round_tripped = BankedEnables::from_mask(set.mask());
        assert_eq!(round_tripped, set);
        assert!(round_tripped.contains(TIMER_PPI) && round_tripped.contains(IPI_SGI));
        assert!(!round_tripped.contains(UART_SPI));
        assert_eq!(BankedEnables::from_mask(0), BankedEnables::new());
    }

    #[test]
    fn the_private_range_is_one_whole_word() {
        // `BankedEnables` holds the set in a single `u32` and hands it to a
        // single register write. If the range ever stops being exactly one
        // word, that write stops covering it.
        assert_eq!(BANKED_IRQ_COUNT, 32);
        let mut set = BankedEnables::new();
        for irq in 0..BANKED_IRQ_COUNT {
            assert!(set.record(irq), "id {} is inside the range", irq);
        }
        assert_eq!(set.mask(), u32::MAX, "the whole word, with no id lost");
        assert!(
            !set.record(BANKED_IRQ_COUNT),
            "and the first id past it is not in the range"
        );
    }
}
