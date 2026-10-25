//! The machine table, which is the only thing that says what a machine is.
//!
//! `MachineConfig::select` is called once per build, by
//! `BuildConfig::from_args`, and everything it returns decides what gets
//! compiled: the architecture picks the target spec, `pci_support` turns the
//! whole PCI bus on or off, `user_img` decides whether a rootfs image is
//! linked into the kernel, and `features` is handed straight to cargo. All of
//! it is read out of one TOML file by serde, with two keys renamed by hand --
//! and a rename that stops matching does not fail to compile. It returns
//! `None` instead, or a default, and the build goes on quietly building
//! something else.
//!
//! None of it had a test.

use super::*;

/// The file `select` reads, parsed again here as plain TOML rather than
/// through the typed struct, so the two readings can be compared.
fn raw_table() -> toml::Value {
    let file = Path::new(std::env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("config")
        .join("machine-features.toml");
    let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}

/// Every (manufacturer, product) pair the file declares, read straight from
/// the TOML.
fn declared() -> Vec<(String, String, toml::Value)> {
    let mut out = Vec::new();
    for (manufacturer, products) in raw_table().as_table().expect("a table of manufacturers") {
        for (name, body) in products.as_table().expect("a table of products") {
            out.push((manufacturer.clone(), name.clone(), body.clone()));
        }
    }
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// Every machine written in the table can be selected by the name it is
/// written under. That name is what `cargo qemu`, `cargo image` and the CI
/// matrix pass, and the only thing a name that no longer selects produces is
/// "Unknown target machine".
#[test]
fn every_machine_in_the_table_selects_by_its_own_name() {
    let all = declared();
    assert!(!all.is_empty(), "the machine table came back empty");
    for (manufacturer, name, _) in &all {
        let got = MachineConfig::select(name)
            .unwrap_or_else(|| panic!("{name} is in the table and did not select"));
        assert_eq!(
            &got.manufacturer, manufacturer,
            "{name} came back under the wrong manufacturer"
        );
    }
}

/// And no two manufacturers may use the same product name. `select` walks a
/// `HashMap` of manufacturers, so if a name appeared twice, which one answered
/// would depend on the hash seed -- the same `cargo qemu` would build for a
/// different board on a different day.
#[test]
fn a_product_name_belongs_to_one_manufacturer_only() {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let all = declared();
    for (manufacturer, name, _) in &all {
        if let Some((other, _)) = seen.iter().find(|(_, n)| *n == name.as_str()) {
            panic!("{name} is declared by both {other} and {manufacturer}");
        }
        seen.push((manufacturer, name));
    }
}

/// The architecture is taken through, spelled the way `Arch::from_str` spells
/// it. `from_args` panics with "Unknown arch" on anything else, which is the
/// only check the build has.
#[test]
fn the_architecture_is_one_of_the_three_the_build_knows() {
    for (_, name, _) in declared() {
        let got = MachineConfig::select(&name).expect("a declared machine");
        assert!(
            ["riscv64", "x86_64", "aarch64"].contains(&got.arch.as_str()),
            "{name} is built for {:?}, which is not an architecture the build knows",
            got.arch
        );
    }
}

/// `pci-support` is renamed by hand onto `pci_support`, and its absence means
/// yes. Getting that default backwards turns on the `no-pci` feature for every
/// QEMU machine, none of which says anything about PCI.
#[test]
fn a_machine_that_says_nothing_about_pci_has_pci() {
    for (_, name, body) in declared() {
        let got = MachineConfig::select(&name).expect("a declared machine");
        let written = body.get("pci-support").and_then(toml::Value::as_bool);
        match written {
            Some(declared) => assert_eq!(
                got.pci_support, declared,
                "{name} declares pci-support = {declared} and came back {}",
                got.pci_support
            ),
            None => assert!(
                got.pci_support,
                "{name} says nothing about PCI and came back without it"
            ),
        }
    }
}

/// `link-user-img` is the other hand-written rename, and the one with no
/// default to fall back on: if it stops matching, every board silently builds
/// without its rootfs linked in, and the kernel comes up with nothing to run.
#[test]
fn the_user_image_is_read_through_the_renamed_key() {
    let mut with_image = 0;
    for (_, name, body) in declared() {
        let got = MachineConfig::select(&name).expect("a declared machine");
        let written = body.get("link-user-img").and_then(toml::Value::as_str);
        match written {
            Some(path) => {
                with_image += 1;
                assert_eq!(
                    got.user_img.as_deref(),
                    Some(Path::new(path)),
                    "{name} declares link-user-img = {path:?} and came back {:?}",
                    got.user_img
                );
            }
            None => assert_eq!(
                got.user_img, None,
                "{name} declares no image and came back with one"
            ),
        }
    }
    assert!(
        with_image > 0,
        "no machine in the table links an image, so the rename is not being exercised"
    );
}

/// The features list is handed to cargo verbatim, in the order written. An
/// absent list is empty rather than a failure to parse.
#[test]
fn the_features_are_taken_through_in_the_order_they_are_written() {
    for (_, name, body) in declared() {
        let got = MachineConfig::select(&name).expect("a declared machine");
        let written: Vec<String> = body
            .get("features")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().expect("a feature name").to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            got.features, written,
            "{name} came back with other features"
        );
    }
}

/// The three machines the whole CI matrix is built for are in the table, each
/// for its own architecture. These three names are written out rather than
/// taken from the file: they are a public interface -- `BuildConfig` builds
/// `virt-{arch}` by name in `cargo qemu` and in `cargo check-style`, and the
/// workflows name them too.
#[test]
fn the_three_virt_machines_are_there_one_per_architecture() {
    for arch in ["riscv64", "x86_64", "aarch64"] {
        let name = format!("virt-{arch}");
        let got = MachineConfig::select(&name)
            .unwrap_or_else(|| panic!("{name} is what cargo qemu asks for and it did not select"));
        assert_eq!(got.arch, arch);
        assert_eq!(got.manufacturer, "qemu");
        assert_eq!(
            got.user_img, None,
            "{name} links an image, so building it would build a rootfs"
        );
        assert!(got.pci_support, "{name} came back without PCI");
    }
}

/// A machine nobody declared is refused, and refused by returning `None`
/// rather than by panicking inside the parser. `from_args` turns that into
/// "Unknown target machine", which is the message a typo on the command line
/// has to produce.
#[test]
fn a_machine_nobody_declared_is_refused_without_panicking() {
    for name in ["", "virt", "virt-riscv", "VIRT-RISCV64", "nezh", "qemu"] {
        assert!(
            MachineConfig::select(name).is_none(),
            "{name:?} selected something"
        );
    }
}

/// The name is matched whole and case-sensitively. A machine name reaches here
/// from the shell, where nothing lowercases it, so `select` must not be the
/// place that decides it was close enough.
#[test]
fn the_name_is_matched_whole_and_not_by_prefix() {
    assert!(MachineConfig::select("virt-riscv64").is_some());
    assert!(MachineConfig::select("virt-riscv64 ").is_none());
    assert!(MachineConfig::select(" virt-riscv64").is_none());
    assert!(MachineConfig::select("virt-riscv64-extra").is_none());
    assert!(MachineConfig::select("Virt-Riscv64").is_none());
}

/// Selecting takes the name by anything that looks like a string, because the
/// caller has a `String` from clap and the tests have literals. It is the same
/// answer either way.
#[test]
fn a_name_selects_the_same_whether_it_arrives_owned_or_borrowed() {
    let owned = String::from("nezha");
    let by_value = MachineConfig::select(owned.clone()).expect("nezha");
    let by_ref = MachineConfig::select(&owned).expect("nezha");
    let by_str = MachineConfig::select("nezha").expect("nezha");
    for got in [&by_value, &by_ref, &by_str] {
        assert_eq!(got.arch, "riscv64");
        assert_eq!(got.manufacturer, "allwinner");
        assert!(!got.pci_support);
        assert_eq!(
            got.user_img.as_deref(),
            Some(Path::new("zCore/riscv64.img"))
        );
    }
}
