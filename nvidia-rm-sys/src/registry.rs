//! The RM's registry: the DWORD keys the user sets, which the RM reads with
//! `osReadRegistryDword` and GSP-RM gets in its SET_REGISTRY RPC.
//!
//! On Linux they are `NVreg_RegistryDwords`, a module parameter of
//! nvidia.ko: `"Key=Value;Key2=Value2"`, read by `rm_parse_option_string`
//! into the registry `RmReadRegistryDword` looks keys up in, and packed for
//! GSP-RM by `RmPackageRegistry`. Here the same string comes from the boot
//! line (`nvidia.registry=`, see `docs/README-nvidia-cmdline.md`), parsed the
//! same way: spaces dropped, `;` between entries, the first `=` of an entry
//! between its name and its value, a value in C's base (`0x` hex, a leading
//! `0` octal, else decimal), and an entry with a second `=`, an empty name
//! or a value that is not a number left out. A key set twice keeps the last
//! value, as `rm_write_registry_dword` overwrites it.
//!
//! Empty unless the user sets it, and then the RM behaves as it does with no
//! registry at all, as before.

use alloc::string::String;
use alloc::vec::Vec;
use lock::Mutex;

/// The keys set, in the order first written.
static KEYS: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

/// `text`, an `NVreg_RegistryDwords` string, as (name, value) pairs.
pub fn parse(text: &str) -> Vec<(String, u32)> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut keys: Vec<(String, u32)> = Vec::new();
    for entry in text.split(';') {
        let mut parts = entry.split('=');
        let (Some(name), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(value) = strtoul(value) else {
            continue;
        };
        match keys.iter_mut().find(|(k, _)| k == name) {
            Some(key) => key.1 = value,
            None => keys.push((String::from(name), value)),
        }
    }
    keys
}

/// `os_strtoul(value, NULL, 0)` of a whole value: `None` for one that is
/// not all digits of its base, or does not fit.
fn strtoul(value: &str) -> Option<u32> {
    let (digits, radix) = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (hex, 16)
    } else if value.len() > 1 && value.starts_with('0') {
        (&value[1..], 8)
    } else {
        (value, 10)
    };
    if digits.is_empty() {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
}

/// Set the registry from `text` (`nvidia.registry=`), before the RM comes
/// up. The keys that were taken.
pub fn set(text: &str) -> Vec<(String, u32)> {
    let keys = parse(text);
    *KEYS.lock() = keys.clone();
    keys
}

/// The value of key `name`, if the user set it.
pub fn dword(name: &str) -> Option<u32> {
    KEYS.lock().iter().find(|(k, _)| k == name).map(|&(_, v)| v)
}

/// Every key, for GSP-RM's copy.
pub fn keys() -> Vec<(String, u32)> {
    KEYS.lock().clone()
}

/// How many keys there are (`osPackageRegistry`'s count).
#[no_mangle]
pub extern "C" fn eclipse_rm_registry_count() -> u32 {
    KEYS.lock().len() as u32
}

/// Key `index`: its name copied, NUL-terminated, into `name` (of `cap`
/// bytes) and its value into `value`. The name's length with its NUL, or 0
/// for no such key or a name that does not fit (asked with a null `name`,
/// the length it needs).
///
/// # Safety
/// `name` is null or `cap` writable bytes; `value` is null or writable.
#[no_mangle]
pub unsafe extern "C" fn eclipse_rm_registry_entry(
    index: u32,
    name: *mut u8,
    cap: u32,
    value: *mut u32,
) -> u32 {
    let keys = KEYS.lock();
    let Some((key, data)) = keys.get(index as usize) else {
        return 0;
    };
    let len = key.len() + 1;
    if !value.is_null() {
        *value = *data;
    }
    if name.is_null() {
        return len as u32;
    }
    if len > cap as usize {
        return 0;
    }
    core::ptr::copy_nonoverlapping(key.as_ptr(), name, key.len());
    *name.add(key.len()) = 0;
    len as u32
}

#[cfg(test)]
mod tests;
