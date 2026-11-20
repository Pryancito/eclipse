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
/// How many card numbers are tried for a free one.
const CARDS: u32 = 8;

/// Card `card`'s three nodes, as Linux names and numbers them: the OSS
/// `/dev/dsp<card>` (`/dev/dsp` for card 0, minor 3 + 16 a card), the ALSA
/// control (32 minors a card) and its playback PCM.
fn card_nodes(card: u32) -> [(String, u32, u32); 3] {
    let suffix = if card == 0 {
        String::new()
    } else {
        card.to_string()
    };
    [
        (format!("dsp{suffix}"), SOUND_MAJOR, DSP_MINOR + 16 * card),
        (
            format!("snd/controlC{card}"),
            ALSA_MAJOR,
            CONTROL_MINOR + 32 * card,
        ),
        (
            format!("snd/pcmC{card}D0p"),
            ALSA_MAJOR,
            PCM_MINOR + 32 * card,
        ),
    ]
}

/// Serve the first card whose three nodes are free: card 0 unless the
/// kernel drives a card that took it (it leaves card 0 to this driver,
/// `first_kernel_audio_card`; a kernel without that numbered its GPU's
/// HDMI card 0 and this driver died at every start with
/// `dev_serve /dev/dsp: -26`), or another server holds it.
fn serve_card() -> (u32, [Node; 3]) {
    for card in 0..CARDS {
        let mut made = Vec::new();
        for (name, major, minor) in card_nodes(card) {
            match Node::serve_status(&name, major, minor, 0o666) {
                Ok(node) => made.push(node),
                Err((Some(Status::ALREADY_EXISTS), _)) => {
                    println!(
                        "{NAME}: /dev/{name} is taken (another card's): trying card {}",
                        card + 1
                    );
                    break;
                }
                Err((_, e)) => die(e),
            }
        }
        // Those made of a card with a name taken go when dropped here.
        if let Ok(nodes) = <[Node; 3]>::try_from(made) {
            return (card, nodes);
        }
    }
    die(format_args!("cards 0 to {} are all taken", CARDS - 1))
}

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
    let ctl = Controller::start(&claim).unwrap_or_else(|e| die(format_args!("hda@{index}: {e}")));
    let path = ctl.path;
    let (card_no, [dsp_node, ctl_node, pcm_node]) = serve_card();
    let names = card_nodes(card_no).map(|(name, _, _)| format!("/dev/{name}"));
    let place = format!("hda@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", &names.join(" ")),
            ("card", &card_no.to_string()),
            ("path", &path.describe()),
        ],
    );
    println!(
        "{NAME}: {place} is card {card_no} ({}), on {}",
        names.join(" "),
        path.describe()
    );

    let mut card = Card {
        stream: Stream::new(ctl),
        dsp: Dsp::new(udrivers::now_ns),
        pcm: Pcm::new(udrivers::now_ns),
        ctl: Ctl::default(),
    };
    card.pcm.card = card_no as i32;
    card.ctl.card = card_no as i32;
    let card = &Mutex::new(card);
    // Rung after every request to /dev/dsp or the PCM: one may have opened
    // the card or started the stream, and the stream has to be followed.
    let woken = &Condvar::new();
    let (dsp_node, ctl_node, pcm_node) = (&dsp_node, &ctl_node, &pcm_node);
    let names = &names;
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
            die(format_args!("the kernel let go of {}: {left}", names[1]))
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
            die(format_args!("the kernel let go of {}: {left}", names[2]))
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
        die(format_args!("the kernel let go of {}: {left}", names[0]))
    })
}

#[cfg(test)]
mod tests {
    use super::card_nodes;

    #[test]
    fn cards_are_named_and_numbered_as_linux_does() {
        let names = |card| card_nodes(card).map(|(n, major, minor)| (n, major, minor));
        assert_eq!(
            names(0),
            [
                ("dsp".into(), 14, 3),
                ("snd/controlC0".into(), 116, 0),
                ("snd/pcmC0D0p".into(), 116, 16)
            ]
        );
        assert_eq!(
            names(1),
            [
                ("dsp1".into(), 14, 19),
                ("snd/controlC1".into(), 116, 32),
                ("snd/pcmC1D0p".into(), 116, 48)
            ]
        );
    }
}
