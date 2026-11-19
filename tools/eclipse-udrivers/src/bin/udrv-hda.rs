//! `udrv-hda [N]`: the driver of Intel HD Audio controller `N` (default 0)
//! that the kernel left to userspace (`udriver=hda@N` on its command line),
//! served as `/dev/dsp` (`udrivers::oss`): `mpg123 -o oss`, `sox -t oss`
//! and the rest play through it as through the kernel's.
//!
//! One thread answers the node; another follows the stream every few
//! milliseconds, giving back what the controller played and telling a
//! writer waiting for room. When this process goes the node goes with it;
//! the kernel resets the controller, and the next driver starts it again.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of the node.

use std::process::exit;
use std::sync::Mutex;
use std::time::Duration;

use udrivers::hda::{self, Controller};
use udrivers::oss::Dsp;
use udrivers::served::Node;
use udrivers::status;
use udrivers::zx::Status;

const NAME: &str = "udrv-hda";
/// `/dev/dsp`: OSS's major and minor.
const SOUND_MAJOR: u32 = 14;
const DSP_MINOR: u32 = 3;
/// How often the stream is followed: a tenth of what the ring holds past
/// its guard, at 48 kHz.
const TICK: Duration = Duration::from_millis(5);

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-hda [N]"),
    };
    let claim = match hda::claim(index) {
        Ok(c) => c,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=hda@N and a controller there): {e}"
        )),
    };
    let ctl = Controller::start(&claim).unwrap_or_else(|e| die(e));
    let path = ctl.path;
    let node = Node::serve("dsp", SOUND_MAJOR, DSP_MINOR, 0o666).unwrap_or_else(|e| die(e));
    let place = format!("hda@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", "/dev/dsp"),
            (
                "path",
                &format!(
                    "pin {:#x} converter {:#x} ({})",
                    path.pin,
                    path.conv,
                    if path.digital { "digital" } else { "analog" }
                ),
            ),
        ],
    );
    println!(
        "{NAME}: {place} is /dev/dsp, pin {:#x} from converter {:#x}",
        path.pin, path.conv
    );

    let dsp = Mutex::new(Dsp::new(ctl, udrivers::now_ns));
    let (node, dsp) = (&node, &dsp);
    std::thread::scope(|s| {
        s.spawn(move || loop {
            std::thread::sleep(TICK);
            dsp.lock()
                .unwrap_or_else(|p| p.into_inner())
                .tick(Some(node));
        });
        die(format_args!(
            "the kernel let go of /dev/dsp: {}",
            node.run(dsp)
        ))
    })
}
