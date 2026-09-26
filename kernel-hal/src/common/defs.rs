use bitflags::bitflags;
use numeric_enum_macro::numeric_enum;

/// The error type which is returned from HAL functions.
/// TODO: more error types.
#[derive(Debug)]
pub struct HalError;

/// The result type returned by HAL functions.
pub type HalResult<T = ()> = core::result::Result<T, HalError>;

bitflags! {
    /// Generic memory flags.
    pub struct MMUFlags: usize {
        #[allow(clippy::identity_op)]
        const CACHE_1   = 1 << 0;
        const CACHE_2   = 1 << 1;
        const READ      = 1 << 2;
        const WRITE     = 1 << 3;
        const EXECUTE   = 1 << 4;
        const USER      = 1 << 5;
        const HUGE_PAGE = 1 << 6;
        const DEVICE    = 1 << 7;
        const RXW = Self::READ.bits | Self::WRITE.bits | Self::EXECUTE.bits;
    }
}
numeric_enum! {
    #[repr(u32)]
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    /// Generic cache policy.
    pub enum CachePolicy {
        Cached = 0,
        Uncached = 1,
        UncachedDevice = 2,
        WriteCombining = 3,
    }
}

// The AArch64 exception-syndrome decoding. Compiled everywhere, not only on
// aarch64: it is pure integer decoding of what the architecture puts in
// `ESR_EL1`, with no register access and nothing target-specific about it, and
// behind a `cfg` it was code that only the machine it runs on could check.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Kind {
    Synchronous = 0,
    Irq = 1,
    Fiq = 2,
    SError = 3,
}

impl Kind {
    /// The exception kind a vector-table entry number names, or `None` if it
    /// names none of them.
    ///
    /// This used to `panic!("bad kind")`, from inside the trap handler, on a
    /// number nobody had checked -- and a panic taken on the way *into* the
    /// trap handler has no context left to report from. There are four kinds
    /// and the caller decides what an unknown one deserves.
    pub fn from_num(x: usize) -> Option<Kind> {
        match x {
            x if x == Kind::Synchronous as usize => Some(Kind::Synchronous),
            x if x == Kind::Irq as usize => Some(Kind::Irq),
            x if x == Kind::Fiq as usize => Some(Kind::Fiq),
            x if x == Kind::SError as usize => Some(Kind::SError),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Source {
    CurrentSpEl0 = 0,
    CurrentSpElx = 1,
    LowerAArch64 = 2,
    LowerAArch32 = 3,
}

impl Source {
    /// Which exception level, and which stack pointer, the exception came
    /// from; `None` if the number names none of the four. Same reasoning as
    /// [`Kind::from_num`].
    pub fn from_num(x: usize) -> Option<Source> {
        match x {
            x if x == Source::CurrentSpEl0 as usize => Some(Source::CurrentSpEl0),
            x if x == Source::CurrentSpElx as usize => Some(Source::CurrentSpElx),
            x if x == Source::LowerAArch64 as usize => Some(Source::LowerAArch64),
            x if x == Source::LowerAArch32 as usize => Some(Source::LowerAArch32),
            _ => None,
        }
    }

    /// Whether the exception came from a lower exception level, i.e. from
    /// user code rather than from the kernel.
    pub fn is_user(self) -> bool {
        matches!(self, Source::LowerAArch64 | Source::LowerAArch32)
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct Info {
    pub source: Source,
    pub kind: Kind,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Fault {
    AddressSize,
    Translation,
    AccessFlag,
    Permission,
    Alignment,
    TlbConflict,
    Other(u8),
}

impl From<u32> for Fault {
    fn from(val: u32) -> Fault {
        use self::Fault::*;

        // IFSC or DFSC bits (ref: D10.2.39, Page 2457~2464).
        match val & 0b111100 {
            0b000000 => AddressSize,
            0b000100 => Translation,
            0b001000 => AccessFlag,
            0b001100 => Permission,
            0b100000 => Alignment,
            0b110000 => TlbConflict,
            _ => Other((val & 0b111111) as u8),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Syndrome {
    Unknown,
    WfiWfe,
    McrMrc,
    McrrMrrc,
    LdcStc,
    SimdFp,
    Vmrs,
    Mrrc,
    IllegalExecutionState,
    Svc(u16),
    Hvc(u16),
    Smc(u16),
    MsrMrsSystem,
    InstructionAbort { kind: Fault, level: u8 },
    PCAlignmentFault,
    DataAbort { kind: Fault, level: u8 },
    SpAlignmentFault,
    TrappedFpu,
    SError,
    Breakpoint,
    Step,
    Watchpoint,
    Brk(u16),
    Other(u32),
}

/// Converts a raw syndrome value (ESR) into a `Syndrome` (ref: D1.10.4, D10.2.39).
impl From<u32> for Syndrome {
    fn from(esr: u32) -> Syndrome {
        use self::Syndrome::*;

        let ec = esr >> 26;
        let iss = esr & 0xFFFFFF;

        match ec {
            0b000000 => Unknown,
            0b000001 => WfiWfe,
            0b000011 => McrMrc,
            0b000100 => McrrMrrc,
            0b000101 => McrMrc,
            0b000110 => LdcStc,
            0b000111 => SimdFp,
            0b001000 => Vmrs,
            0b001100 => Mrrc,
            0b001110 => IllegalExecutionState,
            0b010001 => Svc((iss & 0xFFFF) as u16),
            0b010010 => Hvc((iss & 0xFFFF) as u16),
            0b010011 => Smc((iss & 0xFFFF) as u16),
            0b010101 => Svc((iss & 0xFFFF) as u16),
            0b010110 => Hvc((iss & 0xFFFF) as u16),
            0b010111 => Smc((iss & 0xFFFF) as u16),
            0b011000 => MsrMrsSystem,
            0b100000 | 0b100001 => InstructionAbort {
                kind: Fault::from(iss),
                level: (iss & 0b11) as u8,
            },
            0b100010 => PCAlignmentFault,
            0b100100 | 0b100101 => DataAbort {
                kind: Fault::from(iss),
                level: (iss & 0b11) as u8,
            },
            0b100110 => SpAlignmentFault,
            0b101000 => TrappedFpu,
            0b101100 => TrappedFpu,
            0b101111 => SError,
            0b110000 => Breakpoint,
            0b110001 => Breakpoint,
            0b110010 => Step,
            0b110011 => Step,
            0b110100 => Watchpoint,
            0b110101 => Watchpoint,
            0b111100 => Brk((iss & 0xFFFF) as u16),
            other => Other(other),
        }
    }
}

/// The base page size used by this target.
pub const PAGE_SIZE: usize = super::vm::BASE_PAGE_SIZE as usize;

pub use super::addr::{DevVAddr, PhysAddr, VirtAddr};

#[cfg(test)]
mod aarch64_syndrome_tests {
    use super::*;

    /// Build an `ESR_EL1` value out of its exception class and its syndrome.
    fn esr(ec: u32, iss: u32) -> u32 {
        (ec << 26) | (iss & 0x1ff_ffff)
    }

    // ── the vector-table entry ───────────────────────────────────────────

    #[test]
    fn the_four_exception_kinds_are_the_only_ones_a_vector_names() {
        assert_eq!(Kind::from_num(0), Some(Kind::Synchronous));
        assert_eq!(Kind::from_num(1), Some(Kind::Irq));
        assert_eq!(Kind::from_num(2), Some(Kind::Fiq));
        assert_eq!(Kind::from_num(3), Some(Kind::SError));
        // Anything else is a number nobody checked, and the trap handler used
        // to `panic!("bad kind")` on it -- from inside the trap handler, where
        // a panic has nothing left to report from.
        assert_eq!(Kind::from_num(4), None);
        assert_eq!(Kind::from_num(usize::MAX), None);
    }

    #[test]
    fn the_four_sources_are_the_only_ones_a_vector_names() {
        assert_eq!(Source::from_num(0), Some(Source::CurrentSpEl0));
        assert_eq!(Source::from_num(1), Some(Source::CurrentSpElx));
        assert_eq!(Source::from_num(2), Some(Source::LowerAArch64));
        assert_eq!(Source::from_num(3), Some(Source::LowerAArch32));
        assert_eq!(Source::from_num(4), None);
        assert_eq!(Source::from_num(usize::MAX), None);
    }

    #[test]
    fn only_a_lower_exception_level_is_user_code() {
        // This is what decides whether `MMUFlags::USER` goes into the access a
        // page fault asks for, and the check it ends in is
        // `mapping_flags.contains(access_flags)`. Calling the kernel's own
        // touch of a kernel-only mapping a user access makes it a fatal fault.
        assert!(!Source::CurrentSpEl0.is_user());
        assert!(!Source::CurrentSpElx.is_user());
        assert!(Source::LowerAArch64.is_user());
        assert!(Source::LowerAArch32.is_user());
    }

    // ── DFSC / IFSC ──────────────────────────────────────────────────────

    #[test]
    fn each_fault_status_code_keeps_its_four_levels_together() {
        // D10.2.39: the low two bits of these four groups are the translation
        // level, and the group is what says which question the abort asks.
        for level in 0..4u32 {
            assert_eq!(Fault::from(0b0000_00 | level), Fault::AddressSize);
            assert_eq!(Fault::from(0b0001_00 | level), Fault::Translation);
            assert_eq!(Fault::from(0b0010_00 | level), Fault::AccessFlag);
            assert_eq!(Fault::from(0b0011_00 | level), Fault::Permission);
        }
    }

    #[test]
    fn a_bus_error_is_not_one_of_the_four_the_address_space_can_answer() {
        // Synchronous external abort, not on a translation table walk (0x10),
        // and the same on a walk (0x14). Neither is a question about a
        // mapping, and answering one as a fault to fix is a retry loop over a
        // hardware error.
        assert_eq!(Fault::from(0b01_0000), Fault::Other(0b01_0000));
        assert_eq!(Fault::from(0b01_0100), Fault::Other(0b01_0100));
    }

    #[test]
    fn the_alignment_and_tlb_conflict_codes_are_named() {
        assert_eq!(Fault::from(0b10_0001), Fault::Alignment);
        assert_eq!(Fault::from(0b11_0000), Fault::TlbConflict);
    }

    #[test]
    fn the_status_code_is_read_out_of_the_low_six_bits_alone() {
        // `Fault::from` is handed the whole ISS, which carries WnR (bit 6),
        // CM (bit 8), SAS, SRT and the rest above it. A translation fault is
        // a translation fault whatever a write bit says.
        let iss = 0b0001_11 | (1 << 6) | (1 << 8) | (1 << 24);
        assert_eq!(Fault::from(iss), Fault::Translation);
        // ...and an unnamed code keeps its own six bits, not the masked group.
        assert_eq!(Fault::from(0b01_0001 | (1 << 6)), Fault::Other(0b01_0001));
    }

    // ── EC ───────────────────────────────────────────────────────────────

    #[test]
    fn a_supervisor_call_carries_the_immediate_the_instruction_named() {
        // The AArch64 form (0x15) and the AArch32 form (0x11), and the
        // immediate is the low 16 bits of the ISS -- not the whole of it.
        assert_eq!(
            Syndrome::from(esr(0b01_0101, 0xabcd)),
            Syndrome::Svc(0xabcd)
        );
        assert_eq!(
            Syndrome::from(esr(0b01_0001, 0x1234)),
            Syndrome::Svc(0x1234)
        );
        assert_eq!(
            Syndrome::from(esr(0b01_0101, 0x1_0000 | 0x42)),
            Syndrome::Svc(0x42)
        );
    }

    #[test]
    fn an_abort_carries_its_fault_and_its_level() {
        // 0x20 and 0x24 are the aborts user code takes, 0x21 and 0x25 the
        // ones the kernel takes itself. Which exception level it was is in
        // the vector-table entry, not here, so both forms decode alike.
        for ec in [0b10_0000u32, 0b10_0001] {
            assert_eq!(
                Syndrome::from(esr(ec, 0b0001_10)),
                Syndrome::InstructionAbort {
                    kind: Fault::Translation,
                    level: 2,
                },
                "EC {ec:#08b}"
            );
        }
        for ec in [0b10_0100u32, 0b10_0101] {
            assert_eq!(
                Syndrome::from(esr(ec, 0b0011_11)),
                Syndrome::DataAbort {
                    kind: Fault::Permission,
                    level: 3,
                },
                "EC {ec:#08b}"
            );
        }
    }

    #[test]
    fn a_brk_instruction_carries_its_comment() {
        // `brk #imm16` is what a debugger plants and what `abort()` compiles
        // to on this architecture.
        assert_eq!(
            Syndrome::from(esr(0b11_1100, 0xdead)),
            Syndrome::Brk(0xdead)
        );
        assert_eq!(Syndrome::from(esr(0b11_1100, 1)), Syndrome::Brk(1));
    }

    #[test]
    fn the_debug_exceptions_are_told_apart_from_the_breakpoint_instruction() {
        // 0x30/0x31 are the *hardware* breakpoint, armed through the debug
        // registers; 0x34/0x35 the watchpoint, 0x32/0x33 the single step.
        // None of them is `brk`, which is 0x3c.
        assert_eq!(Syndrome::from(esr(0b11_0000, 0)), Syndrome::Breakpoint);
        assert_eq!(Syndrome::from(esr(0b11_0001, 0)), Syndrome::Breakpoint);
        assert_eq!(Syndrome::from(esr(0b11_0010, 0)), Syndrome::Step);
        assert_eq!(Syndrome::from(esr(0b11_0011, 0)), Syndrome::Step);
        assert_eq!(Syndrome::from(esr(0b11_0100, 0)), Syndrome::Watchpoint);
        assert_eq!(Syndrome::from(esr(0b11_0101, 0)), Syndrome::Watchpoint);
    }

    #[test]
    fn the_named_exception_classes_keep_their_names() {
        let cases = [
            (0b00_0000u32, Syndrome::Unknown),
            (0b00_0001, Syndrome::WfiWfe),
            (0b00_0011, Syndrome::McrMrc),
            (0b00_0100, Syndrome::McrrMrrc),
            (0b00_0101, Syndrome::McrMrc),
            (0b00_0110, Syndrome::LdcStc),
            (0b00_0111, Syndrome::SimdFp),
            (0b00_1000, Syndrome::Vmrs),
            (0b00_1100, Syndrome::Mrrc),
            (0b00_1110, Syndrome::IllegalExecutionState),
            (0b01_1000, Syndrome::MsrMrsSystem),
            (0b10_0010, Syndrome::PCAlignmentFault),
            (0b10_0110, Syndrome::SpAlignmentFault),
            (0b10_1000, Syndrome::TrappedFpu),
            (0b10_1100, Syndrome::TrappedFpu),
            (0b10_1111, Syndrome::SError),
        ];
        for (ec, expected) in cases {
            assert_eq!(Syndrome::from(esr(ec, 0)), expected, "EC {ec:#08b}");
        }
    }

    #[test]
    fn an_exception_class_nobody_decoded_is_reported_by_its_class() {
        // 0x09 is pointer authentication, 0x19 an SVE access, 0x38 the
        // AArch32 `bkpt`: none of them is decoded here, and `Other` carries
        // the class rather than the whole register.
        assert_eq!(
            Syndrome::from(esr(0b00_1001, 0xff)),
            Syndrome::Other(0b00_1001)
        );
        assert_eq!(
            Syndrome::from(esr(0b01_1001, 0xff)),
            Syndrome::Other(0b01_1001)
        );
        assert_eq!(
            Syndrome::from(esr(0b11_1000, 0xff)),
            Syndrome::Other(0b11_1000)
        );
    }

    #[test]
    fn the_class_is_read_out_of_the_top_six_bits() {
        // `esr >> 26`, so the ISS cannot reach the class however wide it is.
        assert_eq!(
            Syndrome::from(esr(0b01_0101, 0x1ff_ffff)),
            Syndrome::Svc(0xffff)
        );
        // ...and an ESR whose top bit is set is still a class, not a negative
        // number: 0x94000000 >> 26 is 0b100101, a data abort the kernel took.
        assert_eq!(
            Syndrome::from(0x9400_0000),
            Syndrome::DataAbort {
                kind: Fault::AddressSize,
                level: 0,
            }
        );
    }
}
