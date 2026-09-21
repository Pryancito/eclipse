//! Linux signals
use crate::error::{LxError, LxResult};
use bitflags::*;
use numeric_enum_macro::numeric_enum;

mod action;

pub use action::*;

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        #[repr(C)]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct FpregsMem {
            mem: [usize; 64]
        }

        impl Default for FpregsMem {
            fn default() -> Self {
                Self {
                    mem: [0; 64]
                }
            }
        }
        /// See musl struct __ucontext
        #[repr(C)]
        #[derive(Clone, Default, Debug)]
        pub struct SignalUserContext {
            pub flags: usize,
            pub link: usize,
            pub stack: SignalStack,
            pub context: MachineContext,
            pub sig_mask: Sigset,
            pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
            pub fpregs_mem: FpregsMem,
        }
    } else if #[cfg(target_arch = "riscv64")] {
        /// See musl struct __ucontext
        #[repr(C)]
        #[derive(Clone, Default, Debug)]
        pub struct SignalUserContext {
            pub flags: usize,
            pub link: usize,
            pub stack: SignalStack,
            pub sig_mask: Sigset,
            pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
            pub context: MachineContext,
        }
    } else { // others structures, this sample is for aarch64
        /// See musl struct __ucontext
        #[repr(C)]
        #[derive(Clone, Default, Debug)]
        pub struct SignalUserContext {
            pub flags: usize,
            pub link: usize,
            pub stack: SignalStack,
            pub sig_mask: Sigset,
            pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
            pub context: MachineContext,
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        /// struct mcontext
        #[repr(C)]
        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        pub struct MachineContext {
            // gregs
            pub r8: usize,
            pub r9: usize,
            pub r10: usize,
            pub r11: usize,
            pub r12: usize,
            pub r13: usize,
            pub r14: usize,
            pub r15: usize,
            pub rdi: usize,
            pub rsi: usize,
            pub rbp: usize,
            pub rbx: usize,
            pub rdx: usize,
            pub rax: usize,
            pub rcx: usize,
            pub rsp: usize,
            pub rip: usize,
            pub eflags: usize,
            pub cs: u16,
            pub gs: u16,
            pub fs: u16,
            pub _pad: u16,
            pub err: usize,
            pub trapno: usize,
            pub oldmask: usize,
            pub cr2: usize,
            // fpregs
            // TODO
            pub fpstate: usize,
            // reserved
            pub _reserved1: [usize; 8],
        }

        impl MachineContext {
            pub fn new(pc : usize) -> Self {
                Self {
                    rip: pc,
                    ..Default::default()
                }
            }

            pub fn get_pc(&self) -> usize {
                self.rip
            }

            pub fn set_pc(&mut self, pc: usize) {
                self.rip = pc;
            }
        }
    } else if #[cfg(target_arch = "riscv64")] {
        /// struct mcontext
        #[repr(C, align(16))]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct MachineContext {
            // general regs, but only regs[0](namely `pc`) is used
            pub general_regs: [usize; 32],
            // fpregs
            pub fpstate: [usize; 66],
        }

        impl Default for MachineContext {
            fn default() -> Self {
                Self {
                    general_regs: [0; 32],
                    fpstate: [0; 66],
                }
            }
        }

        impl MachineContext {
            pub fn new(pc : usize) -> Self {
                let mut n = Self::default();
                n.general_regs[0] = pc;
                n
            }

            pub fn get_pc(&self) -> usize {
                self.general_regs[0]
            }

            pub fn set_pc(&mut self, pc: usize) {
                self.general_regs[0] = pc;
            }
        }
    } else {
        /// TODO: other archs, this sample is for aarch64
        /// struct mcontext
        #[repr(C)]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct MachineContext {
            pub reserved_: [usize; 18 + 256],
        }

        impl Default for MachineContext {
            fn default() -> Self {
                Self {
                    reserved_: [0; 18 + 256],
                }
            }
        }

        impl MachineContext {
            pub fn new(_pc : usize) -> Self {
                unimplemented!();
            }

            pub fn get_pc(&self) -> usize {
                unimplemented!();
            }

            pub fn set_pc(&mut self, _pc: usize) -> usize {
                unimplemented!();
            }
        }
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct SignalFrame {
    /// point to ret_code
    pub ret_code_addr: usize,
    /// Signal Frame info
    pub info: SigInfo,
    /// adapt interface, a little bit waste
    pub ucontext: SignalUserContext,
    /// call sys_sigreturn
    pub ret_code: [u8; 7],
}

bitflags! {
    pub struct SignalStackFlags : u32 {
        const ONSTACK = 1;
        const DISABLE = 2;
        const AUTODISARM = 0x80000000;
    }
}

/// Linux struct stack_t
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SignalStack {
    pub sp: usize,
    pub flags: SignalStackFlags,
    pub size: usize,
}

impl Default for SignalStack {
    fn default() -> Self {
        // default to disabled
        SignalStack {
            sp: 0,
            flags: SignalStackFlags::DISABLE,
            size: 0,
        }
    }
}

numeric_enum! {
    #[repr(u8)]
    #[derive(Eq, PartialEq, Debug, Copy, Clone)]
    pub enum Signal {
        SIGHUP = 1,
        SIGINT = 2,
        SIGQUIT = 3,
        SIGILL = 4,
        SIGTRAP = 5,
        SIGABRT = 6,
        SIGBUS = 7,
        SIGFPE = 8,
        SIGKILL = 9,
        SIGUSR1 = 10,
        SIGSEGV = 11,
        SIGUSR2 = 12,
        SIGPIPE = 13,
        SIGALRM = 14,
        SIGTERM = 15,
        SIGSTKFLT = 16,
        SIGCHLD = 17,
        SIGCONT = 18,
        SIGSTOP = 19,
        SIGTSTP = 20,
        SIGTTIN = 21,
        SIGTTOU = 22,
        SIGURG = 23,
        SIGXCPU = 24,
        SIGXFSZ = 25,
        SIGVTALRM = 26,
        SIGPROF = 27,
        SIGWINCH = 28,
        SIGIO = 29,
        SIGPWR = 30,
        SIGSYS = 31,
        // real time signals
        SIGRT32 = 32,
        SIGRT33 = 33,
        SIGRT34 = 34,
        SIGRT35 = 35,
        SIGRT36 = 36,
        SIGRT37 = 37,
        SIGRT38 = 38,
        SIGRT39 = 39,
        SIGRT40 = 40,
        SIGRT41 = 41,
        SIGRT42 = 42,
        SIGRT43 = 43,
        SIGRT44 = 44,
        SIGRT45 = 45,
        SIGRT46 = 46,
        SIGRT47 = 47,
        SIGRT48 = 48,
        SIGRT49 = 49,
        SIGRT50 = 50,
        SIGRT51 = 51,
        SIGRT52 = 52,
        SIGRT53 = 53,
        SIGRT54 = 54,
        SIGRT55 = 55,
        SIGRT56 = 56,
        SIGRT57 = 57,
        SIGRT58 = 58,
        SIGRT59 = 59,
        SIGRT60 = 60,
        SIGRT61 = 61,
        SIGRT62 = 62,
        SIGRT63 = 63,
        SIGRT64 = 64,
    }
}

impl Signal {
    pub const RTMIN: usize = 32;
    pub const RTMAX: usize = 64;

    /// Read a signal number as it arrives in a syscall argument register.
    ///
    /// Every syscall that takes one declares it `int` — `kill(2)`, `tkill(2)`,
    /// `tgkill(2)`, `rt_sigaction(2)`, `rt_sigqueueinfo(2)`,
    /// `pidfd_send_signal(2)`, `prctl(PR_SET_PDEATHSIG)` — so Linux truncates
    /// the register to 32 bits and then hands it to `valid_signal()`, which
    /// takes it as `unsigned long`: a negative number becomes enormous and is
    /// rejected, and anything above `_NSIG` (64) is rejected too.
    ///
    /// Signal 0 is *valid* and delivers nothing: it is the existence probe
    /// behind `kill(pid, 0)`. It comes back as `Ok(None)` so each caller can
    /// answer it where Linux does — after looking the target up, so a dead
    /// process still gets `ESRCH`. A caller that has no use for it (sigaction)
    /// turns `None` into `EINVAL`.
    ///
    /// Reading the register as `signum as u8` instead, which is what every
    /// call site did, **truncated**: `kill(pid, 265)` delivered SIGKILL and
    /// `prctl(PR_SET_PDEATHSIG, 265)` latched it, where Linux answers EINVAL
    /// to both; and `kill(pid, 0)` answered EINVAL, which is how a stale lock
    /// file looks alive to the program holding it.
    pub fn from_syscall_arg(signum: usize) -> LxResult<Option<Self>> {
        // The declared `int` of the uAPI: drop the high half, keep the sign.
        let sig = signum as u32 as i32;
        if sig == 0 {
            return Ok(None);
        }
        // `valid_signal()`: 1..=_NSIG, with negatives failing as huge unsigned.
        if sig < 1 || sig > Self::RTMAX as i32 {
            return Err(LxError::EINVAL);
        }
        core::convert::TryFrom::try_from(sig as u8)
            .map(Some)
            .map_err(|_| LxError::EINVAL)
    }

    pub fn is_standard(self) -> bool {
        (self as usize) < Self::RTMIN
    }

    pub fn as_bit(&self) -> u64 {
        1 << (*self as u64 - 1)
    }
}

/// The signal number every syscall receives is an `int` in a register, and the
/// tree read it as `signum as u8` in eight places.
///
/// What that costs is not theoretical: the low byte of 265 is 9, so
/// `kill(pid, 265)` killed the target outright, and the low byte of 0 is 0,
/// which no `Signal` variant matches, so `kill(pid, 0)` — the existence probe
/// every lock file in userspace is built on — answered EINVAL. The comment in
/// `sys_kill`'s own `send_to_pid` describes what a profile lock does with
/// ESRCH versus anything else, and the probe never reached it.
#[cfg(test)]
mod signal_arg_tests {
    use super::*;

    #[test]
    fn signal_zero_is_a_probe_not_an_error() {
        // `kill(pid, 0)` sends nothing and reports whether the target exists.
        // `None` is how the caller learns to skip delivery and still answer
        // ESRCH for a pid that is gone.
        assert_eq!(Signal::from_syscall_arg(0), Ok(None));
    }

    #[test]
    fn every_named_signal_survives_the_round_trip() {
        for n in 1..=Signal::RTMAX {
            let got = Signal::from_syscall_arg(n).expect("1..=64 are all valid");
            let got = got.expect("only 0 is the probe");
            assert_eq!(got as usize, n, "signal {} came back as {:?}", n, got);
        }
    }

    #[test]
    fn a_number_above_nsig_is_rejected_instead_of_truncated() {
        // 265 & 0xff == 9 == SIGKILL. This is the whole bug in one line.
        assert_eq!(Signal::from_syscall_arg(265), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(256), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(65), Err(LxError::EINVAL));
        assert_eq!(Signal::from_syscall_arg(usize::MAX), Err(LxError::EINVAL));
    }

    #[test]
    fn the_gap_between_nsig_and_a_full_byte_is_rejected() {
        // 65..=255 fit in a `u8` and would have reached `Signal::try_from`,
        // which rejects them; the range check has to agree, or a stricter
        // guard here would start refusing what the enum accepts.
        for n in (Signal::RTMAX + 1)..=255 {
            assert_eq!(
                Signal::from_syscall_arg(n),
                Err(LxError::EINVAL),
                "{} is not a signal",
                n
            );
        }
    }

    #[test]
    fn a_negative_signal_number_is_rejected_whatever_its_low_byte() {
        // A negative `int` arrives sign-extended in the register. -247 has low
        // byte 9, so `as u8` turned `kill(pid, -247)` into SIGKILL.
        for sig in [-1i32, -9, -247, -256, i32::MIN] {
            let arg = sig as isize as usize;
            assert_eq!(
                Signal::from_syscall_arg(arg),
                Err(LxError::EINVAL),
                "kill(_, {}) must be EINVAL",
                sig
            );
        }
    }

    #[test]
    fn the_high_half_of_the_register_is_dropped_the_way_linux_drops_it() {
        // `SYSCALL_DEFINE` truncates to the declared `int` before validating,
        // so a caller that leaves rubbish in the high 32 bits gets the same
        // answer as one that does not. Matching Linux here matters more than
        // being stricter than it: a guard harder than the kernel's rejects
        // programs the kernel accepts.
        assert_eq!(
            Signal::from_syscall_arg(0xdead_beef_0000_0009),
            Ok(Some(Signal::SIGKILL))
        );
        assert_eq!(Signal::from_syscall_arg(0xffff_ffff_0000_0000), Ok(None));
    }

    #[test]
    fn the_real_time_range_is_reachable() {
        // musl's `SIGRTMIN` is 34 after it reserves three for its own use, so
        // a threaded program that never gets here has no cancellation.
        assert_eq!(
            Signal::from_syscall_arg(Signal::RTMIN),
            Ok(Some(Signal::SIGRT32))
        );
        assert_eq!(
            Signal::from_syscall_arg(Signal::RTMAX),
            Ok(Some(Signal::SIGRT64))
        );
        assert!(!Signal::SIGRT32.is_standard());
        assert!(Signal::SIGSYS.is_standard());
    }
}
