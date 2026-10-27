//! The ELD the HDMI/DP audio codec reports, built from a base EDID. Own
//! file for the size ceiling of `nvidia.rs`.

/// Build a 96-byte ELD from a 128-byte base EDID (nvkms FillELDBuffer layout).
/// CEA SADs live in extension block 1, which the bootloader does not keep;
/// a basic-audio 2ch LPCM SAD is used so the sink still plays 48 kHz stereo.
pub(super) fn build_eld_from_base_edid(edid: &[u8], display_id: u32, is_dp: bool) -> [u8; 96] {
    let mut eld = [0u8; 96];
    if edid.len() < 128 {
        return eld;
    }
    let mut mnl = 0u32;
    const DESC: [usize; 4] = [54, 72, 90, 108];
    for off in DESC {
        if edid[off] == 0 && edid[off + 1] == 0 && edid[off + 2] == 0 && edid[off + 3] == 0xFC {
            // The name descriptor is a fixed 13 bytes, and a name shorter than
            // that is terminated by 0x0A and padded with spaces. `MNL` is the
            // name's own length, so it stops at the terminator the way Linux's
            // `get_monitor_name` does: copying all 13 and calling it 13 puts
            // the newline and the padding inside the string the HDA codec
            // reports, which is what userspace reads back out of
            // `/proc/asound/card*/eld#*`.
            let name = &edid[off + 5..off + 18];
            let len = name.iter().position(|&c| c == 0x0A).unwrap_or(name.len());
            eld[20..20 + len].copy_from_slice(&name[..len]);
            mnl = len as u32;
            break;
        }
    }
    // LPCM, 2ch, 32/44.1/48 kHz, 16/20/24-bit (HDMI "basic audio").
    let sad = [0x09u8, 0x07, 0x07];
    let sad_count = 1u32;
    let spk_alloc = 0x01u8;
    eld[0] = 2 << 3;
    eld[4] = mnl as u8;
    eld[5] = ((sad_count << 4) | if is_dp { 1 << 2 } else { 0 }) as u8;
    eld[7] = spk_alloc;
    eld[8] = display_id as u8;
    eld[9] = (display_id >> 8) as u8;
    eld[10] = (display_id >> 16) as u8;
    eld[11] = (display_id >> 24) as u8;
    eld[16] = edid[8];
    eld[17] = edid[9];
    eld[18] = edid[10];
    eld[19] = edid[11];
    let sad_off = 20 + mnl as usize;
    eld[sad_off..sad_off + 3].copy_from_slice(&sad);
    eld[2] = (16 + mnl + sad_count * 3).div_ceil(4) as u8;
    eld
}
