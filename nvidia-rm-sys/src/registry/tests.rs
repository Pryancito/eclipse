use super::*;
use alloc::string::ToString;
use alloc::vec;

fn pairs(text: &str) -> Vec<(alloc::string::String, u32)> {
    parse(text)
}

fn kv(k: &str, v: u32) -> (alloc::string::String, u32) {
    (k.to_string(), v)
}

/// What a Linux user writes in `NVreg_RegistryDwords`, read the same way.
#[test]
fn entries_are_split_on_semicolons_and_read_in_c_bases() {
    assert_eq!(
        pairs("ForceP2P=0x11;RMPcieP2PType=1; PeerMappingOverride = 010"),
        vec![
            kv("ForceP2P", 0x11),
            kv("RMPcieP2PType", 1),
            kv("PeerMappingOverride", 8)
        ]
    );
}

/// `rm_parse_option_string` leaves out an entry with no name, no value or a
/// second `=`; here also one whose value is not a number.
#[test]
fn malformed_entries_are_left_out() {
    assert_eq!(
        pairs("=1;A=;B;C=1=2;D=zz;E=0x;F=99999999999;G=0"),
        vec![kv("G", 0)]
    );
    assert!(pairs("").is_empty());
}

/// A key written twice keeps its last value, in its first place.
#[test]
fn a_key_set_twice_keeps_the_last_value() {
    assert_eq!(pairs("A=1;B=2;A=3"), vec![kv("A", 3), kv("B", 2)]);
}

/// The registry the RM reads, and the copy GSP-RM gets.
#[test]
fn the_rm_reads_what_was_set() {
    set("ForceP2P=0x11;Other=7");
    assert_eq!(dword("ForceP2P"), Some(0x11));
    assert_eq!(dword("forcep2p"), None);
    assert_eq!(eclipse_rm_registry_count(), 2);
    let mut name = [0xffu8; 16];
    let mut value = 0;
    let len = unsafe { eclipse_rm_registry_entry(1, name.as_mut_ptr(), 16, &mut value) };
    assert_eq!((len, value), (6, 7));
    assert_eq!(&name[..6], b"Other\0");
    assert_eq!(
        unsafe { eclipse_rm_registry_entry(0, core::ptr::null_mut(), 0, core::ptr::null_mut()) },
        9
    );
    assert_eq!(
        unsafe { eclipse_rm_registry_entry(0, name.as_mut_ptr(), 8, &mut value) },
        0
    );
    assert_eq!(
        unsafe { eclipse_rm_registry_entry(2, name.as_mut_ptr(), 16, &mut value) },
        0
    );
    set("");
    assert_eq!(dword("ForceP2P"), None);
    assert_eq!(eclipse_rm_registry_count(), 0);
}
