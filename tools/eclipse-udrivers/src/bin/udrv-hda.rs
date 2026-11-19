//! `udrv-hda [N]`: the driver of Intel HD Audio controller `N` (default 0)
//! that the kernel left to userspace (`udriver=hda@N` on its command line),
//! served as `/dev/dsp` (`udrivers::oss`) and as ALSA's
//! `/dev/snd/controlC0` and `/dev/snd/pcmC0D0p` (`udrivers::alsa`):
//! `mpg123 -o oss`, `aplay`, PulseAudio's ALSA sink and the rest play
//! through them as through the kernel's. One open plays at a time, of
//! `/dev/dsp` and the PCM.
//!
//! A thread answers each node; another follows the stream every few
//! milliseconds, giving back what the controller played and telling a
//! writer waiting for room. With nothing open and nothing playing that
//! thread sleeps until a request comes, so an idle card costs nothing. When this process goes the nodes go with it;
//! the kernel resets the controller, and the next driver starts it again.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of the node.

use std::process::exit;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use udrivers::alsa::{Ctl, CtlNode, Pcm, PcmNode};
use udrivers::hda::{self, Controller};
use udrivers::oss::{Dsp, DspNode};
use udrivers::served::Node;
use udrivers::status;
use udrivers::stream::{Owner, Stream};
use udrivers::zx::Status;

const NAME: &str = "udrv-hda";
/// `/dev/dsp`: OSS's major and minor.
const SOUND_MAJOR: u32 = 14;
const DSP_MINOR: u32 = 3;
/// ALSA's major; card 0's control and its playback PCM.
const ALSA_MAJOR: u32 = 116;
const CONTROL_MINOR: u32 = 0;
const PCM_MINOR: u32 = 16;

/// The card: its stream, and the nodes over it.
struct Card {
    stream: Stream<Controller>,
    dsp: Dsp,
    pcm: Pcm,
    ctl: Ctl,
}
/// How often the stream is followed: a period of 256 frames at 48 kHz, the
/// shortest an ALSA client is likely to ask for, is 5.3 ms.
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
    let serve = |name: &str, major: u32, minor: u32| {
        Node::serve(name, major, minor, 0o666).unwrap_or_else(|e| die(e))
    };
    let dsp_node = serve("dsp", SOUND_MAJOR, DSP_MINOR);
    let ctl_node = serve("snd/controlC0", ALSA_MAJOR, CONTROL_MINOR);
    let pcm_node = serve("snd/pcmC0D0p", ALSA_MAJOR, PCM_MINOR);
    let place = format!("hda@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", "/dev/dsp /dev/snd/controlC0 /dev/snd/pcmC0D0p"),
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
        "{NAME}: {place} is /dev/dsp and /dev/snd, pin {:#x} from converter {:#x}",
        path.pin, path.conv
    );

    let card = Mutex::new(Card {
        stream: Stream::new(ctl),
        dsp: Dsp::new(udrivers::now_ns),
        pcm: Pcm::new(udrivers::now_ns),
        ctl: Ctl::default(),
    });
    let card = &card;
    // Rung after every request to /dev/dsp or the PCM: one may have opened
    // the card or started the stream, and the stream has to be followed.
    let woken = &Condvar::new();
    let (dsp_node, ctl_node, pcm_node) = (&dsp_node, &ctl_node, &pcm_node);
    std::thread::scope(|s| {
        s.spawn(move || loop {
            let c = card.lock().unwrap_or_else(|p| p.into_inner());
            let idle = |c: &mut Card| c.stream.owner.is_none() && !c.stream.running();
            drop(woken.wait_while(c, idle).unwrap_or_else(|p| p.into_inner()));
            std::thread::sleep(TICK);
            let mut c = card.lock().unwrap_or_else(|p| p.into_inner());
            let c = &mut *c;
            match c.stream.owner {
                Some(Owner::Pcm(_)) => c.pcm.tick(&mut c.stream, Some(pcm_node)),
                _ => c.dsp.tick(&mut c.stream, Some(dsp_node)),
            }
        });
        s.spawn(move || {
            let left = ctl_node.run_with(card, |c, n, req| {
                n.answer(
                    &mut CtlNode {
                        ctl: &mut c.ctl,
                        stream: &mut c.stream,
                    },
                    req,
                )
            });
            die(format_args!(
                "the kernel let go of /dev/snd/controlC0: {left}"
            ))
        });
        s.spawn(move || {
            let left = pcm_node.run_with(card, |c, n, req| {
                let reply = n.answer(
                    &mut PcmNode {
                        pcm: &mut c.pcm,
                        stream: &mut c.stream,
                    },
                    req,
                );
                woken.notify_one();
                reply
            });
            die(format_args!(
                "the kernel let go of /dev/snd/pcmC0D0p: {left}"
            ))
        });
        let left = dsp_node.run_with(card, |c, n, req| {
            let reply = n.answer(
                &mut DspNode {
                    dsp: &mut c.dsp,
                    stream: &mut c.stream,
                },
                req,
            );
            woken.notify_one();
            reply
        });
        die(format_args!("the kernel let go of /dev/dsp: {left}"))
    })
}
