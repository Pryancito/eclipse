//! The GPU's interrupt, as the driver asks for it: through the `IrqScheme`
//! `zcore_drivers::net` hands it (`msi_register_and_unmask`, `msi_mask`,
//! `msi_mask_and_unregister`), which in the kernel is the CPU's interrupt
//! controller. Here it is the claim's interrupt object: the kernel takes
//! the MSI and a thread of this process, asleep in `interrupt_wait`, runs
//! the handler the driver registered.
//!
//! The driver numbers the vector as the kernel's PCI scan does (`irq +
//! 32`); a GPU's scheme answers its one vector only, and [`Vectors`] puts
//! every GPU's behind the one scheme the driver is handed. Masking stops the
//! handler from being run, not the kernel from taking the interrupt: the
//! line is the kernel's, armed for the whole claim.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock, RwLock};

use udrivers::zx::Status;
use zcore_drivers::scheme::{IrqHandler, IrqScheme, Scheme};
use zcore_drivers::{DeviceError, DeviceResult};

/// Sleep until the interrupt fires once more.
pub type Wait = Box<dyn Fn() -> Result<(), Status> + Send + Sync>;

pub struct ClaimIrq {
    vector: usize,
    wait: Mutex<Option<Wait>>,
    waiter: Once,
    handler: Mutex<Option<IrqHandler>>,
    masked: AtomicBool,
}

impl ClaimIrq {
    /// The scheme for `vector`, whose interrupts `wait` sleeps for.
    pub fn new(vector: usize, wait: Wait) -> Arc<Self> {
        Arc::new(ClaimIrq {
            vector,
            wait: Mutex::new(Some(wait)),
            waiter: Once::new(),
            handler: Mutex::new(None),
            masked: AtomicBool::new(true),
        })
    }

    /// The thread that sleeps on the interrupt, started with the first
    /// handler and kept: a wait cannot be called off, and the next handler
    /// needs it again.
    fn start(self: &Arc<Self>) {
        let me = self.clone();
        self.waiter.call_once(move || {
            let wait = me.wait.lock().unwrap().take().expect("started once");
            std::thread::spawn(move || loop {
                match wait() {
                    Ok(()) => me.fired(),
                    Err(Status::INTERNAL_INTR_RETRY) => {}
                    Err(e) => {
                        eprintln!("udrv-nvidia: the GPU's interrupt is gone: {e}");
                        break;
                    }
                }
            });
        });
    }

    /// One interrupt: the handler, unless masked. Cloned out of its slot
    /// first, so a handler that masks or unregisters its own vector (the
    /// driver's storm cap does) does not find the slot held.
    fn fired(&self) {
        if self.masked.load(Ordering::Acquire) {
            return;
        }
        let handler = self.handler.lock().unwrap().clone();
        if let Some(handler) = handler {
            handler();
        }
    }

    fn ours(&self, vector: usize) -> DeviceResult {
        if vector == self.vector {
            Ok(())
        } else {
            Err(DeviceError::InvalidParam)
        }
    }
}

impl Scheme for ClaimIrq {
    fn name(&self) -> &str {
        "udrv-nvidia-irq"
    }
}

impl IrqScheme for ClaimIrq {
    fn is_valid_irq(&self, vector: usize) -> bool {
        vector == self.vector
    }

    fn mask(&self, vector: usize) -> DeviceResult {
        self.ours(vector)?;
        self.masked.store(true, Ordering::Release);
        Ok(())
    }

    fn unmask(&self, vector: usize) -> DeviceResult {
        self.ours(vector)?;
        self.masked.store(false, Ordering::Release);
        Ok(())
    }

    fn register_handler(&self, vector: usize, handler: IrqHandler) -> DeviceResult {
        self.ours(vector)?;
        let mut slot = self.handler.lock().unwrap();
        if slot.is_some() {
            return Err(DeviceError::AlreadyExists);
        }
        *slot = Some(handler);
        Ok(())
    }

    fn unregister(&self, vector: usize) -> DeviceResult {
        self.ours(vector)?;
        self.handler.lock().unwrap().take();
        Ok(())
    }
}

/// Every GPU's scheme, one vector each, as the one scheme
/// `zcore_drivers::net` hands the driver: each call goes to the scheme of
/// its vector.
#[derive(Default)]
pub struct Vectors(RwLock<Vec<Arc<ClaimIrq>>>);

impl Vectors {
    /// `irq` beside the others. Refused for a vector already there.
    pub fn add(&self, irq: Arc<ClaimIrq>) -> bool {
        let mut all = self.0.write().unwrap();
        if all.iter().any(|i| i.vector == irq.vector) {
            return false;
        }
        all.push(irq);
        true
    }

    fn of(&self, vector: usize) -> Result<Arc<ClaimIrq>, DeviceError> {
        self.0
            .read()
            .unwrap()
            .iter()
            .find(|i| i.vector == vector)
            .cloned()
            .ok_or(DeviceError::InvalidParam)
    }
}

impl Scheme for Vectors {
    fn name(&self) -> &str {
        "udrv-nvidia-irq"
    }
}

impl IrqScheme for Vectors {
    fn is_valid_irq(&self, vector: usize) -> bool {
        self.of(vector).is_ok()
    }

    fn mask(&self, vector: usize) -> DeviceResult {
        self.of(vector)?.mask(vector)
    }

    fn unmask(&self, vector: usize) -> DeviceResult {
        self.of(vector)?.unmask(vector)
    }

    fn register_handler(&self, vector: usize, handler: IrqHandler) -> DeviceResult {
        self.of(vector)?.register_handler(vector, handler)
    }

    fn unregister(&self, vector: usize) -> DeviceResult {
        self.of(vector)?.unregister(vector)
    }
}

static VECTORS: OnceLock<Arc<Vectors>> = OnceLock::new();

/// Add `irq` to the scheme `zcore_drivers::net` hands the driver, with its
/// waiter running: one per GPU. Refused for a vector already there.
pub fn install(irq: Arc<ClaimIrq>) -> bool {
    let vectors = VECTORS.get_or_init(|| {
        let vectors = Arc::new(Vectors::default());
        zcore_drivers::net::pci_set_irq_host(vectors.clone());
        vectors
    });
    if !vectors.add(irq.clone()) {
        return false;
    }
    irq.start();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A scheme whose interrupt fires each time `tx` is sent to, and the
    /// count of fires the waiter has gone through.
    fn scheme() -> (Arc<ClaimIrq>, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        let (seen_tx, seen_rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let seen_tx = Mutex::new(seen_tx);
        let wait: Wait = Box::new(move || {
            // The previous fire is through `fired` when the next wait
            // starts: say so, then sleep.
            let _ = seen_tx.lock().unwrap().send(());
            rx.lock().unwrap().recv().map_err(|_| Status::PEER_CLOSED)
        });
        let irq = ClaimIrq::new(42, wait);
        irq.start();
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        (irq, tx, seen_rx)
    }

    fn fire(tx: &mpsc::Sender<()>, seen: &mpsc::Receiver<()>) {
        tx.send(()).unwrap();
        seen.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn the_handler_runs_only_while_registered_and_unmasked() {
        let (irq, tx, seen) = scheme();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        irq.register_handler(
            42,
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        // Registered but not yet unmasked, as `msi_register_and_unmask`
        // leaves it between its two calls.
        fire(&tx, &seen);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        irq.unmask(42).unwrap();
        fire(&tx, &seen);
        fire(&tx, &seen);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        irq.mask(42).unwrap();
        fire(&tx, &seen);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        irq.unmask(42).unwrap();
        irq.unregister(42).unwrap();
        fire(&tx, &seen);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_handler_may_mask_its_own_vector() {
        let (irq, tx, seen) = scheme();
        let count = Arc::new(AtomicUsize::new(0));
        let (c, me) = (count.clone(), Arc::downgrade(&irq));
        irq.register_handler(
            42,
            Arc::new(move || {
                // The driver's storm cap: mask from inside the handler.
                if c.fetch_add(1, Ordering::SeqCst) == 1 {
                    me.upgrade().unwrap().mask(42).unwrap();
                }
            }),
        )
        .unwrap();
        irq.unmask(42).unwrap();
        for _ in 0..4 {
            fire(&tx, &seen);
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn only_its_own_vector_and_one_handler() {
        let (irq, _tx, _seen) = scheme();
        assert!(irq.is_valid_irq(42) && !irq.is_valid_irq(43));
        assert!(irq.register_handler(43, Arc::new(|| {})).is_err());
        assert!(irq.unmask(43).is_err());
        irq.register_handler(42, Arc::new(|| {})).unwrap();
        assert!(irq.register_handler(42, Arc::new(|| {})).is_err());
        irq.unregister(42).unwrap();
        irq.register_handler(42, Arc::new(|| {})).unwrap();
    }

    #[test]
    fn two_gpus_vectors_each_reach_their_own_handler() {
        let (one, tx1, seen1) = scheme();
        let (tx2, rx2) = mpsc::channel::<()>();
        let (seen_tx2, seen2) = mpsc::channel::<()>();
        let (rx2, seen_tx2) = (Mutex::new(rx2), Mutex::new(seen_tx2));
        let other = ClaimIrq::new(
            43,
            Box::new(move || {
                let _ = seen_tx2.lock().unwrap().send(());
                rx2.lock().unwrap().recv().map_err(|_| Status::PEER_CLOSED)
            }),
        );
        other.start();
        seen2.recv_timeout(Duration::from_secs(5)).unwrap();
        let vectors = Vectors::default();
        assert!(vectors.add(one.clone()) && vectors.add(other.clone()));
        assert!(!vectors.add(one.clone()), "42 is there already");
        assert!(vectors.is_valid_irq(42) && vectors.is_valid_irq(43) && !vectors.is_valid_irq(44));
        let counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        for (vector, count) in [42, 43].into_iter().zip(&counts) {
            let c = count.clone();
            vectors
                .register_handler(
                    vector,
                    Arc::new(move || {
                        c.fetch_add(1, Ordering::SeqCst);
                    }),
                )
                .unwrap();
            vectors.unmask(vector).unwrap();
        }
        fire(&tx1, &seen1);
        fire(&tx2, &seen2);
        fire(&tx2, &seen2);
        assert_eq!(counts[0].load(Ordering::SeqCst), 1);
        assert_eq!(counts[1].load(Ordering::SeqCst), 2);
        vectors.mask(43).unwrap();
        fire(&tx2, &seen2);
        fire(&tx1, &seen1);
        assert_eq!(counts[0].load(Ordering::SeqCst), 2);
        assert_eq!(counts[1].load(Ordering::SeqCst), 2);
        assert!(vectors.unmask(44).is_err());
    }
}
