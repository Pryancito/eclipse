use std::collections::BTreeMap;

use super::*;

/// A codec: each node's parameters and verbs' answers, and every verb sent.
#[derive(Default)]
struct Fake {
    params: BTreeMap<(u32, u32), u32>,
    answers: BTreeMap<(u32, u32), u32>,
    sent: Vec<(u32, u32, u32)>,
}

impl Verbs for Fake {
    fn send(&mut self, word: u32) -> Option<u32> {
        let nid = (word >> 20) & 0x7f;
        // Every 12-bit verb is 0x7xx or 0xfxx; the rest are 4-bit ones.
        let (verb, payload) = if (word >> 8) & 0xfff >= 0x700 {
            ((word >> 8) & 0xfff, word & 0xff)
        } else {
            ((word >> 16) & 0xf, word & 0xffff)
        };
        self.sent.push((nid, verb, payload));
        if verb == VERB_GET_PARAMETER {
            return Some(*self.params.get(&(nid, payload)).unwrap_or(&0));
        }
        Some(*self.answers.get(&(nid, verb)).unwrap_or(&0))
    }
}

/// QEMU's `hda-output`: an audio function (1) with a converter (2) and a
/// line-out pin (3) on it, which senses nothing.
fn qemu_output() -> Fake {
    let mut f = Fake::default();
    f.params.insert((0, PAR_NODE_COUNT), 1 << 16 | 1);
    f.params.insert((1, PAR_FUNCTION_TYPE), FUNCTION_AUDIO);
    f.params.insert((1, PAR_NODE_COUNT), 2 << 16 | 2);
    // Its own amp caps, as QEMU's converter says (`AC_WCAP_AMP_OVRD`).
    f.params.insert(
        (2, PAR_AUDIO_WIDGET_CAP),
        WIDGET_AUDIO_OUT << 20 | WCAP_OUT_AMP | WCAP_AMP_OVERRIDE,
    );
    f.params.insert((3, PAR_AUDIO_WIDGET_CAP), WIDGET_PIN << 20);
    f.params.insert((3, PAR_PIN_CAP), 1 << 4);
    f.params.insert((3, PAR_CONN_LIST_LEN), 1);
    f.params.insert((2, PAR_OUT_AMP_CAP), 0x4a);
    f.answers.insert((3, VERB_GET_CONN_LIST), 2);
    f
}

fn codec(f: Fake) -> Codec<Fake> {
    Codec { verbs: f, cad: 0 }
}

#[test]
fn the_audio_function_and_its_one_path_are_found() {
    let mut c = codec(qemu_output());
    assert_eq!(c.audio_function(), Some(1));
    let paths = c.paths(1);
    assert_eq!(
        paths,
        [Path {
            afg: 1,
            conv: 2,
            pin: 3,
            ..Path::default()
        }]
    );
    assert!(paths[0].analog_out() && paths[0].via().is_empty());
}

#[test]
fn a_pin_with_no_connector_is_no_path() {
    let mut f = qemu_output();
    f.answers.insert((3, VERB_GET_CONFIG_DEFAULT), 1 << 30);
    assert!(codec(f).paths(1).is_empty());
}

#[test]
fn the_converter_is_found_deep_in_a_long_connection_list() {
    let mut f = qemu_output();
    // Six entries in the long form, two a word: the fake answers 11 and 2
    // for every word, so 2 is at index 1.
    f.params.insert((3, PAR_CONN_LIST_LEN), 0x80 | 6);
    let mut c = codec(f);
    c.verbs
        .answers
        .insert((3, VERB_GET_CONN_LIST), 2 << 16 | 11);
    assert_eq!(c.paths(1)[0].conn, 1);
}

#[test]
fn routing_powers_the_path_and_opens_its_amp() {
    let mut c = codec(qemu_output());
    let path = c.paths(1)[0];
    c.verbs.sent.clear();
    c.route(1, &path).unwrap();
    let sent = &c.verbs.sent;
    for want in [
        (1, VERB_SET_POWER_STATE, 0),
        (2, VERB_SET_POWER_STATE, 0),
        (3, VERB_SET_CONN_SELECT, 0),
        (3, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN),
        // Unmuted, both sides, at the 0 dB step 0x4a.
        (2, VERB16_SET_AMP, 0xb000 | 0x4a),
    ] {
        assert!(sent.contains(&want), "{want:x?} not in {sent:x?}");
    }
}

#[test]
fn a_stream_puts_the_converter_on_its_tag_and_format() {
    let mut c = codec(qemu_output());
    let path = c.paths(1)[0];
    c.verbs.sent.clear();
    c.start(&path, 1, 0x0011).unwrap();
    assert_eq!(
        c.verbs.sent,
        [(2, VERB16_SET_FORMAT, 0x11), (2, VERB_SET_STREAM_ID, 0x10)]
    );
}

#[test]
fn a_display_that_hears_beats_a_jack_that_does_not() {
    let line = Path {
        conv: 2,
        pin: 3,
        ..Path::default()
    };
    let hdmi = Path {
        conv: 4,
        pin: 5,
        digital: true,
        hdmi: true,
        device: DEVICE_DIGITAL_OUT,
        ..line
    };
    assert_eq!(choose(&[(line, false), (hdmi, true)]), Some(hdmi));
    // Nothing plugged anywhere: the first plays, as on QEMU.
    assert_eq!(choose(&[(line, false), (hdmi, false)]), Some(line));
    assert_eq!(choose(&[(line, true), (hdmi, false)]), Some(line));
    assert_eq!(choose(&[]), None);
}

#[test]
fn presence_on_a_digital_pin_is_asked_for_first() {
    let mut c = codec(qemu_output());
    c.verbs.answers.insert((5, VERB_GET_PIN_SENSE), 1 << 31);
    let hdmi = Path {
        conv: 4,
        pin: 5,
        digital: true,
        hdmi: true,
        senses: true,
        ..Path::default()
    };
    assert!(c.present(&hdmi));
    assert_eq!(c.verbs.sent[0], (5, VERB_SET_PIN_SENSE, 0));
}

#[test]
fn formats_say_rate_size_and_channels() {
    assert_eq!(stream_format(48000, 2), 0x0011);
    assert_eq!(stream_format(44100, 2), 0x4011);
    assert_eq!(stream_format(8000, 1), 0x0510);
    assert_eq!(stream_format(192000, 2), 0x1811);
    assert_eq!(nearest_rate(44000), 44100);
    assert_eq!(nearest_rate(1), 8000);
    assert_eq!(nearest_rate(1_000_000), 192000);
}

#[test]
fn a_write_starts_a_lead_ahead_and_fills_to_the_guard() {
    let mut mem = vec![0u8; RING_LEN];
    let mut r = Ring::default();
    let data = vec![7u8; RING_LEN];
    let n = r.write(&mut mem, &data);
    assert_eq!(n, RING_LEN - GUARD - LEAD);
    assert_eq!(r.free(), 0);
    assert!(mem[..LEAD].iter().all(|b| *b == 0));
    assert!(mem[LEAD..LEAD + n].iter().all(|b| *b == 7));
    assert_eq!(r.write(&mut mem, &data), 0);
}

#[test]
fn only_whole_frames_are_taken() {
    let mut mem = vec![0u8; RING_LEN];
    let mut r = Ring::default();
    assert_eq!(r.write(&mut mem, &[1, 2, 3, 4, 5, 6]), 4);
}

#[test]
fn what_is_played_is_zeroed_and_frees_room() {
    let mut mem = vec![0u8; RING_LEN];
    let mut r = Ring::default();
    r.write(&mut mem, &vec![9u8; 8192]);
    assert_eq!(r.played_to(&mut mem, LEAD + 4096), LEAD + 4096);
    assert!(mem[..LEAD + 4096].iter().all(|b| *b == 0));
    assert!(mem[LEAD + 4096..LEAD + 8192].iter().all(|b| *b == 9));
    assert_eq!(r.queued(), 4096);
}

#[test]
fn the_ring_wraps_on_writes_and_on_plays() {
    let mut mem = vec![0u8; RING_LEN];
    let mut r = Ring::default();
    let chunk = vec![3u8; 16384];
    // Around the ring a few times, the play position chasing the writes.
    for _ in 0..4 * RING_LEN / chunk.len() {
        let n = r.write(&mut mem, &chunk);
        assert_eq!(n, chunk.len());
        let to = ((r.played + 16384) % RING_LEN as u64) as usize;
        r.played_to(&mut mem, to);
    }
    assert!(r.written > 3 * RING_LEN as u64);
    // What is queued is 3s and the rest of the ring zeros.
    let queued = r.queued();
    let at = (r.played % RING_LEN as u64) as usize;
    let threes = (0..RING_LEN)
        .filter(|i| mem[(at + i) % RING_LEN] == 3)
        .count();
    assert_eq!(threes, queued);
    assert!((0..queued).all(|i| mem[(at + i) % RING_LEN] == 3));
}

#[test]
fn a_client_that_fell_behind_starts_again_ahead_of_the_controller() {
    let mut mem = vec![0u8; RING_LEN];
    let mut r = Ring::default();
    r.write(&mut mem, &[5u8; 4096]);
    // The controller ran past everything written.
    r.played_to(&mut mem, 30000);
    assert_eq!(r.queued(), 0);
    r.write(&mut mem, &[6u8; 4]);
    assert_eq!(r.written, 30000 + LEAD as u64 + 4);
    assert_eq!(&mem[30000 + LEAD..30000 + LEAD + 4], &[6; 4]);
}

/// A desktop Realtek (ALC892/ALC897/ALC1220 as their codec dumps show
/// them): the line out 0x14 reaches its converter 0x02 only through mixer
/// 0x0c, whose second input is the loopback mixer 0x0b; the front
/// headphone jack 0x1b goes through a selector 0x0d to the same mixer; the
/// rear mic 0x18 is output capable but a mic; S/PDIF 0x1e is straight on
/// digital converter 0x06. The function's amps are shared: the converter
/// overrides nothing.
fn realtek() -> Fake {
    let mut f = Fake::default();
    f.params.insert((0, PAR_NODE_COUNT), 1 << 16 | 1);
    f.params.insert((1, PAR_FUNCTION_TYPE), FUNCTION_AUDIO);
    f.params.insert((1, PAR_NODE_COUNT), 2 << 16 | 0x1d);
    f.params.insert((1, PAR_OUT_AMP_CAP), 0x0005_3f40 | 0x40);
    f.params.insert((1, PAR_IN_AMP_CAP), 0x8000_0000 | 0x17);
    let widget = |f: &mut Fake, nid: u32, caps: u32| {
        f.params.insert((nid, PAR_AUDIO_WIDGET_CAP), caps);
    };
    widget(&mut f, 0x02, WIDGET_AUDIO_OUT << 20 | WCAP_OUT_AMP);
    widget(&mut f, 0x06, WIDGET_AUDIO_OUT << 20 | WCAP_DIGITAL);
    widget(&mut f, 0x0b, WIDGET_MIXER << 20 | WCAP_IN_AMP);
    widget(&mut f, 0x0c, WIDGET_MIXER << 20 | WCAP_IN_AMP);
    widget(&mut f, 0x0d, WIDGET_SELECTOR << 20);
    for (pin, pincap, config, conns) in [
        // Rear green line out: senses, EAPD.
        (
            0x14,
            PINCAP_OUT | PINCAP_PRESENCE | PINCAP_EAPD,
            0x0101_4010,
            0x0c_u32,
        ),
        // Rear pink mic: output capable, a mic.
        (
            0x18,
            PINCAP_OUT | 1 << 5 | PINCAP_PRESENCE,
            0x01a1_9030,
            0x0c,
        ),
        // Front headphones: through the selector.
        (
            0x1b,
            PINCAP_OUT | PINCAP_PRESENCE | PINCAP_HP_DRIVE,
            0x0221_4020,
            0x0d,
        ),
        // S/PDIF out: straight on its converter, senses nothing.
        (0x1e, PINCAP_OUT, 0x0145_6130, 0x06),
    ] {
        widget(
            &mut f,
            pin,
            WIDGET_PIN << 20 | WCAP_OUT_AMP | WCAP_AMP_OVERRIDE,
        );
        f.params.insert((pin, PAR_PIN_CAP), pincap);
        f.params.insert((pin, PAR_CONN_LIST_LEN), 1);
        f.answers.insert((pin, VERB_GET_CONN_LIST), conns);
        f.answers.insert((pin, VERB_GET_CONFIG_DEFAULT), config);
    }
    // The mixer: the converter, then the loopback.
    f.params.insert((0x0c, PAR_CONN_LIST_LEN), 2);
    f.answers
        .insert((0x0c, VERB_GET_CONN_LIST), 0x0b << 8 | 0x02);
    // The selector: the loopback, then the mixer.
    f.params.insert((0x0d, PAR_CONN_LIST_LEN), 2);
    f.answers
        .insert((0x0d, VERB_GET_CONN_LIST), 0x0c << 8 | 0x0b);
    f
}

#[test]
fn a_realtek_line_out_is_found_through_its_mixer() {
    let mut c = codec(realtek());
    let paths = c.paths(1);
    let pins: Vec<_> = paths.iter().map(|p| p.pin).collect();
    // Not the mic.
    assert_eq!(pins, [0x14, 0x1b, 0x1e]);
    let line = paths[0];
    assert_eq!(
        (line.conv, line.conn, line.device),
        (0x02, 0, DEVICE_LINE_OUT)
    );
    assert_eq!(
        line.via(),
        [Hop {
            nid: 0x0c,
            conn: 0,
            mixer: true
        }]
    );
    let hp = paths[1];
    assert_eq!(hp.conv, 0x02);
    assert_eq!(
        hp.via(),
        [
            Hop {
                nid: 0x0d,
                conn: 1,
                mixer: false
            },
            Hop {
                nid: 0x0c,
                conn: 0,
                mixer: true
            }
        ]
    );
    assert!(paths[2].digital && !paths[2].analog_out());
}

#[test]
fn with_nothing_sensed_the_line_out_plays_and_not_the_spdif() {
    let mut c = codec(realtek());
    let paths: Vec<_> = c.paths(1).into_iter().map(|p| (p, false)).collect();
    assert_eq!(choose(&paths).unwrap().pin, 0x14);
    // Headphones plugged into the front beat the rear line out.
    let paths: Vec<_> = paths.iter().map(|(p, _)| (*p, p.pin == 0x1b)).collect();
    assert_eq!(choose(&paths).unwrap().pin, 0x1b);
}

#[test]
fn routing_through_a_mixer_opens_its_input_and_the_shared_amps() {
    let mut c = codec(realtek());
    let paths = c.paths(1);
    let hp = paths[1];
    c.verbs.sent.clear();
    c.route(1, &hp).unwrap();
    let sent = &c.verbs.sent;
    for want in [
        (0x0d, VERB_SET_POWER_STATE, 0),
        (0x0c, VERB_SET_POWER_STATE, 0),
        // The selector on the mixer, the pin driving headphones.
        (0x0d, VERB_SET_CONN_SELECT, 1),
        (0x1b, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN | PIN_CTL_HP_EN),
        // The mixer's input 0 (the converter) open at the function's 0 dB
        // step 0x17, the converter's output at the function's 0x40.
        (0x0c, VERB16_SET_AMP, 0x7000 | 0x17),
        (0x02, VERB16_SET_AMP, 0xb000 | 0x40),
    ] {
        assert!(sent.contains(&want), "{want:x?} not in {sent:x?}");
    }
    // A mixer takes no selection: it sums.
    assert!(!sent.contains(&(0x0c, VERB_SET_CONN_SELECT, 0)));
    let line = paths[0];
    c.verbs.sent.clear();
    c.route(1, &line).unwrap();
    assert!(c.verbs.sent.contains(&(0x14, VERB_SET_EAPD, 2)));
    assert!(c
        .verbs
        .sent
        .contains(&(0x14, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN)));
}

#[test]
fn a_connection_range_is_spelled_out() {
    let mut f = qemu_output();
    // Short form: 0x02, then a range to 0x05.
    f.params.insert((7, PAR_CONN_LIST_LEN), 2);
    f.answers
        .insert((7, VERB_GET_CONN_LIST), (0x80 | 0x05) << 8 | 0x02);
    let mut c = codec(f);
    assert_eq!(c.connections(7), [2, 3, 4, 5]);
    // Long form.
    c.verbs.params.insert((7, PAR_CONN_LIST_LEN), 0x80 | 2);
    c.verbs
        .answers
        .insert((7, VERB_GET_CONN_LIST), (0x8000 | 0x13) << 16 | 0x10);
    assert_eq!(c.connections(7), [0x10, 0x11, 0x12, 0x13]);
}

#[test]
fn a_pin_that_cannot_sense_is_not_asked() {
    let mut c = codec(qemu_output());
    c.verbs.answers.insert((3, VERB_GET_PIN_SENSE), 1 << 31);
    let p = c.paths(1)[0];
    assert!(!p.senses);
    c.verbs.sent.clear();
    assert!(!c.present(&p));
    assert!(c.verbs.sent.is_empty());
}

#[test]
fn a_pin_that_needs_it_is_triggered_before_it_is_sensed() {
    let mut c = codec(realtek());
    c.verbs.params.insert(
        (0x14, PAR_PIN_CAP),
        PINCAP_OUT | PINCAP_PRESENCE | PINCAP_TRIGGER,
    );
    c.verbs.answers.insert((0x14, VERB_GET_PIN_SENSE), 1 << 31);
    let line = c.paths(1)[0];
    c.verbs.sent.clear();
    assert!(c.present(&line));
    assert_eq!(
        c.verbs.sent,
        [(0x14, VERB_SET_PIN_SENSE, 0), (0x14, VERB_GET_PIN_SENSE, 0)]
    );
}

#[test]
fn a_pin_whose_firmware_overrides_jack_detect_does_not_sense() {
    let mut c = codec(realtek());
    c.verbs
        .answers
        .insert((0x14, VERB_GET_CONFIG_DEFAULT), 0x0101_4110);
    assert!(!c.paths(1)[0].senses);
}

#[test]
fn a_codec_with_blank_pin_configs_still_offers_its_outputs() {
    let mut f = realtek();
    for pin in [0x14, 0x18, 0x1b, 0x1e] {
        // Device "other" (0xf), a jack.
        f.answers
            .insert((pin, VERB_GET_CONFIG_DEFAULT), 0x00f0_0000);
    }
    assert_eq!(codec(f).paths(1).len(), 4);
}

/// Two codecs on one link: one that answers nothing (an HDMI codec of a
/// disabled iGPU) and a Realtek. The fake answers by codec address.
struct Link {
    codecs: BTreeMap<u32, Fake>,
    sent: usize,
}

impl Verbs for Link {
    fn send(&mut self, word: u32) -> Option<u32> {
        self.sent += 1;
        self.codecs.get_mut(&(word >> 28))?.send(word)
    }
}

#[test]
fn every_codec_is_surveyed_and_a_silent_one_passed_over() {
    let mut link = Link {
        codecs: BTreeMap::new(),
        sent: 0,
    };
    link.codecs.insert(2, realtek());
    let mut c = Codec {
        verbs: link,
        cad: 0,
    };
    // Codec 0 does not answer; codec 2 is the Realtek.
    let paths = survey(&mut c, 1 << 0 | 1 << 2);
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|(p, _)| p.cad == 2 && p.afg == 1));
    assert_eq!(choose(&paths).unwrap().pin, 0x14);
}

/// Registers that remember what was written and read it back.
#[derive(Default)]
struct Mem(std::cell::RefCell<BTreeMap<usize, u32>>);

impl Regs for Mem {
    fn r8(&self, off: usize) -> u8 {
        *self.0.borrow().get(&off).unwrap_or(&0) as u8
    }
    fn r16(&self, off: usize) -> u16 {
        *self.0.borrow().get(&off).unwrap_or(&0) as u16
    }
    fn r32(&self, off: usize) -> u32 {
        *self.0.borrow().get(&off).unwrap_or(&0)
    }
    fn w8(&self, off: usize, v: u8) {
        self.0.borrow_mut().insert(off, v as u32);
    }
    fn w16(&self, off: usize, v: u16) {
        self.0.borrow_mut().insert(off, v as u32);
    }
    fn w32(&self, off: usize, v: u32) {
        self.0.borrow_mut().insert(off, v);
    }
}

#[test]
fn a_codec_that_stops_answering_is_given_up_on_after_a_few_verbs() {
    let mut words = vec![0u32; 2 * 1024];
    let mut rings = Rings {
        regs: Mem::default(),
        corb: words.as_mut_ptr(),
        rirb: words[1024..].as_mut_ptr(),
        corb_entries: 256,
        rirb_entries: 256,
        rirb_rp: 0,
        missed: [0; 16],
        timeouts: 0,
    };
    // Nothing ever lands in the RIRB: each of the first DEAD_AFTER verbs
    // waits VERB_NS out, the rest fail at once.
    let start = now_ns();
    for _ in 0..10 {
        assert_eq!(rings.send(2 << 28 | 0xf0000), None);
    }
    let spent = now_ns() - start;
    assert_eq!(rings.timeouts, DEAD_AFTER);
    assert!(spent < (DEAD_AFTER as u64 + 1) * VERB_NS, "{spent} ns");
    // Another codec is not tarred with it.
    assert_eq!(rings.missed[0], 0);
}
