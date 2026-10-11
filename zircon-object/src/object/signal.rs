use super::*;
use bitflags::bitflags;

bitflags! {
    /// Signals that waitable kernel objects expose to applications.
    #[derive(Default)]
    pub struct Signal: u32 {
        #[allow(clippy::identity_op)]
        const READABLE                      = 1 << 0;
        const WRITABLE                      = 1 << 1;
        const PEER_CLOSED                   = 1 << 2;
        const SIGNALED                      = 1 << 3;
        const HANDLE_CLOSED                 = 1 << 23;

        const KERNEL_ALL                    = 0xff_ffff;
        const USER_ALL                      = 0xff << 24;

        const CLOCK_STARTED                 = 1 << 4;

        const COUNTER_POSITIVE              = 1 << 4;
        const COUNTER_NON_POSITIVE          = 1 << 5;

        const SOCKET_PEER_WRITE_DISABLED    = 1 << 4;
        const SOCKET_WRITE_DISABLED         = 1 << 5;
        const SOCKET_CONTROL_READABLE       = 1 << 6;
        const SOCKET_CONTROL_WRITABLE       = 1 << 7;
        const SOCKET_ACCEPT                 = 1 << 8;
        const SOCKET_SHARE                  = 1 << 9;
        const SOCKET_READ_THRESHOLD         = 1 << 10;
        const SOCKET_WRITE_THRESHOLD        = 1 << 11;


        const TASK_TERMINATED               = Self::SIGNALED.bits;

        const JOB_TERMINATED                = Self::SIGNALED.bits;
        const JOB_NO_JOBS                   = 1 << 4;
        const JOB_NO_PROCESSES              = 1 << 5;

        const PROCESS_TERMINATED            = Self::SIGNALED.bits;

        const THREAD_TERMINATED             = Self::SIGNALED.bits;
        const THREAD_RUNNING                = 1 << 4;
        const THREAD_SUSPENDED              = 1 << 5;

        const VMO_ZERO_CHILDREN             = Self::SIGNALED.bits;

        const INTERRUPT_SIGNAL              = 1 << 4;

        // for Linux
        const SIGCHLD                       = 1 << 6;

        // for user
        const USER_SIGNAL_0                 = 1 << 24;
        const USER_SIGNAL_1                 = 1 << 25;
        const USER_SIGNAL_2                 = 1 << 26;
        const USER_SIGNAL_3                 = 1 << 27;
        const USER_SIGNAL_4                 = 1 << 28;
        const USER_SIGNAL_5                 = 1 << 29;
        const USER_SIGNAL_6                 = 1 << 30;
        const USER_SIGNAL_7                 = 1 << 31;
    }
}

impl Signal {
    /// Verify whether `number` only sets the bits specified in `allowed_signals`.
    pub fn verify_user_signal(allowed_signals: Signal, number: u32) -> ZxResult<Signal> {
        if (number & !allowed_signals.bits()) != 0 {
            Err(ZxError::INVALID_ARGS)
        } else {
            Ok(Signal::from_bits(number).ok_or(ZxError::INVALID_ARGS)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_user_signal() {
        assert_eq!(
            Err(ZxError::INVALID_ARGS),
            Signal::verify_user_signal(Signal::USER_ALL, 1 << 0)
        );

        assert_eq!(
            Ok(Signal::USER_SIGNAL_0),
            Signal::verify_user_signal(Signal::USER_ALL, 1 << 24)
        );
    }

    /// The numbers userspace waits on. `zx_object_wait_one`, `zx_object_wait_many`
    /// and `zx_object_signal` take a bare `u32`, so the number *is* the ABI:
    /// a bit that moves here is a program woken by the wrong event, or never
    /// woken at all. The one test this file had built its argument out of
    /// `Signal::USER_ALL`, so every constant could have moved together.
    ///
    /// The names come from `zircon/types.h`, where bits 4..11 are the eight
    /// `__ZX_OBJECT_SIGNAL_n` an object type names for itself -- which is why
    /// so many of these share a bit.
    #[test]
    fn every_signal_is_the_bit_the_uapi_names() {
        for (name, signal, bit) in [
            ("ZX_OBJECT_READABLE", Signal::READABLE, 1u32 << 0),
            ("ZX_OBJECT_WRITABLE", Signal::WRITABLE, 1 << 1),
            ("ZX_OBJECT_PEER_CLOSED", Signal::PEER_CLOSED, 1 << 2),
            ("ZX_EVENT_SIGNALED", Signal::SIGNALED, 1 << 3),
            ("ZX_CLOCK_STARTED", Signal::CLOCK_STARTED, 1 << 4),
            ("ZX_COUNTER_POSITIVE", Signal::COUNTER_POSITIVE, 1 << 4),
            (
                "ZX_COUNTER_NON_POSITIVE",
                Signal::COUNTER_NON_POSITIVE,
                1 << 5,
            ),
            (
                "ZX_SOCKET_PEER_WRITE_DISABLED",
                Signal::SOCKET_PEER_WRITE_DISABLED,
                1 << 4,
            ),
            (
                "ZX_SOCKET_WRITE_DISABLED",
                Signal::SOCKET_WRITE_DISABLED,
                1 << 5,
            ),
            (
                "ZX_SOCKET_CONTROL_READABLE",
                Signal::SOCKET_CONTROL_READABLE,
                1 << 6,
            ),
            (
                "ZX_SOCKET_CONTROL_WRITABLE",
                Signal::SOCKET_CONTROL_WRITABLE,
                1 << 7,
            ),
            ("ZX_SOCKET_ACCEPT", Signal::SOCKET_ACCEPT, 1 << 8),
            ("ZX_SOCKET_SHARE", Signal::SOCKET_SHARE, 1 << 9),
            (
                "ZX_SOCKET_READ_THRESHOLD",
                Signal::SOCKET_READ_THRESHOLD,
                1 << 10,
            ),
            (
                "ZX_SOCKET_WRITE_THRESHOLD",
                Signal::SOCKET_WRITE_THRESHOLD,
                1 << 11,
            ),
            ("ZX_JOB_NO_JOBS", Signal::JOB_NO_JOBS, 1 << 4),
            ("ZX_JOB_NO_PROCESSES", Signal::JOB_NO_PROCESSES, 1 << 5),
            ("ZX_THREAD_RUNNING", Signal::THREAD_RUNNING, 1 << 4),
            ("ZX_THREAD_SUSPENDED", Signal::THREAD_SUSPENDED, 1 << 5),
            ("ZX_INTERRUPT_SIGNAL", Signal::INTERRUPT_SIGNAL, 1 << 4),
            ("SIGCHLD (this tree's own)", Signal::SIGCHLD, 1 << 6),
            ("ZX_SIGNAL_HANDLE_CLOSED", Signal::HANDLE_CLOSED, 1 << 23),
            ("ZX_USER_SIGNAL_0", Signal::USER_SIGNAL_0, 1 << 24),
            ("ZX_USER_SIGNAL_1", Signal::USER_SIGNAL_1, 1 << 25),
            ("ZX_USER_SIGNAL_2", Signal::USER_SIGNAL_2, 1 << 26),
            ("ZX_USER_SIGNAL_3", Signal::USER_SIGNAL_3, 1 << 27),
            ("ZX_USER_SIGNAL_4", Signal::USER_SIGNAL_4, 1 << 28),
            ("ZX_USER_SIGNAL_5", Signal::USER_SIGNAL_5, 1 << 29),
            ("ZX_USER_SIGNAL_6", Signal::USER_SIGNAL_6, 1 << 30),
            ("ZX_USER_SIGNAL_7", Signal::USER_SIGNAL_7, 1 << 31),
        ] {
            assert_eq!(signal.bits(), bit, "{} is not the bit it names", name);
        }
        assert_eq!(Signal::KERNEL_ALL.bits(), 0x00ff_ffff);
        assert_eq!(Signal::USER_ALL.bits(), 0xff00_0000);
    }

    /// The four `*_TERMINATED` signals, and `VMO_ZERO_CHILDREN` with them,
    /// are all `SIGNALED`, deliberately: a waiter does not have to know which
    /// kind of object it holds. Written down so the next reader does not
    /// "fix" the duplication, and so moving `SIGNALED` moves all five
    /// together or none.
    #[test]
    fn the_terminated_signals_are_all_the_signaled_bit() {
        for (name, signal) in [
            ("TASK_TERMINATED", Signal::TASK_TERMINATED),
            ("JOB_TERMINATED", Signal::JOB_TERMINATED),
            ("PROCESS_TERMINATED", Signal::PROCESS_TERMINATED),
            ("THREAD_TERMINATED", Signal::THREAD_TERMINATED),
            ("VMO_ZERO_CHILDREN", Signal::VMO_ZERO_CHILDREN),
        ] {
            assert_eq!(signal, Signal::SIGNALED, "{} drifted off SIGNALED", name);
        }
    }

    /// The shape behind the numbers: the word is split in two halves that do
    /// not overlap and leave no bit unowned -- the kernel's 0..23 and
    /// userspace's 24..31 -- and every per-object signal lives in the eight
    /// bits 4..11 that each object type names for itself. An alias that
    /// drifts out of its half is wrong whatever number it holds.
    #[test]
    fn the_kernel_half_and_the_user_half_split_the_word_between_them() {
        assert_eq!(Signal::KERNEL_ALL & Signal::USER_ALL, Signal::empty());
        assert_eq!(
            (Signal::KERNEL_ALL | Signal::USER_ALL).bits(),
            u32::MAX,
            "a bit nobody owns is a bit no caller can be told about"
        );
        assert!(Signal::KERNEL_ALL.contains(Signal::HANDLE_CLOSED));
        assert!(Signal::KERNEL_ALL.contains(Signal::READABLE | Signal::SIGNALED));

        const PER_OBJECT: u32 = 0xff << 4;
        for (name, signal) in [
            ("CLOCK_STARTED", Signal::CLOCK_STARTED),
            ("COUNTER_POSITIVE", Signal::COUNTER_POSITIVE),
            ("COUNTER_NON_POSITIVE", Signal::COUNTER_NON_POSITIVE),
            (
                "SOCKET_PEER_WRITE_DISABLED",
                Signal::SOCKET_PEER_WRITE_DISABLED,
            ),
            ("SOCKET_WRITE_DISABLED", Signal::SOCKET_WRITE_DISABLED),
            ("SOCKET_CONTROL_READABLE", Signal::SOCKET_CONTROL_READABLE),
            ("SOCKET_CONTROL_WRITABLE", Signal::SOCKET_CONTROL_WRITABLE),
            ("SOCKET_ACCEPT", Signal::SOCKET_ACCEPT),
            ("SOCKET_SHARE", Signal::SOCKET_SHARE),
            ("SOCKET_READ_THRESHOLD", Signal::SOCKET_READ_THRESHOLD),
            ("SOCKET_WRITE_THRESHOLD", Signal::SOCKET_WRITE_THRESHOLD),
            ("JOB_NO_JOBS", Signal::JOB_NO_JOBS),
            ("JOB_NO_PROCESSES", Signal::JOB_NO_PROCESSES),
            ("THREAD_RUNNING", Signal::THREAD_RUNNING),
            ("THREAD_SUSPENDED", Signal::THREAD_SUSPENDED),
            ("INTERRUPT_SIGNAL", Signal::INTERRUPT_SIGNAL),
        ] {
            assert_eq!(
                signal.bits() & !PER_OBJECT,
                0,
                "{} is outside the eight bits an object type owns",
                name
            );
            assert_eq!(
                signal.bits().count_ones(),
                1,
                "{} names more than one bit",
                name
            );
        }
        for (name, signal) in [
            ("USER_SIGNAL_0", Signal::USER_SIGNAL_0),
            ("USER_SIGNAL_7", Signal::USER_SIGNAL_7),
        ] {
            assert!(
                Signal::USER_ALL.contains(signal),
                "{} is not in the user half",
                name
            );
        }
    }

    /// What actually stands between userspace and a signal word it may not
    /// set -- which is not what the code reads like. `verify_user_signal` has
    /// two guards, and the second, `Signal::from_bits(..).ok_or(..)?`, **can
    /// never fire**: `KERNEL_ALL` and `USER_ALL` are declared as flags
    /// spanning bits 0..23 and 24..31, so `Signal::all()` is every bit of the
    /// word and `from_bits` accepts any `u32` there is. The mask check is the
    /// whole gate.
    ///
    /// Nothing is exposed by that today, because no `allowed_signals` returns
    /// a set wider than `USER_ALL | SIGNALED` and every bit in it is named.
    /// It is written down because a dead guard reads like a safety net: the
    /// day an object allows a range with unnamed bits in it, the bits go
    /// through whole and no second opinion is coming.
    #[test]
    fn the_mask_is_the_only_guard_that_can_refuse_a_signal() {
        assert_eq!(
            Signal::all().bits(),
            u32::MAX,
            "the two range flags between them name every bit, so from_bits cannot fail"
        );

        // Bit 12 is one of the eleven `zircon/types.h` leaves unassigned.
        const UNASSIGNED: u32 = 1 << 12;
        assert_eq!(Signal::KERNEL_ALL.bits() & UNASSIGNED, UNASSIGNED);
        let through = Signal::verify_user_signal(Signal::KERNEL_ALL, UNASSIGNED)
            .expect("a mask that admits the bit lets it through");
        assert_eq!(
            through.bits(),
            UNASSIGNED,
            "and it goes through whole, under no name"
        );

        // Outside the mask it is refused, which is the guard that works.
        assert_eq!(
            Signal::verify_user_signal(Signal::USER_ALL, UNASSIGNED),
            Err(ZxError::INVALID_ARGS),
        );
        // And a named bit inside the mask comes back as itself.
        assert_eq!(
            Signal::verify_user_signal(Signal::USER_ALL, Signal::USER_SIGNAL_3.bits()),
            Ok(Signal::USER_SIGNAL_3),
        );
    }
}
