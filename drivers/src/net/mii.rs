// From Linux

/* Generic MII registers. */
pub const MII_BMCR: u32 = 0x00;
pub const MII_BMSR: u32 = 0x01;
pub const MII_PHYSID1: u32 = 0x02;
pub const MII_PHYSID2: u32 = 0x03;
pub const MII_ADVERTISE: u32 = 0x04;
pub const MII_LPA: u32 = 0x05;
pub const MII_EXPANSION: u32 = 0x06;
pub const MII_CTRL1000: u32 = 0x09;
pub const MII_STAT1000: u32 = 0x0a;
pub const MII_MMD_CTRL: u32 = 0x0d;
pub const MII_MMD_DATA: u32 = 0x0e;
pub const MII_ESTATUS: u32 = 0x0f;
pub const MII_DCOUNTER: u32 = 0x12;
pub const MII_FCSCOUNTER: u32 = 0x13;
pub const MII_NWAYTEST: u32 = 0x14;
pub const MII_RERRCOUNTER: u32 = 0x15;
pub const MII_SREVISION: u32 = 0x16;
pub const MII_RESV1: u32 = 0x17;
pub const MII_LBRERROR: u32 = 0x18;
pub const MII_PHYADDR: u32 = 0x19;
pub const MII_RESV2: u32 = 0x1a;
pub const MII_TPISTATUS: u32 = 0x1b;
pub const MII_NCONFIG: u32 = 0x1c;

/* Basic mode control register. */
pub const BMCR_RESV: u32 = 0x003f;
pub const BMCR_SPEED1000: u32 = 0x0040;
pub const BMCR_CTST: u32 = 0x0080;
pub const BMCR_FULLDPLX: u32 = 0x0100;
pub const BMCR_ANRESTART: u32 = 0x0200;
pub const BMCR_ISOLATE: u32 = 0x0400;
pub const BMCR_PDOWN: u32 = 0x0800;
pub const BMCR_ANENABLE: u32 = 0x1000;
pub const BMCR_SPEED100: u32 = 0x2000;
pub const BMCR_LOOPBACK: u32 = 0x4000;
pub const BMCR_RESET: u32 = 0x8000;
pub const BMCR_SPEED10: u32 = 0x0000;

/* Basic mode status register. */
pub const BMSR_ERCAP: u32 = 0x0001;
pub const BMSR_JCD: u32 = 0x0002;
pub const BMSR_LSTATUS: u32 = 0x0004;
pub const BMSR_ANEGCAPABLE: u32 = 0x0008;
pub const BMSR_RFAULT: u32 = 0x0010;
pub const BMSR_ANEGCOMPLETE: u32 = 0x0020;
pub const BMSR_RESV: u32 = 0x00c0;
pub const BMSR_ESTATEN: u32 = 0x0100;
pub const BMSR_100HALF2: u32 = 0x0200;
pub const BMSR_100FULL2: u32 = 0x0400;
pub const BMSR_10HALF: u32 = 0x0800;
pub const BMSR_10FULL: u32 = 0x1000;
pub const BMSR_100HALF: u32 = 0x2000;
pub const BMSR_100FULL: u32 = 0x4000;
pub const BMSR_100BASE4: u32 = 0x8000;

/* Advertisement control register. */
pub const ADVERTISE_SLCT: u32 = 0x001f;
pub const ADVERTISE_CSMA: u32 = 0x0001;
pub const ADVERTISE_10HALF: u32 = 0x0020;
pub const ADVERTISE_1000XFULL: u32 = 0x0020;
pub const ADVERTISE_10FULL: u32 = 0x0040;
pub const ADVERTISE_1000XHALF: u32 = 0x0040;
pub const ADVERTISE_100HALF: u32 = 0x0080;
pub const ADVERTISE_1000XPAUSE: u32 = 0x0080;
pub const ADVERTISE_100FULL: u32 = 0x0100;
pub const ADVERTISE_1000XPSE_ASYM: u32 = 0x0100;
pub const ADVERTISE_100BASE4: u32 = 0x0200;
pub const ADVERTISE_PAUSE_CAP: u32 = 0x0400;
pub const ADVERTISE_PAUSE_ASYM: u32 = 0x0800;
pub const ADVERTISE_RESV: u32 = 0x1000;
pub const ADVERTISE_RFAULT: u32 = 0x2000;
pub const ADVERTISE_LPACK: u32 = 0x4000;
pub const ADVERTISE_NPAGE: u32 = 0x8000;

pub const ADVERTISE_FULL: u32 = ADVERTISE_100FULL | ADVERTISE_10FULL | ADVERTISE_CSMA;
pub const ADVERTISE_ALL: u32 =
    ADVERTISE_10HALF | ADVERTISE_10FULL | ADVERTISE_100HALF | ADVERTISE_100FULL;

/* Link partner ability register. */
pub const LPA_SLCT: u32 = 0x001f;
pub const LPA_10HALF: u32 = 0x0020;
pub const LPA_1000XFULL: u32 = 0x0020;
pub const LPA_10FULL: u32 = 0x0040;
pub const LPA_1000XHALF: u32 = 0x0040;
pub const LPA_100HALF: u32 = 0x0080;
pub const LPA_1000XPAUSE: u32 = 0x0080;
pub const LPA_100FULL: u32 = 0x0100;
pub const LPA_1000XPAUSE_ASYM: u32 = 0x0100;
pub const LPA_100BASE4: u32 = 0x0200;
pub const LPA_PAUSE_CAP: u32 = 0x0400;
pub const LPA_PAUSE_ASYM: u32 = 0x0800;
pub const LPA_RESV: u32 = 0x1000;
pub const LPA_RFAULT: u32 = 0x2000;
pub const LPA_LPACK: u32 = 0x4000;
pub const LPA_NPAGE: u32 = 0x8000;

pub const LPA_DUPLEX: u32 = LPA_10FULL | LPA_100FULL;
pub const LPA_100: u32 = LPA_100FULL | LPA_100HALF | LPA_100BASE4;

/* 1000BASE-T Control register */
pub const ADVERTISE_1000FULL: u32 = 0x0200;
pub const ADVERTISE_1000HALF: u32 = 0x0100;
pub const CTL1000_AS_MASTER: u32 = 0x0800;
pub const CTL1000_ENABLE_MASTER: u32 = 0x1000;

/* 1000BASE-T Status register */
pub const LPA_1000MSFAIL: u32 = 0x8000;
pub const LPA_1000LOCALRXOK: u32 = 0x2000;
pub const LPA_1000REMRXOK: u32 = 0x1000;
pub const LPA_1000FULL: u32 = 0x0800;
pub const LPA_1000HALF: u32 = 0x0400;

/* Flow control flags */
pub const FLOW_CTRL_TX: u32 = 0x01;
pub const FLOW_CTRL_RX: u32 = 0x02;

#[cfg(test)]
mod tests {
    use super::*;

    /// These are register bit positions copied from Linux, and the single
    /// register this file is most used for is the basic mode control one: a
    /// transposed digit there resets the PHY (bit 15) where it meant to set
    /// the speed, which takes the link down on a cable that is fine.
    #[test]
    fn the_control_register_bits_are_each_a_single_distinct_bit() {
        let single = [
            ("SPEED1000", BMCR_SPEED1000),
            ("CTST", BMCR_CTST),
            ("FULLDPLX", BMCR_FULLDPLX),
            ("ANRESTART", BMCR_ANRESTART),
            ("ISOLATE", BMCR_ISOLATE),
            ("PDOWN", BMCR_PDOWN),
            ("ANENABLE", BMCR_ANENABLE),
            ("SPEED100", BMCR_SPEED100),
            ("LOOPBACK", BMCR_LOOPBACK),
            ("RESET", BMCR_RESET),
        ];
        let mut seen = 0u32;
        for (name, bit) in single {
            assert_eq!(bit.count_ones(), 1, "BMCR_{} is not one bit", name);
            assert_eq!(seen & bit, 0, "BMCR_{} repeats an earlier bit", name);
            seen |= bit;
        }
        // 10 Mbit is the absence of both speed bits, not a bit of its own.
        assert_eq!(BMCR_SPEED10, 0);
        // And the reserved field is the low six bits, disjoint from all of
        // the above: a named bit that fell inside it would be ignored by the
        // PHY.
        assert_eq!(BMCR_RESV, 0x3f);
        assert_eq!(seen & BMCR_RESV, 0);
    }

    /// The composites are hand-written ORs, and a typo in one is silent: the
    /// PHY simply advertises something other than what the driver meant, and
    /// the link negotiates down.
    #[test]
    fn the_advertisement_composites_are_the_or_of_their_parts() {
        assert_eq!(
            ADVERTISE_FULL,
            ADVERTISE_100FULL | ADVERTISE_10FULL | ADVERTISE_CSMA
        );
        assert_eq!(
            ADVERTISE_ALL,
            ADVERTISE_10HALF | ADVERTISE_10FULL | ADVERTISE_100HALF | ADVERTISE_100FULL
        );
        // `ALL` is every speed/duplex pair and `FULL` only the full-duplex
        // ones, so `FULL` must not claim a half-duplex mode.
        assert_eq!(ADVERTISE_FULL & ADVERTISE_10HALF, 0);
        assert_eq!(ADVERTISE_FULL & ADVERTISE_100HALF, 0);
        // `CSMA` is the selector field, which `ALL` deliberately leaves out.
        assert_eq!(ADVERTISE_ALL & ADVERTISE_CSMA, 0);
        assert_eq!(ADVERTISE_SLCT & ADVERTISE_CSMA, ADVERTISE_CSMA);
    }

    /// Same for the link-partner side, which is what the driver reads to
    /// decide the speed it programs into the MAC.
    #[test]
    fn the_link_partner_composites_are_the_or_of_their_parts() {
        assert_eq!(LPA_DUPLEX, LPA_10FULL | LPA_100FULL);
        assert_eq!(LPA_100, LPA_100FULL | LPA_100HALF | LPA_100BASE4);
        // 100BASE-T4 is not full duplex, so it must not be in `DUPLEX`.
        assert_eq!(LPA_DUPLEX & LPA_100BASE4, 0);
        // And `LPA_100` must not include a 10 Mbit mode, or a 10 Mbit link
        // partner would be read as 100.
        assert_eq!(LPA_100 & LPA_10FULL, 0);
        assert_eq!(LPA_100 & LPA_10HALF, 0);
    }

    /// The advertisement and link-partner registers share a layout, which is
    /// why `LPA_*` and `ADVERTISE_*` have the same values for the same modes.
    /// They are written out twice, so they can drift.
    #[test]
    fn the_advertisement_and_partner_layouts_agree_mode_by_mode() {
        for (adv, lpa, mode) in [
            (ADVERTISE_10HALF, LPA_10HALF, "10HALF"),
            (ADVERTISE_10FULL, LPA_10FULL, "10FULL"),
            (ADVERTISE_100HALF, LPA_100HALF, "100HALF"),
            (ADVERTISE_100FULL, LPA_100FULL, "100FULL"),
            (ADVERTISE_100BASE4, LPA_100BASE4, "100BASE4"),
            (ADVERTISE_PAUSE_CAP, LPA_PAUSE_CAP, "PAUSE_CAP"),
            (ADVERTISE_PAUSE_ASYM, LPA_PAUSE_ASYM, "PAUSE_ASYM"),
            (ADVERTISE_RFAULT, LPA_RFAULT, "RFAULT"),
            (ADVERTISE_LPACK, LPA_LPACK, "LPACK"),
            (ADVERTISE_NPAGE, LPA_NPAGE, "NPAGE"),
        ] {
            assert_eq!(adv, lpa, "{} is at a different bit on each side", mode);
        }
        assert_eq!(ADVERTISE_SLCT, LPA_SLCT);
        assert_eq!(ADVERTISE_RESV, LPA_RESV);
    }

    /// The register NUMBERS are what goes on the MDIO wire, so an off-by-one
    /// reads a different register entirely -- and the three the drivers use
    /// most (control, status, the two ID halves) are consecutive from zero.
    #[test]
    fn the_register_numbers_are_the_mdio_addresses() {
        assert_eq!(
            [MII_BMCR, MII_BMSR, MII_PHYSID1, MII_PHYSID2, MII_ADVERTISE],
            [0, 1, 2, 3, 4]
        );
        assert_eq!(MII_LPA, 5);
        assert_eq!(MII_CTRL1000, 9);
        assert_eq!(MII_STAT1000, 0x0a);
        // MDIO addresses a 32-register space, so none of them may be above 31.
        for r in [MII_BMCR, MII_LPA, MII_CTRL1000, MII_STAT1000, MII_NCONFIG] {
            assert!(r < 32, "register {} is outside the MDIO space", r);
        }
    }

    /// The status register's link bit is the one a driver polls to decide
    /// whether the carrier is up, and `ANEGCOMPLETE` is what it waits for
    /// before reading `LPA`; they must be distinct single bits.
    #[test]
    fn the_status_bits_a_driver_polls_are_distinct() {
        assert_eq!(BMSR_LSTATUS.count_ones(), 1);
        assert_eq!(BMSR_ANEGCOMPLETE.count_ones(), 1);
        assert_ne!(BMSR_LSTATUS, BMSR_ANEGCOMPLETE);
        assert_eq!(BMSR_LSTATUS & BMSR_ANEGCOMPLETE, 0);
        assert_eq!(BMSR_RESV, 0x00c0);
        assert_eq!(BMSR_LSTATUS & BMSR_RESV, 0);
        assert_eq!(BMSR_ANEGCOMPLETE & BMSR_RESV, 0);
    }

    /// The gigabit control and status bits live in their own registers, so
    /// they may repeat values from the 10/100 ones -- but not within their
    /// own register.
    #[test]
    fn the_gigabit_bits_are_distinct_within_their_own_registers() {
        for (a, b) in [
            (ADVERTISE_1000FULL, ADVERTISE_1000HALF),
            (CTL1000_AS_MASTER, CTL1000_ENABLE_MASTER),
        ] {
            assert_eq!(a.count_ones(), 1);
            assert_eq!(b.count_ones(), 1);
            assert_eq!(a & b, 0);
        }
        let status = [
            LPA_1000MSFAIL,
            LPA_1000LOCALRXOK,
            LPA_1000REMRXOK,
            LPA_1000FULL,
            LPA_1000HALF,
        ];
        let mut seen = 0u32;
        for bit in status {
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(seen & bit, 0, "a 1000BASE-T status bit repeats");
            seen |= bit;
        }
    }

    /// The flow-control flags are this file's own two-bit field, not a PHY
    /// register, and the driver ORs them: they must not collide.
    #[test]
    fn the_flow_control_flags_are_two_separate_bits() {
        assert_eq!(FLOW_CTRL_TX | FLOW_CTRL_RX, 0x03);
        assert_eq!(FLOW_CTRL_TX & FLOW_CTRL_RX, 0);
    }
}
