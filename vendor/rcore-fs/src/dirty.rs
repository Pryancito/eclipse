use core::fmt::{Debug, Error, Formatter};
use core::ops::{Deref, DerefMut};

/// Dirty wraps a value of type T with functions similiar to that of a Read/Write
/// lock but simply sets a dirty flag on write(), reset on read()
pub struct Dirty<T> {
    value: T,
    dirty: bool,
}

impl<T> Dirty<T> {
    /// Create a new Dirty
    pub fn new(val: T) -> Dirty<T> {
        Dirty {
            value: val,
            dirty: false,
        }
    }

    /// Create a new Dirty with dirty set
    pub fn new_dirty(val: T) -> Dirty<T> {
        Dirty {
            value: val,
            dirty: true,
        }
    }

    /// Returns true if dirty, false otherwise
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Reset dirty
    pub fn sync(&mut self) {
        self.dirty = false;
    }
}

impl<T> Deref for Dirty<T> {
    type Target = T;

    /// Read the value
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Dirty<T> {
    /// Writable value return, sets the dirty flag
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.value
    }
}

impl<T> Drop for Dirty<T> {
    /// Guard it is not dirty when dropping
    fn drop(&mut self) {
        assert!(!self.dirty, "data dirty when dropping");
    }
}

impl<T: Debug> Debug for Dirty<T> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        let tag = if self.dirty { "Dirty" } else { "Clean" };
        write!(f, "[{}] {:?}", tag, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn a_new_value_is_clean_and_a_read_does_not_dirty_it() {
        let d = Dirty::new(7u32);
        assert!(!d.dirty());
        assert_eq!(*d, 7);
        assert!(!d.dirty(), "reading the value marked it dirty");
        drop(d);
    }

    #[test]
    fn writing_through_the_deref_dirties_it() {
        let mut d = Dirty::new(7u32);
        *d = 8;
        assert!(d.dirty());
        assert_eq!(*d, 8);
        d.sync();
    }

    #[test]
    fn new_dirty_starts_owing_the_disk() {
        let mut d = Dirty::new_dirty(1u8);
        assert!(d.dirty(), "a value made dirty did not say so");
        d.sync();
        assert!(!d.dirty());
    }

    #[test]
    fn sync_is_what_clears_it_and_a_later_write_dirties_it_again() {
        let mut d = Dirty::new(0u8);
        *d = 1;
        d.sync();
        assert!(!d.dirty());
        *d = 2;
        assert!(d.dirty());
        d.sync();
    }

    #[test]
    fn the_tag_in_the_debug_output_says_which_it_is() {
        let mut d = Dirty::new(5u8);
        assert_eq!(format!("{:?}", d), "[Clean] 5");
        *d = 6;
        assert_eq!(format!("{:?}", d), "[Dirty] 6");
        d.sync();
    }

    #[test]
    fn dropping_a_dirty_value_is_refused_loudly() {
        // The guard exists because a `Dirty` that goes out of scope still
        // owing the disk is data the caller believes it wrote. Every user in
        // this tree holds one inside a lock, so the panic is the only notice
        // anybody would get.
        let err = std::panic::catch_unwind(|| {
            let mut d = Dirty::new(0u8);
            *d = 1;
            drop(d);
        })
        .expect_err("a dirty value was dropped in silence");
        let msg = err
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| err.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(msg.contains("dirty when dropping"), "message: {:?}", msg);
    }
}
