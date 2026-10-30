//! Which card answers which question.
//!
//! `DRM_STATE.drivers` is one list, and three different questions get asked of
//! it: who is the primary, who has the monitor, and who does compute. They
//! have three different answers on a box with more than one card, and getting
//! them from `drivers.first()` -- which, because [`register_driver`] inserts
//! at 0, is the LAST card probed -- is what asked the headless card whether
//! the panel needed a software blit and whether there was a vblank to wait
//! on. Kept together here so the next question that needs an answer is asked
//! next to the other three instead of reaching for the first entry again.

use super::*;

/// Register a new DRM driver. Registering one that is already in the list
/// does nothing, matched by identity (`Arc::ptr_eq`) the way
/// [`unregister_driver`] matches.
///
/// The guard is not defensive. `create_root_fs` walks `drivers::all_drm()` and
/// registers every card it finds, and `LinuxProcess::new` calls
/// `create_root_fs` -- that is what `with_root` exists to avoid "re-scanning
/// disks / re-mounting" for. So every call of it put a SECOND entry in this
/// list for each card already there, and nothing ever took one out: the list
/// is append-only on purpose, because no GPU gets unplugged.
///
/// What that costs: [`super::build_gpu_nodes`] builds one node per entry, so a
/// duplicated card gets a duplicated `/dev/dri/cardN`, with the minors of
/// every card after it pushed along; `software_kms_active()` asks every entry
/// the same question twice; and each duplicate holds an `Arc` clone of the
/// driver for the rest of the boot.
///
/// In the unit-test binary it also made the DRM tests unreliable, which is how
/// it was found: every test that creates a Linux process runs
/// `create_root_fs`, and it did so WITHOUT `test_globals::lock()`, so it
/// re-registered the emulated cards of whichever DRM test was running beside
/// it and moved the card indices under its feet. The suite failed about one
/// run in ten, with the victim changing -- the two-card node test, the exit
/// reclaim tests, `/dev/shm` -- which reads as a real regression and is not
/// one.
pub fn register_driver(driver: Arc<dyn DrmScheme>) {
    let mut state = DRM_STATE.lock();
    if state.drivers.iter().any(|d| Arc::ptr_eq(d, &driver)) {
        return;
    }
    if driver.name() == "simplefb" {
        state.drivers.push(driver);
    } else {
        state.drivers.insert(0, driver);
    }
}

/// Unregister a driver [`register_driver`] added, matched by identity
/// (`Arc::ptr_eq`) rather than by name. Returns whether one was removed.
///
/// Test-only, and `DRM_STATE.drivers` is append-only for a reason: nothing in
/// this kernel unplugs a GPU. But a unit-test binary runs every test of the
/// crate in ONE process, and a registered driver changes the answers the whole
/// DRM core gives -- `software_kms_active()` asks every driver whether it can
/// scan out, `get_primary_driver()` takes the first entry, and `get_resources`
/// filters the topology on whether any driver has hardware KMS. One left behind
/// would put every later test on the hardware path.
#[cfg(test)]
pub(crate) fn unregister_driver(driver: &Arc<dyn DrmScheme>) -> bool {
    let mut state = DRM_STATE.lock();
    match state.drivers.iter().position(|d| Arc::ptr_eq(d, driver)) {
        Some(pos) => {
            state.drivers.remove(pos);
            true
        }
        None => false,
    }
}

/// Get the primary DRM driver
pub fn get_primary_driver() -> Option<Arc<dyn DrmScheme>> {
    DRM_STATE.lock().drivers.first().cloned()
}

/// The DRM driver of the card that drives the display, for the questions that
/// are about the monitor rather than about whoever happens to be first in the
/// list: does the panel need the software blit, is there a real vblank to wait
/// on, and -- since there is no udevd here to enumerate nodes -- which card
/// gets `card0`, the node the session hands wlroots (`WLR_DRM_DEVICES`). See
/// [`super::build_gpu_nodes`].
///
/// **There is no udevd here**, so this also decides which card gets `card0`
/// ([`super::build_gpu_nodes`]). In Linux a compositor enumerates `/dev/dri`
/// through udev and opens the node that has resources; here wlroots' GPU
/// enumeration finds nothing (see `xtask/src/linux/desktop.rs`) and the
/// session names the node itself -- `WLR_DRM_DEVICES`, hardcoded in four
/// places, all of them `/dev/dri/card0`. So `card0` is not one node among
/// several that get tried, it is the only one that gets opened, and it has to
/// be a card that can scan out.
///
/// That is what cost the desktop on 26-oct: once each node answered
/// `GETRESOURCES` for its own card, `card0` was still [`get_primary_driver`]
/// -- the LAST-probed card, because [`register_driver`] inserts at 0, which on
/// a two-card box is the one with no monitor -- so it honestly reported zero
/// CRTCs and labwc exited with *"Unable to open /dev/dri/card0 as KMS device /
/// Found 0 GPUs, cannot create backend"*. The topology was right and the node
/// order was wrong.
///
/// `nvidia.monitor=BB.DD.F` names that card outright. It is spelled as a PCI
/// address and not as `cardN` on purpose: the node number is what this
/// function's answer *produces*, so naming a node as the input would mean "the
/// card that is currently `card1`", which depends on the PCI probe order and
/// can differ between boots. A BDF names the same card every time, and it is
/// the spelling `nvidia.compute=` already uses. `dmesg | grep /dev/dri` prints
/// one line per card with its BDF, which is where the value comes from.
///
/// Without the flag it is the first registered driver which is not a compute
/// GPU. A compute GPU is one that drives no display by definition (the NVIDIA
/// driver answers `is_compute_gpu()` false exactly for the card whose BAR1
/// holds the boot framebuffer -- the same fact Linux exposes as `boot_vga`),
/// and a non-NVIDIA driver answers false too, which is right: a plain KMS or
/// framebuffer driver does drive the output. When every registered driver says
/// it is a compute GPU there is no better answer than [`get_primary_driver`],
/// so that is the fallback rather than `None` -- a `None` here would silently
/// turn the software-KMS answer around.
///
/// A pin that matches no registered card falls through to the automatic
/// answer. It says so in the klog, but from [`super::build_gpu_nodes`] and not
/// from here: this is on the per-ioctl path ([`super::software_kms_active`]
/// asks it, and so does every page flip), so it must neither allocate nor log.
pub fn get_display_driver() -> Option<Arc<dyn DrmScheme>> {
    // Read before the lock: the first call reads the cmdline, which allocates.
    display_driver_pinned(nvidia_monitor_pin())
}

/// [`get_display_driver`] with the `nvidia.monitor` pin handed in, which is the
/// only way to test what the pin does: the real one comes from the bootloader,
/// and `boot::cmdline()` in a host test build is the empty string.
fn display_driver_pinned(pin: Option<(u8, u8, u8)>) -> Option<Arc<dyn DrmScheme>> {
    let state = DRM_STATE.lock();
    if let Some(pin) = pin {
        if let Some(i) = resolve_bdf_pin(state.drivers.iter().map(|d| d.pci_bdf()), pin) {
            return state.drivers.get(i).cloned();
        }
    }
    state
        .drivers
        .iter()
        .find(|d| !d.is_compute_gpu())
        .or_else(|| state.drivers.first())
        .cloned()
}

/// The `nvidia.monitor=BB.DD.F` pin, cached after the first read.
///
/// [`get_display_driver`] runs per ioctl and `boot::cmdline()` allocates a
/// `String` every call, so the parse happens once. Two racing first callers
/// both parse and both write the same answer, which is why the lock is not
/// held across the parse: allocating under a spinlock is what the cache exists
/// to avoid, not something to do once for tidiness.
fn nvidia_monitor_pin() -> Option<(u8, u8, u8)> {
    static PIN: Mutex<Option<Option<(u8, u8, u8)>>> = Mutex::new(None);
    if let Some(cached) = *PIN.lock() {
        return cached;
    }
    let parsed = kernel_hal::cmdline::NvidiaParams::boot().monitor;
    *PIN.lock() = Some(parsed);
    parsed
}

/// The `nvidia.monitor=BB.DD.F` pin as the kernel parsed it, or `None` when
/// it was not given or did not parse. What
/// `/sys/module/nvidia/parameters/monitor` reads.
pub fn monitor_pin() -> Option<(u8, u8, u8)> {
    nvidia_monitor_pin()
}

/// Say in the klog what `nvidia.monitor=BB.DD.F` did, if it was given.
///
/// Called from [`super::build_gpu_nodes`], which runs once at filesystem setup
/// and already prints the node table the BDF is read off of, and not from
/// [`get_display_driver`], which runs per ioctl and must not log or allocate.
///
/// A pin that names no registered card is a warning and not a failure: the
/// automatic answer is still a working one, and a boot that stops because a
/// cmdline flag has a typo in it is worse than a boot that says so.
pub fn log_monitor_pin() {
    let Some((bus, dev, func)) = nvidia_monitor_pin() else {
        return;
    };
    let matched = {
        let state = DRM_STATE.lock();
        resolve_bdf_pin(state.drivers.iter().map(|d| d.pci_bdf()), (bus, dev, func)).is_some()
    };
    if matched {
        kernel_hal::klog_info!(
            "[drm] nvidia.monitor={:02x}.{:02x}.{:x} -- that card is card0",
            bus,
            dev,
            func
        );
    } else {
        kernel_hal::klog_warn!(
            "[drm] nvidia.monitor={:02x}.{:02x}.{:x} does not match any DRM GPU -- card0 is the \
             automatic pick instead",
            bus,
            dev,
            func
        );
    }
}

/// `nvidia.compute=BB.DD.F` on the kernel cmdline (hex, dots — the cmdline
/// already uses `:` as its token separator, so a PCI BDF with colons cannot
/// be a single token). Example: `nvidia.compute=65.00.0`.
fn parse_nvidia_compute_bdf() -> Option<(u8, u8, u8)> {
    kernel_hal::cmdline::NvidiaParams::boot().compute
}

/// A GPU's PCI address as [`DrmScheme::pci_bdf`] reports it: domain, bus,
/// device, function.
type PciBdf = (u32, u8, u8, u8);

#[cfg(test)]
mod compute_bdf_tests;
/// Que registrar la misma tarjeta dos veces no la ponga dos veces, que es lo
/// que hacía cada `LinuxProcess::new` a través de `create_root_fs`.
#[cfg(test)]
mod dedupe_tests;
#[cfg(test)]
mod monitor_pin_tests;

/// The NVIDIA GPU that owns compute (SAXPY, NVK EXEC, CE-present): either
/// the `nvidia.compute=BB.DD.F` pin, or the first driver with
/// [`DrmScheme::is_compute_gpu`].
///
/// **The pin is obeyed, including when it names the card that scans out.**
/// It used to be dropped in that case, with a klog line, and the automatic
/// choice taken instead. That is the defect [`kernel_hal::cmdline::pci_bdf`] already
/// argues against one screen up — *"a pin is exactly the knob someone reaches
/// for when the automatic choice already went wrong, so it has to either mean
/// what it says or say nothing"* — and refusing by role was the automatic
/// choice overruling the operator about the one thing only the operator knows.
/// On a one-card box it was not even a choice: the only card scans out, so the
/// pin could never name anything else.
///
/// What it cost is real and it is the operator's to spend: that card's GSP
/// boot can wedge the bus, so the klog says plainly what was asked for.
/// Without the pin nothing changes — the automatic fallback below still skips
/// the card that scans out, so a box that asks for nothing gets exactly the
/// node map it got before.
pub fn get_compute_driver() -> Option<Arc<dyn DrmScheme>> {
    let drivers = kernel_hal::drivers::all_drm();
    let list = drivers.as_vec();
    if let Some((bus, dev, func)) = parse_nvidia_compute_bdf() {
        match resolve_bdf_pin(list.iter().map(|d| d.pci_bdf()), (bus, dev, func)) {
            Some(i) => {
                if list[i].is_console_gpu() {
                    kernel_hal::klog_info!(
                        "[drm] nvidia.compute={:02x}.{:02x}.{:x} is the card that scans out the \
                         console -- obeying the pin anyway (its GSP boot can wedge the bus; \
                         that is what was asked for)",
                        bus,
                        dev,
                        func
                    );
                }
                return list.get(i).cloned();
            }
            None => {
                kernel_hal::klog_info!(
                    "[drm] nvidia.compute={:02x}.{:02x}.{:x} does not match any DRM GPU",
                    bus,
                    dev,
                    func
                );
            }
        }
    }
    list.iter().find(|d| d.is_compute_gpu()).cloned()
}

/// Which card a `BB.DD.F` pin names, by index into `cards` -- the registered
/// drivers' PCI addresses in registry order.
///
/// Separate from the callers because they read the real driver registry and
/// the real kernel command line, neither of which a test here can set: this
/// half is the decision. Shared by `nvidia.compute` and `nvidia.monitor` so
/// "names no card" means the same thing to both.
///
/// Takes an iterator rather than a slice because [`get_display_driver`] asks
/// this per ioctl and must not allocate a `Vec` to do it.
///
/// **Whether a card scans out is not consulted.** `nvidia.compute` used to
/// look past the card that scans out, which was the automatic choice
/// overruling the operator about the one thing only the operator knows; its
/// caller still says in the klog what it is about to do. See
/// [`get_compute_driver`] for why.
fn resolve_bdf_pin<I>(mut cards: I, pin: (u8, u8, u8)) -> Option<usize>
where
    I: Iterator<Item = Option<PciBdf>>,
{
    let (bus, dev, func) = pin;
    cards.position(|bdf| matches!(bdf, Some((_, b, dv, f)) if b == bus && dv == dev && f == func))
}
