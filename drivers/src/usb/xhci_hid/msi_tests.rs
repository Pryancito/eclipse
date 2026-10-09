use super::*;
use crate::scheme::{IrqHandler, IrqScheme};
extern crate std;

fn alone_with_the_queue<R>(body: impl FnOnce() -> R) -> R {
    static TURNSTILE: Mutex<()> = Mutex::new(());
    let _guard = TURNSTILE.lock();
    clear();
    let out = body();
    clear();
    out
}

fn clear() {
    MSI_PENDING.lock().clear();
    *MSI_IRQ_HOST.lock() = None;
}

struct PickyIntc {
    refuse: Vec<usize>,
    registered: Mutex<Vec<usize>>,
    unmasked: Mutex<Vec<usize>>,
}

impl PickyIntc {
    fn refusing(refuse: &[usize]) -> Arc<Self> {
        Arc::new(PickyIntc {
            refuse: refuse.to_vec(),
            registered: Mutex::new(Vec::new()),
            unmasked: Mutex::new(Vec::new()),
        })
    }
}

impl Scheme for PickyIntc {
    fn name(&self) -> &str {
        "picky-intc"
    }
}

impl IrqScheme for PickyIntc {
    fn is_valid_irq(&self, _irq: usize) -> bool {
        true
    }
    fn mask(&self, _irq: usize) -> DeviceResult {
        Ok(())
    }
    fn unmask(&self, irq: usize) -> DeviceResult {
        self.unmasked.lock().push(irq);
        Ok(())
    }
    fn register_handler(&self, _irq: usize, _h: IrqHandler) -> DeviceResult {
        Ok(())
    }
    fn register_device(&self, irq: usize, _dev: Arc<dyn Scheme>) -> DeviceResult {
        if self.refuse.contains(&irq) {
            return Err(DeviceError::InvalidParam);
        }
        self.registered.lock().push(irq);
        Ok(())
    }
    fn unregister(&self, _irq: usize) -> DeviceResult {
        Ok(())
    }
}

/// An input device, as far as this queue is concerned.
struct Hid(&'static str);
impl Scheme for Hid {
    fn name(&self) -> &str {
        self.0
    }
}

#[test]
fn a_noted_vector_is_registered_and_unmasked_when_the_walk_finishes() {
    alone_with_the_queue(|| {
        let intc = PickyIntc::refusing(&[]);
        pci_set_irq_host(intc.clone());
        pci_note_pending_msi(11, Arc::new(Hid("keyboard")));
        pci_finish_msi_registrations().expect("the walk failed");
        assert_eq!(intc.registered.lock().clone(), std::vec![11]);
        assert_eq!(intc.unmasked.lock().clone(), std::vec![11]);
        assert!(MSI_PENDING.lock().is_empty());
    })
}

#[test]
fn a_vector_the_controller_refuses_does_not_cost_the_devices_behind_it() {
    // What this used to do: `?` inside the `drain` returned on the first
    // refusal, `Drain::drop` threw away everything behind it, and the
    // caller is `let _ = pci_finish_msi_registrations()`. So the mouse
    // queued after a keyboard whose vector was refused got no interrupt,
    // and not one line was logged anywhere.
    alone_with_the_queue(|| {
        let intc = PickyIntc::refusing(&[7]);
        pci_set_irq_host(intc.clone());
        pci_note_pending_msi(7, Arc::new(Hid("keyboard")));
        pci_note_pending_msi(9, Arc::new(Hid("mouse")));
        pci_finish_msi_registrations().expect("the walk failed");
        assert_eq!(
            intc.registered.lock().clone(),
            std::vec![9],
            "the device behind the refused one lost its interrupt"
        );
        assert!(MSI_PENDING.lock().is_empty());
    })
}

#[test]
fn two_devices_sharing_a_vector_are_both_kept() {
    // The same question as its twin in `net`: the dedup is about a repeated
    // note of one device, not about the vector, which xHCI devices do share.
    alone_with_the_queue(|| {
        let intc = PickyIntc::refusing(&[]);
        pci_set_irq_host(intc.clone());
        pci_note_pending_msi(11, Arc::new(Hid("keyboard")));
        pci_note_pending_msi(11, Arc::new(Hid("mouse")));
        assert_eq!(
            MSI_PENDING.lock().len(),
            2,
            "a device sharing a vector was dropped"
        );
        pci_finish_msi_registrations().expect("the walk failed");
        assert_eq!(intc.registered.lock().clone(), std::vec![11, 11]);
    })
}

#[test]
fn the_same_device_and_vector_is_not_queued_twice() {
    alone_with_the_queue(|| {
        let dev: Arc<dyn Scheme> = Arc::new(Hid("keyboard"));
        pci_note_pending_msi(11, dev.clone());
        pci_note_pending_msi(11, dev.clone());
        assert_eq!(MSI_PENDING.lock().len(), 1);
        pci_note_pending_msi(12, dev);
        assert_eq!(MSI_PENDING.lock().len(), 2);
    })
}

#[test]
fn a_walk_with_no_interrupt_controller_leaves_the_queue_alone() {
    alone_with_the_queue(|| {
        pci_note_pending_msi(11, Arc::new(Hid("keyboard")));
        assert!(
            pci_finish_msi_registrations().is_err(),
            "a walk with nowhere to register reported success"
        );
        assert_eq!(
            MSI_PENDING.lock().len(),
            1,
            "the queue was emptied with nowhere to register"
        );
    })
}

#[test]
fn a_full_queue_says_which_device_it_is_dropping() {
    alone_with_the_queue(|| {
        for v in 0..MAX_MSI_PENDING + 5 {
            pci_note_pending_msi(v, Arc::new(Hid("keyboard")));
        }
        let q = MSI_PENDING.lock();
        assert_eq!(q.len(), MAX_MSI_PENDING);
        assert_eq!(q[0].0, 5, "the oldest entries were not the ones dropped");
    })
}
