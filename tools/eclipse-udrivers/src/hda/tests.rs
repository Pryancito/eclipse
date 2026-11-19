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
    f.params
        .insert((2, PAR_AUDIO_WIDGET_CAP), WIDGET_AUDIO_OUT << 20 | 1 << 2);
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
            conv: 2,
            pin: 3,
            conn: 0,
            digital: false,
            hdmi: false
        }]
    );
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
        conn: 0,
        digital: false,
        hdmi: false,
    };
    let hdmi = Path {
        conv: 4,
        pin: 5,
        digital: true,
        hdmi: true,
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
        conn: 0,
        digital: true,
        hdmi: true,
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
