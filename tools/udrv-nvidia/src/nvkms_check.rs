//! `udrv-nvidia nvkms-check`: the way nvidia-modeset is asked for on a
//! card whose driver is this process, checked with no GPU.
//!
//! `nvidia.nvkms` has the kernel give each of its NVIDIA cards to
//! nvidia-modeset once the console GPU is up. The cards of the GPUs left to
//! this driver (`udriver=nvidia@N`) are no card of the kernel's, so for
//! those it asks the driver, over each card it serves
//! (`drm_remote::op::NVKMS_LOAD`), and then what the heads say
//! (`SCANOUT_LIVENESS`). On the console GPU that load is the modeset that
//! puts the console back on the monitor after the GPU's bring-up.
//!
//! In QEMU, on a boot with `udriver=nvidia@0:nvidia.nvkms` (a machine with
//! no NVIDIA GPU leaves none, and the kernel still waits for its card):
//!
//! - `udrv-nvidia nvkms-check`: serve a card with nothing behind it at a
//!   place on the bus (`drm::check::Card`). The kernel asks it for
//!   nvidia-modeset, once, logs what it answers, and asks for its heads.
//! - `udrv-nvidia nvkms-check again`, after it: a driver that started
//!   again serves the same GPU, and is not asked a second time in the boot.
use std::process::exit;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::drm::{self, check::Card};
use crate::{fail, NAME};

/// Where the card says it is: a place no PCI function has.
const PLACE: (u32, u8, u8, u8) = (0, 0xfe, 0x1f, 0);

/// The kernel's task looks for cards to ask twice a second.
const ASKED_WITHIN: Duration = Duration::from_secs(10);
const NOT_ASKED_FOR: Duration = Duration::from_secs(3);

pub fn nvkms_check(arg: Option<&str>) {
    let again = match arg {
        None => false,
        Some("again") => true,
        Some(_) => fail("usage: udrv-nvidia nvkms-check [again]".into()),
    };
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !(cmdline.contains("nvidia.nvkms") && cmdline.contains("udriver=nvidia@")) {
        fail("the kernel's line needs udriver=nvidia@0 and nvidia.nvkms".into());
    }
    let card = Arc::new(Card {
        place: Some(PLACE),
        ..Default::default()
    });
    let served = card.clone();
    std::thread::spawn(move || match drm::serve(served) {
        Ok(why) => fail(format!("the kernel let go of the card: {why}")),
        Err(e) => fail(e),
    });
    let loads = || card.nvkms_loads.load(Ordering::Relaxed);
    let phases = || card.liveness.lock().unwrap().clone();
    let until = |d: Duration, done: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !done() && t.elapsed() < d {
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    };
    if again {
        until(NOT_ASKED_FOR, &|| loads() != 0);
        ok(
            "a driver that started again is not asked for nvidia-modeset twice in a boot",
            loads() == 0 && phases().is_empty(),
        );
    } else {
        ok(
            "the kernel asks the card's driver to load nvidia-modeset",
            until(ASKED_WITHIN, &|| loads() == 1),
        );
        ok(
            "...and then what its heads say",
            until(ASKED_WITHIN, &|| phases() == ["post-nvkms"]),
        );
        until(NOT_ASKED_FOR, &|| loads() != 1);
        ok("...once", loads() == 1 && phases() == ["post-nvkms"]);
    }
    println!("{NAME}: nvkms-check: ok");
}
