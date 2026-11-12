//! Looking before sleeping.
//!
//! A sleep and the wake behind it cost more than a fast device, or a client
//! with its next request ready, takes to answer; so a wait may start with a
//! look, a short spin on what is waited for. A look that runs out was wasted
//! (and on one CPU it always is: nobody else ran meanwhile), so after one
//! the next waits sleep straight away, twice as many after each miss in a
//! row (up to [`SKIP_MAX`]), until a look is tried again and pays.

/// The longest run of waits that sleep without looking.
pub const SKIP_MAX: u32 = 64;

/// The looks of one kind of wait: whether the next one is worth it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hybrid {
    on: bool,
    /// Waits left to sleep through.
    skip: u32,
    /// How many the next miss costs.
    backoff: u32,
}

impl Hybrid {
    /// With `on` false no wait ever looks.
    pub const fn new(on: bool) -> Self {
        Hybrid {
            on,
            skip: 0,
            backoff: 1,
        }
    }

    /// Whether this wait starts with a look. When it does, [`Self::done`]
    /// is owed how the look went.
    pub fn try_look(&mut self) -> bool {
        if !self.on {
            return false;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        true
    }

    /// How the look [`Self::try_look`] allowed went.
    pub fn done(&mut self, hit: bool) {
        if hit {
            self.backoff = 1;
            return;
        }
        self.skip = self.backoff;
        if self.backoff < SKIP_MAX {
            self.backoff *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many waits sleep straight away before the next look.
    fn sleeps_until_next_look(h: &mut Hybrid) -> u32 {
        let mut n = 0;
        while !h.try_look() {
            n += 1;
        }
        n
    }

    #[test]
    fn switched_off_it_never_looks() {
        let mut h = Hybrid::new(false);
        for _ in 0..100 {
            assert!(!h.try_look());
        }
    }

    #[test]
    fn looks_that_pay_keep_every_wait_looking() {
        let mut h = Hybrid::new(true);
        for _ in 0..10 {
            assert!(h.try_look());
            h.done(true);
        }
    }

    #[test]
    fn each_miss_in_a_row_doubles_the_waits_slept_through_up_to_the_cap() {
        let mut h = Hybrid::new(true);
        let mut runs = Vec::new();
        for _ in 0..9 {
            assert!(h.try_look());
            h.done(false);
            runs.push(sleeps_until_next_look(&mut h));
        }
        assert_eq!(runs, [1, 2, 4, 8, 16, 32, 64, 64, 64]);
    }

    #[test]
    fn one_look_that_pays_forgets_the_misses() {
        let mut h = Hybrid::new(true);
        for _ in 0..5 {
            assert!(h.try_look());
            h.done(false);
            sleeps_until_next_look(&mut h);
        }
        h.done(true);
        assert!(h.try_look());
        h.done(false);
        assert_eq!(sleeps_until_next_look(&mut h), 1);
    }
}
