use super::build_eld_from_base_edid;

/// A valid-enough EDID for the ELD builder: the header, the manufacturer
/// and product bytes it copies verbatim, and whatever `f` puts in the
/// descriptors. The builder itself only range-checks the length -- its one
/// caller hands it a block `edid::block_valid` already accepted -- so the
/// checksum does not matter here, and leaving it out keeps each test's
/// intent in view.
fn edid(f: impl FnOnce(&mut [u8; 128])) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // "DEL" 0x2412 -- a Dell U2412M's manufacturer id and product code.
    b[8] = 0x10;
    b[9] = 0xAC;
    b[10] = 0x12;
    b[11] = 0x24;
    f(&mut b);
    b
}

/// Put a monitor-name descriptor holding `name` in descriptor slot `slot`,
/// padded exactly the way a monitor does it: 0x0A to end the string, then
/// spaces to fill the 13 bytes.
fn name_descriptor(b: &mut [u8; 128], slot: usize, name: &str) {
    let off = 54 + slot * 18;
    b[off] = 0;
    b[off + 1] = 0;
    b[off + 2] = 0;
    b[off + 3] = 0xFC;
    b[off + 4] = 0;
    let text = &mut b[off + 5..off + 18];
    text.fill(b' ');
    let bytes = name.as_bytes();
    let n = bytes.len().min(13);
    text[..n].copy_from_slice(&bytes[..n]);
    if n < 13 {
        text[n] = 0x0A;
    }
}

fn mnl(eld: &[u8; 96]) -> usize {
    (eld[4] & 0x1F) as usize
}

fn monitor_name(eld: &[u8; 96]) -> &[u8] {
    &eld[20..20 + mnl(eld)]
}

fn sad_count(eld: &[u8; 96]) -> usize {
    (eld[5] >> 4) as usize
}

#[test]
fn the_monitor_name_stops_at_its_terminator() {
    // A name shorter than the field ends at 0x0A and is padded with
    // spaces. Reporting all 13 bytes puts both inside the string every
    // client reads back, and inflates `MNL` past the name.
    let e = edid(|b| name_descriptor(b, 0, "DELL U2412M"));
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(mnl(&eld), 11);
    assert_eq!(monitor_name(&eld), b"DELL U2412M");
}

#[test]
fn a_name_that_fills_the_field_keeps_all_thirteen_bytes() {
    // 13 characters leave no room for a terminator, so nothing is trimmed.
    let e = edid(|b| name_descriptor(b, 0, "ACME MONITOR1"));
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(mnl(&eld), 13);
    assert_eq!(monitor_name(&eld), b"ACME MONITOR1");
}

#[test]
fn the_name_is_found_in_a_later_descriptor_too() {
    // Slot 0 is the preferred timing on every real monitor, so the name is
    // never in it; a builder that only looked at the first descriptor would
    // report no name at all.
    let e = edid(|b| {
        b[54] = 0x01; // slot 0: a detailed timing
        name_descriptor(b, 2, "VX2758");
    });
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(monitor_name(&eld), b"VX2758");
}

#[test]
fn a_monitor_that_states_no_name_gets_an_empty_one_not_junk() {
    let e = edid(|b| b[54] = 0x01);
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(mnl(&eld), 0);
    // With no name the SADs start right at byte 20, so nothing of the
    // descriptor may have leaked into the name field.
    assert_eq!(&eld[20..23], &[0x09, 0x07, 0x07]);
}

#[test]
fn the_baseline_length_covers_the_name_and_the_descriptors_and_no_more() {
    // `Baseline_Eld_Len` is in DWORDs and does not count the four header
    // bytes, so the block runs from byte 4 to `4 + 4 * len`. A codec reads
    // exactly that much: too small truncates the last SAD, too large hands
    // it zero padding as if it were another descriptor.
    for name in ["", "AB", "DELL U2412M", "ACME MONITOR1"] {
        let e = edid(|b| {
            if !name.is_empty() {
                name_descriptor(b, 1, name);
            }
        });
        let eld = build_eld_from_base_edid(&e, 1, false);
        let used = 20 + mnl(&eld) + sad_count(&eld) * 3;
        let declared = 4 + 4 * eld[2] as usize;
        assert!(
            declared >= used && declared - used < 4,
            "name {:?}: declared {} bytes, {} used",
            name,
            declared,
            used
        );
    }
}

#[test]
fn the_manufacturer_and_product_bytes_are_passed_through_verbatim() {
    // These four are how userspace matches a sink against a quirk table,
    // so they are the EDID's own bytes, not anything decoded.
    let e = edid(|b| name_descriptor(b, 0, "DELL U2412M"));
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(&eld[16..20], &e[8..12]);
}

#[test]
fn displayport_and_hdmi_are_told_apart_in_the_connector_type() {
    // `Conn_Type` in bits 3:2 of byte 5: 0 is HDMI, 1 is DisplayPort. The
    // codec unmutes a different way for each, so getting it wrong is a
    // silent monitor.
    let e = edid(|b| name_descriptor(b, 0, "SINK"));
    let hdmi = build_eld_from_base_edid(&e, 1, false);
    let dp = build_eld_from_base_edid(&e, 1, true);
    assert_eq!((hdmi[5] >> 2) & 0x3, 0);
    assert_eq!((dp[5] >> 2) & 0x3, 1);
    // And nothing else about the sink changes with the protocol.
    assert_eq!(hdmi[4], dp[4]);
    assert_eq!(&hdmi[16..20], &dp[16..20]);
}

#[test]
fn the_port_id_carries_the_sor_mask_little_endian() {
    // The caller passes `1 << sor`, and the HDA driver matches its pin
    // against this, so the four bytes have to come back in the same order.
    let e = edid(|b| b[54] = 0x01);
    for sor in 0..8u32 {
        let eld = build_eld_from_base_edid(&e, 1 << sor, false);
        assert_eq!(&eld[8..12], &(1u32 << sor).to_le_bytes());
    }
}

#[test]
fn the_declared_descriptor_count_matches_the_descriptors_written() {
    // Basic audio: one LPCM descriptor, 2 channels, 32/44.1/48 kHz,
    // 16/20/24-bit. Every HDMI sink is required to support it, which is why
    // it is stated unconditionally with no CEA extension block to read.
    let e = edid(|b| name_descriptor(b, 0, "SINK"));
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(sad_count(&eld), 1);
    let at = 20 + mnl(&eld);
    assert_eq!(&eld[at..at + 3], &[0x09, 0x07, 0x07]);
    // Front left + front right.
    assert_eq!(eld[7], 0x01);
}

#[test]
fn the_version_is_the_one_the_codec_expects() {
    let e = edid(|b| b[54] = 0x01);
    let eld = build_eld_from_base_edid(&e, 1, false);
    assert_eq!(eld[0] >> 3, 2, "ELD_Ver must be CEA-861D baseline");
}

#[test]
fn a_block_shorter_than_an_edid_produces_nothing_rather_than_reading_past_it() {
    // The caller checks the length too, but this runs off the end of the
    // buffer if the guard is not here: the descriptors alone reach byte 126.
    let eld = build_eld_from_base_edid(&[0u8; 64], 1, false);
    assert_eq!(eld, [0u8; 96]);
}
