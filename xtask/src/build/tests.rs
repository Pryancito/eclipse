//! What a machine name turns into before anything is compiled.
//!
//! `BuildConfig::from_args` is the whole of the translation from one string on
//! the command line to the feature set, the environment and the target spec
//! cargo is then invoked with. Every one of its decisions is silent: an extra
//! feature builds a driver that should not be there, a missing one leaves out
//! the personality, and a wrong target directory strips an object file that
//! the last build left behind. None of it had a test.
//!
//! Only machines with no `link-user-img` are built here. `from_args` builds
//! the rootfs image for the others, which needs the network and a target
//! toolchain, so the four boards in the table are out of reach from a unit
//! test -- that is said again where it matters below.

use super::*;

/// The machines that can be put through `from_args` without it going off and
/// building a rootfs image: the ones the table gives no `link-user-img`.
const PURE: [(&str, Arch); 3] = [
    ("virt-riscv64", Arch::Riscv64),
    ("virt-x86_64", Arch::X86_64),
    ("virt-aarch64", Arch::Aarch64),
];

fn config_for(machine: &str, debug: bool) -> BuildConfig {
    BuildConfig::from_args(BuildArgs {
        machine: machine.into(),
        debug,
    })
}

/// The architecture comes from the table, not from the machine name. They
/// agree for `virt-*` by construction, but it is the table that decides, and
/// it is the architecture that picks the target spec JSON and the target
/// directory.
#[test]
fn the_architecture_comes_from_the_table_and_not_from_the_machine_name() {
    for (machine, arch) in PURE {
        let built = config_for(machine, false);
        assert_eq!(
            built.arch.name(),
            arch.name(),
            "{machine} was built for the wrong architecture"
        );
    }
}

/// Every machine that does not ask for zircon gets the linux personality, and
/// none of them asks. That default is what makes `cargo qemu` and
/// `cargo check-style` build a Linux kernel without saying so.
#[test]
fn a_machine_that_does_not_ask_for_zircon_is_built_with_linux() {
    for (machine, _) in PURE {
        let built = config_for(machine, false);
        assert!(
            built.features.contains("linux"),
            "{machine} was built without the linux personality: {:?}",
            built.features
        );
        assert!(
            !built.features.contains("zircon"),
            "{machine} asked for zircon: {:?}",
            built.features
        );
    }
}

/// The features the table writes down are all there, and nothing else is
/// added behind them. `graphic` is the one the three QEMU machines declare,
/// and dropping it builds a kernel with no display at all.
#[test]
fn the_features_from_the_table_are_kept_and_nothing_extra_is_invented() {
    for (machine, _) in PURE {
        let declared = MachineConfig::select(machine).expect("a declared machine");
        let built = config_for(machine, false);
        for feature in &declared.features {
            assert!(
                built.features.contains(feature),
                "{machine} lost {feature}: {:?}",
                built.features
            );
        }
        let mut extra: Vec<&str> = built
            .features
            .iter()
            .map(String::as_str)
            .filter(|f| !declared.features.iter().any(|d| d == f))
            .collect();
        extra.sort_unstable();
        assert_eq!(
            extra,
            ["linux"],
            "{machine} was built with features the table never asked for"
        );
    }
}

/// A machine that supports PCI is not given `no-pci`, and one that does not
/// support it is. The three QEMU machines say nothing about PCI, so they take
/// the default, which is that they have it.
#[test]
fn only_a_machine_without_pci_is_built_with_no_pci() {
    for (machine, _) in PURE {
        let declared = MachineConfig::select(machine).expect("a declared machine");
        assert!(
            declared.pci_support,
            "{machine} no longer supports PCI, so this test is checking the wrong half"
        );
        let built = config_for(machine, false);
        assert!(
            !built.features.contains("no-pci"),
            "{machine} supports PCI and was built with no-pci: {:?}",
            built.features
        );
    }
    // The other half -- a machine with `pci-support = false` getting `no-pci`
    // -- cannot be built here: every such machine in the table also declares
    // `link-user-img`, and `from_args` builds the rootfs image for those. What
    // can be checked is that such machines exist, so the branch is not dead.
    let without_pci: Vec<&str> = ["nezha", "cr1825", "visionfive"]
        .into_iter()
        .filter(|m| {
            MachineConfig::select(m)
                .map(|c| !c.pci_support)
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        without_pci.len(),
        3,
        "the boards that have no PCI are {without_pci:?}"
    );
}

/// None of these machines links a rootfs image, so none of them gets the
/// feature or the environment variable that goes with one. This is also what
/// keeps the test from building an image: a machine that grew a
/// `link-user-img` would make `from_args` go and build one.
#[test]
fn a_machine_with_no_image_gets_no_image_feature_and_no_environment() {
    for (machine, _) in PURE {
        let built = config_for(machine, false);
        assert!(
            !built.features.contains("link-user-img"),
            "{machine} was built with link-user-img: {:?}",
            built.features
        );
        assert!(
            built.env.is_empty(),
            "{machine} was built with environment set: {:?}",
            built.env
        );
    }
}

/// Release is the default and debug is asked for. The flag only moves the
/// directory the object file is read back out of, so getting it wrong strips
/// and disassembles whatever the other kind of build left there last.
#[test]
fn the_object_file_is_read_from_the_directory_the_build_wrote_it_to() {
    for (machine, arch) in PURE {
        let release = config_for(machine, false);
        let debug = config_for(machine, true);
        assert!(!release.debug, "{machine} defaulted to debug");
        assert!(debug.debug, "{machine} ignored --debug");

        let r = release.target_file_path();
        let d = debug.target_file_path();
        assert_ne!(r, d, "{machine} reads both kinds of build from one place");
        assert!(
            r.ends_with(format!("target/{}/release/zcore", arch.name())),
            "{}",
            r.display()
        );
        assert!(
            d.ends_with(format!("target/{}/debug/zcore", arch.name())),
            "{}",
            d.display()
        );
        assert!(r.is_absolute(), "{}", r.display());
        assert!(r.starts_with(*PROJECT_DIR), "{}", r.display());
    }
}

/// Two architectures do not share a target directory. They are built with
/// different target specs into the same `target/`, so a shared path would hand
/// one architecture's kernel to the other's objcopy.
#[test]
fn two_architectures_do_not_read_the_same_object_file() {
    let mut seen: Vec<PathBuf> = Vec::new();
    for (machine, _) in PURE {
        let path = config_for(machine, false).target_file_path();
        assert!(
            !seen.contains(&path),
            "{machine} reads the same object file as another architecture: {}",
            path.display()
        );
        seen.push(path);
    }
}

/// The target spec is named after the architecture and lives in `zCore/`, and
/// the file is really there. `cargo -Z json-target-spec` fails late and
/// unhelpfully on a path that is not, and the name comes from `Arch::name`,
/// which nothing else ties to these files.
#[test]
fn every_architecture_has_the_target_spec_the_build_names() {
    for (machine, arch) in PURE {
        let built = config_for(machine, false);
        let spec = INNER.join(format!("{}.json", built.arch.name()));
        assert!(
            spec.is_file(),
            "{machine} is built against {}, which is not there",
            spec.display()
        );
        assert!(
            spec.ends_with(format!("zCore/{}.json", arch.name())),
            "{}",
            spec.display()
        );
    }
}

/// `asm` and `bin` are built for a machine, not for an architecture. They
/// were once built for an architecture and the flag was renamed; this is the
/// shape the help text has to show, and it is the one place the two can be
/// told apart, since the arguments are flattened into the subcommand and are
/// private to this module.
#[test]
fn asm_and_bin_are_built_for_a_machine_and_not_for_an_architecture() {
    use crate::{Cli, Commands};
    use clap::Parser;
    for sub in ["asm", "bin"] {
        let parsed = Cli::try_parse_from(["xtask", sub, "--machine", "virt-riscv64"])
            .unwrap_or_else(|e| panic!("{sub}: {e}"));
        let args = match parsed.command {
            Commands::Asm(a) | Commands::Bin(a) => a,
            _ => panic!("{sub} parsed as another subcommand"),
        };
        assert_eq!(args.build.machine, "virt-riscv64");
        assert!(!args.build.debug, "{sub} defaulted to a debug build");
        assert_eq!(args.output, None, "{sub} invented an output path");

        assert!(
            Cli::try_parse_from(["xtask", sub, "--arch", "riscv64"]).is_err(),
            "{sub} takes an architecture, so the help text showing --arch is right"
        );
        assert!(
            Cli::try_parse_from(["xtask", sub]).is_err(),
            "{sub} built something without being told what for"
        );
    }
    // And `-m` is the short form, because that is what the flattened
    // `BuildArgs` declares and nothing else in these two subcommands claims
    // the letter.
    let parsed =
        Cli::try_parse_from(["xtask", "asm", "-m", "virt-aarch64", "-o", "a.asm"]).expect("asm");
    match parsed.command {
        Commands::Asm(a) => {
            assert_eq!(a.build.machine, "virt-aarch64");
            assert_eq!(a.output, Some(PathBuf::from("a.asm")));
        }
        _ => panic!("parsed as another subcommand"),
    }
}

/// `qemu` is built for an architecture, and its three optional flags are all
/// absent by default. `--smp` absent is not one hart and `--gdb` absent is
/// not port zero: both are read as `Option` and only added to the QEMU command
/// line when they are there.
#[test]
fn qemu_takes_an_architecture_and_leaves_smp_and_gdb_unset() {
    use crate::{Cli, Commands};
    use clap::Parser;
    let bare = Cli::try_parse_from(["xtask", "qemu", "--arch", "riscv64"]).expect("qemu");
    match bare.command {
        Commands::Qemu(a) => {
            assert_eq!(a.arch.arch.name(), "riscv64");
            assert!(!a.debug);
            assert_eq!(a.smp, None, "an unasked-for hart count must stay unasked");
            assert_eq!(a.gdb, None, "qemu must not wait for gdb unless asked");
        }
        _ => panic!("parsed as another subcommand"),
    }
    let full = Cli::try_parse_from([
        "xtask", "qemu", "--arch", "aarch64", "--smp", "4", "--gdb", "1234", "--debug",
    ])
    .expect("qemu");
    match full.command {
        Commands::Qemu(a) => {
            assert_eq!(a.arch.arch.name(), "aarch64");
            assert!(a.debug);
            assert_eq!(a.smp, Some(4));
            assert_eq!(a.gdb, Some(1234));
        }
        _ => panic!("parsed as another subcommand"),
    }
    assert!(
        Cli::try_parse_from(["xtask", "qemu", "--arch", "riscv64", "--smp", "256"]).is_err(),
        "the hart count is a u8, so 256 is not one"
    );
}

/// `gdb` requires the port, because there is nothing sensible to connect to
/// without it, and takes the architecture that picks which gdb to run.
#[test]
fn gdb_requires_the_port_it_is_to_connect_to() {
    use crate::{Cli, Commands};
    use clap::Parser;
    assert!(
        Cli::try_parse_from(["xtask", "gdb", "--arch", "riscv64"]).is_err(),
        "gdb ran without a port to connect to"
    );
    let parsed =
        Cli::try_parse_from(["xtask", "gdb", "--arch", "riscv64", "--port", "1234"]).expect("gdb");
    match parsed.command {
        Commands::Gdb(a) => {
            assert_eq!(a.port, 1234);
            assert_eq!(a.arch.arch.name(), "riscv64");
        }
        _ => panic!("parsed as another subcommand"),
    }
}

/// A machine nobody declared stops the build, with the message that names the
/// thing the user got wrong.
#[test]
fn an_unknown_machine_stops_the_build_and_says_which_one() {
    let caught = std::panic::catch_unwind(|| {
        config_for("virt-mips64", false);
    });
    let payload = caught.expect_err("an unknown machine has to stop the build");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("(the panic carried no text)");
    assert!(
        message.contains("Unknown target machine"),
        "the refusal reads {message:?}"
    );
}
