use {
    super::*,
    crate::vm::{VmObject, PAGE_SIZE},
    crate::{ZxError, ZxResult},
    alloc::sync::Arc,
    core::sync::atomic::{AtomicI64, Ordering},
};

/// A userspace-adjustable clock backed by the platform monotonic clock.
pub struct Clock {
    base: KObjectBase,
    /// How far the synthetic timeline runs ahead of the monotonic one:
    /// `synthetic - reference`, which is all a read needs.
    ///
    /// The correspondence used to be kept as the two halves it arrives in, one
    /// atomic each. A read loaded them one after the other, so a read racing
    /// an update could take the new reference with the old synthetic value and
    /// answer with a time off by the whole size of the adjustment -- in the
    /// usual case, a clock that jumps backwards. One number cannot be torn,
    /// and it is the only number `read` ever uses.
    offset: AtomicI64,
    /// The floor the clock was created with. A synthetic value below it is
    /// refused, which is the whole point of asking for one.
    backstop: i64,
    mapped_vmo: Option<Arc<VmObject>>,
}

impl_kobject!(Clock);

impl Clock {
    /// Create a clock whose initial value is `backstop`.
    pub fn new(backstop: i64, mappable: bool) -> Arc<Self> {
        let mapped_vmo = mappable.then(|| {
            let vmo = VmObject::new_paged(1);
            // This is the error_bound field in Fuchsia's mapped clock ABI.
            // The rest of the zero-filled page, including all padding, must
            // not disclose kernel data.
            vmo.write(0, &u64::MAX.to_ne_bytes()).unwrap();
            vmo
        });
        let now = kernel_hal::timer::timer_now().as_nanos() as i64;
        Arc::new(Self {
            base: KObjectBase::default(),
            offset: AtomicI64::new(backstop.saturating_sub(now)),
            backstop,
            mapped_vmo,
        })
    }

    /// Read the current synthetic time.
    pub fn read(&self) -> i64 {
        let now = kernel_hal::timer::timer_now().as_nanos() as i64;
        now.saturating_add(self.offset.load(Ordering::Relaxed))
    }

    /// Set a reference/synthetic correspondence for subsequent reads.
    ///
    /// Refuses a synthetic value below the backstop the clock was created
    /// with: a backstop that anyone may step over is not a backstop.
    pub fn update(&self, reference: i64, synthetic: i64) -> ZxResult {
        if synthetic < self.backstop {
            return Err(ZxError::INVALID_ARGS);
        }
        self.offset
            .store(synthetic.saturating_sub(reference), Ordering::Relaxed);
        Ok(())
    }

    /// The floor this clock was created with.
    pub fn backstop(&self) -> i64 {
        self.backstop
    }

    pub fn mapped_vmo(&self) -> Option<Arc<VmObject>> {
        self.mapped_vmo.clone()
    }

    pub const fn mapped_size(&self) -> usize {
        PAGE_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicBool;

    fn now() -> i64 {
        kernel_hal::timer::timer_now().as_nanos() as i64
    }

    #[test]
    /// A read is the synthetic value the correspondence named, carried forward
    /// by however much monotonic time has passed since its reference. And a
    /// fresh clock reads as the backstop it was made with, which is the
    /// correspondence `new` writes.
    fn a_read_carries_the_correspondence_forward() {
        let clock = Clock::new(0, false);
        assert!(clock.read() >= 0);

        let synthetic = 777_000_000_000;
        clock.update(now(), synthetic).unwrap();
        let read = clock.read();
        assert!(read >= synthetic, "{} is behind the value set", read);
        assert!(
            read < synthetic + 1_000_000_000,
            "{} is ahead of it by more than the time this test took",
            read,
        );

        let fresh = Clock::new(synthetic, false);
        let read = fresh.read();
        assert!(
            read >= synthetic && read < synthetic + 1_000_000_000,
            "{}",
            read
        );
    }

    #[test]
    /// The backstop is the floor a clock is created with, and a floor anyone
    /// may step over is not a floor: `zx_clock_update` refuses a synthetic
    /// value below it instead of quietly moving the clock into the past.
    fn a_clock_does_not_go_below_the_backstop_it_was_made_with() {
        let backstop = 5_000_000_000;
        let clock = Clock::new(backstop, false);
        assert_eq!(clock.backstop(), backstop);
        assert!(clock.read() >= backstop);

        assert_eq!(
            clock.update(now(), backstop - 1).err(),
            Some(ZxError::INVALID_ARGS),
        );
        assert!(clock.read() >= backstop, "a refused update changes nothing");

        clock.update(now(), backstop).unwrap();
        assert!(clock.read() >= backstop, "and the floor itself is allowed");
    }

    #[test]
    /// The correspondence used to be kept as the two halves it arrives in, one
    /// atomic each, and `read` loaded them one after the other. A read racing
    /// an update could then pair the new reference with the old synthetic
    /// value and answer with a time off by the whole size of the adjustment.
    ///
    /// Both correspondences written below describe the **same** timeline -- an
    /// offset of `K` -- so every honest read is `now + K` no matter how the
    /// two threads interleave, and a torn one is out by a whole second.
    fn a_read_racing_an_update_cannot_take_half_of_each() {
        const K: i64 = 1_000_000_000_000;
        const STEP: i64 = 1_000_000_000;
        const TOLERANCE: i64 = 100_000_000;

        let clock = Clock::new(0, false);
        // The timeline is in place before anyone reads it: a clock nobody has
        // updated yet still reads as its backstop, which is a different
        // timeline and not what this test is about.
        clock.update(0, K).unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let clock = clock.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reference = 0i64;
                while !stop.load(Ordering::Relaxed) {
                    clock.update(reference, reference + K).unwrap();
                    reference = if reference == 0 { STEP } else { 0 };
                }
            })
        };

        for _ in 0..200_000 {
            let read = clock.read();
            let drift = read - now() - K;
            assert!(
                drift.abs() < TOLERANCE,
                "read {} is {} ns off a timeline that never moved",
                read,
                drift,
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
