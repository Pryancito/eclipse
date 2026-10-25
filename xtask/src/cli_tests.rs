//! The command line, and whether the commands the help text shows can be
//! typed.
//!
//! This file is the whole user interface of the build: nineteen subcommands,
//! each documented with a worked example in its own help text, and seventeen
//! cargo aliases in `.cargo/config.toml` that are the only reason
//! `cargo qemu` is shorter than `cargo xtask qemu`. Three things can come
//! apart without anything failing to compile: a subcommand can be renamed out
//! from under its alias, an alias can be deleted, and an example in the help
//! can go on showing a flag that was renamed years ago. All three produce the
//! same thing at the keyboard -- clap refusing a command the documentation
//! says to type -- and none of it had a test.

use super::*;

/// The alias file, read with the byte-order mark the file actually starts
/// with taken off.
fn alias_file() -> String {
    let path = PROJECT_DIR.join(".cargo").join("config.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.trim_start_matches('\u{feff}').to_string()
}

/// `cargo <alias>` to the xtask subcommand it runs, for every alias that runs
/// one. The file is tiny and xtask has no TOML parser of its own, so this
/// reads it a line at a time rather than growing a dependency for it.
fn aliases() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in alias_file().lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("not an alias: {line:?}"));
        let value = value.trim().trim_matches('"');
        // `xtask` itself is the one alias that is not a subcommand: it is what
        // every other alias expands through.
        if let Some(sub) = value.strip_prefix("xtask ") {
            out.push((name.trim().to_string(), sub.trim().to_string()));
        }
    }
    out
}

/// Every `cargo ...` line written in a help example, in the order they
/// appear. `dump` is the one subcommand that does not exist on every host --
/// it is compiled out on riscv64 -- and `include_str!` cannot see a `cfg`, so
/// its example is dropped where the subcommand is not there.
fn documented_commands() -> Vec<String> {
    include_str!("main.rs")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("/// "))
        .filter(|l| l.starts_with("cargo "))
        .filter(|l| cfg!(not(target_arch = "riscv64")) || !l.ends_with(" dump"))
        .map(str::to_string)
        .collect()
}

/// Turn a documented `cargo ...` line into the argv `xtask` itself would see,
/// resolving the alias. `None` means there is no way to type it: no alias and
/// not spelled `cargo xtask ...`.
fn as_argv(documented: &str) -> Option<Vec<String>> {
    let mut words = documented.split_whitespace();
    assert_eq!(words.next(), Some("cargo"), "{documented}");
    let first = words.next().unwrap_or_else(|| panic!("{documented}"));
    let mut argv = vec!["xtask".to_string()];
    if first == "xtask" {
        argv.push(
            words
                .next()
                .unwrap_or_else(|| panic!("no subcommand in {documented}"))
                .to_string(),
        );
    } else {
        let (_, sub) = aliases().into_iter().find(|(name, _)| name == first)?;
        argv.push(sub);
    }
    argv.extend(words.map(str::to_string));
    Some(argv)
}

/// Every alias in `.cargo/config.toml` names a subcommand that still exists.
/// An alias to a renamed subcommand is not a compile error anywhere: it is
/// `cargo rootfs` answering "unrecognized subcommand" to whoever reads the
/// README.
#[test]
fn every_cargo_alias_names_a_subcommand_that_exists() {
    let all = aliases();
    assert!(
        all.len() >= 17,
        "the alias file came back with only {} aliases",
        all.len()
    );
    for (alias, sub) in all {
        // `--help` is the one thing every subcommand accepts, so it reaches
        // the name without having to satisfy the required arguments too.
        let refused = Cli::try_parse_from(["xtask", &sub, "--help"]).err();
        let kind = refused.map(|e| e.kind());
        assert_eq!(
            kind,
            Some(clap::error::ErrorKind::DisplayHelp),
            "cargo {alias} runs `xtask {sub}`, which is not a subcommand"
        );
    }
}

/// And every command the help text tells the user to type can be typed:
/// either there is an alias for it, or the example spells it
/// `cargo xtask ...`. An example naming a `cargo <x>` with no alias is a
/// command that exists and that nobody can reach the short way.
#[test]
fn every_command_the_help_tells_you_to_type_can_be_typed() {
    let documented = documented_commands();
    assert!(
        documented.len() >= 19,
        "only {} examples were found in the help text",
        documented.len()
    );
    let unreachable: Vec<&String> = documented.iter().filter(|d| as_argv(d).is_none()).collect();
    assert!(
        unreachable.is_empty(),
        "the help shows commands that have no alias: {unreachable:?}"
    );
}

/// And every one of them parses, flags and all. This is the test that catches
/// an example still showing a flag the arguments no longer have: the help text
/// is the only documentation this tool has, and a wrong flag there costs the
/// reader a round trip through clap's error.
#[test]
fn every_example_in_the_help_actually_parses() {
    let mut broken = Vec::new();
    for documented in documented_commands() {
        let Some(argv) = as_argv(&documented) else {
            continue; // reported by its own test
        };
        if let Err(e) = Cli::try_parse_from(&argv) {
            broken.push(format!("{documented} -> {:?}", e.kind()));
        }
    }
    assert!(
        broken.is_empty(),
        "the help shows commands that do not parse: {broken:#?}"
    );
}

/// No two aliases run the same subcommand, and no subcommand has two aliases.
/// A duplicate is how a rename leaves the old name working and the new one
/// shadowed, so that nobody notices the rename happened.
#[test]
fn no_subcommand_has_two_aliases() {
    let all = aliases();
    for (i, (alias, sub)) in all.iter().enumerate() {
        for (other, other_sub) in &all[i + 1..] {
            assert_ne!(alias, other, "{alias} is aliased twice");
            assert_ne!(
                sub, other_sub,
                "{sub} is reached by both {alias} and {other}"
            );
        }
    }
}

/// A subcommand nobody declared is refused as an unknown subcommand, and a
/// bare `xtask` with nothing after it is refused for want of one. Both are
/// what a typo at the shell produces, and neither may be taken for a default.
#[test]
fn an_unknown_subcommand_is_refused_and_so_is_none_at_all() {
    use clap::error::ErrorKind;
    // `expect_err` would want `Cli: Debug`, which it has no other reason to
    // be, so each refusal is taken by `err()` instead.
    let why = |argv: &[&str]| -> ErrorKind {
        Cli::try_parse_from(argv)
            .err()
            .unwrap_or_else(|| panic!("{argv:?} parsed"))
            .kind()
    };
    assert_eq!(why(&["xtask", "rootfsx"]), ErrorKind::InvalidSubcommand);
    // A bare `xtask` shows the help rather than picking a subcommand of its
    // own: there is no default, and every one of them has a side effect.
    assert_eq!(
        why(&["xtask"]),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    assert_eq!(
        why(&["xtask", "Rootfs", "--arch", "riscv64"]),
        ErrorKind::InvalidSubcommand,
        "subcommands are lower case"
    );
}

/// `--arch` is required wherever it is taken, and the variant defaults to the
/// desktop one. That default is the whole reason every command written before
/// variants existed still means what it did: a command with no `--variant`
/// builds the desktop image, in the desktop rootfs directory.
#[test]
fn an_architecture_is_required_and_the_variant_defaults_to_the_desktop() {
    use clap::error::ErrorKind;
    for sub in [
        "rootfs",
        "musl-libs",
        "ffmpeg",
        "opencv",
        "libc-test",
        "other-test",
        "image",
        "verify-rootfs",
    ] {
        let e = Cli::try_parse_from(["xtask", sub])
            .err()
            .unwrap_or_else(|| panic!("{sub} parsed with no architecture"));
        assert_eq!(
            e.kind(),
            ErrorKind::MissingRequiredArgument,
            "{sub} does not require an architecture"
        );

        let parsed = Cli::try_parse_from(["xtask", sub, "--arch", "aarch64"])
            .unwrap_or_else(|e| panic!("{sub}: {e}"));
        let arch = match parsed.command {
            Commands::Rootfs(a)
            | Commands::MuslLibs(a)
            | Commands::Ffmpeg(a)
            | Commands::Opencv(a)
            | Commands::LibcTest(a)
            | Commands::OtherTest(a)
            | Commands::Image(a)
            | Commands::VerifyRootfs(a) => a,
            _ => panic!("{sub} parsed as another subcommand"),
        };
        assert_eq!(arch.arch.name(), "aarch64");
        assert_eq!(
            arch.variant,
            crate::variant::Variant::Desktop,
            "{sub} no longer defaults to the desktop variant"
        );
    }
}

/// A port is what `git-proxy` is set with, and leaving it out is what unsets
/// the proxy -- so an absent `--port` has to arrive as `None` rather than as
/// a zero. Port 0 is a real port number as far as clap is concerned.
#[test]
fn git_proxy_without_a_port_is_what_unsets_the_proxy() {
    let unset = Cli::try_parse_from(["xtask", "git-proxy", "--global"]).expect("git-proxy");
    match unset.command {
        Commands::GitProxy(ProxyPort { port, global }) => {
            assert_eq!(port, None, "a missing port must not arrive as a number");
            assert!(global);
        }
        _ => panic!("parsed as another subcommand"),
    }
    let set = Cli::try_parse_from(["xtask", "git-proxy", "--port", "12345"]).expect("git-proxy");
    match set.command {
        Commands::GitProxy(ProxyPort { port, global }) => {
            assert_eq!(port, Some(12345));
            assert!(!global, "--global must be asked for");
        }
        _ => panic!("parsed as another subcommand"),
    }
    assert!(
        Cli::try_parse_from(["xtask", "git-proxy", "--port", "99999"]).is_err(),
        "a port above 65535 is not a port"
    );
}

/// `linux-libos` takes the whole command for the guest as one argument, so it
/// has to be quoted at the shell and arrives with its spaces intact. Taking
/// it as several positionals would make `--args "ls -l"` and `--args ls -l`
/// mean the same thing, and only one of them is what the user typed.
#[test]
fn linux_libos_takes_the_guest_command_as_one_argument() {
    let parsed = Cli::try_parse_from(["xtask", "linux-libos", "--args", "/bin/busybox ls -l"])
        .expect("linux-libos");
    match parsed.command {
        Commands::LinuxLibos(arg) => assert_eq!(arg.args, "/bin/busybox ls -l"),
        _ => panic!("parsed as another subcommand"),
    }
    assert!(
        Cli::try_parse_from(["xtask", "linux-libos"]).is_err(),
        "linux-libos with nothing to run is not a command"
    );
}

/// `mk-sfs` takes all three of its arguments, and a size that is not a number
/// is refused rather than rounded to zero -- it is the size of the image the
/// command is about to write.
#[test]
fn mk_sfs_takes_a_directory_an_image_and_a_size_that_is_a_number() {
    let parsed = Cli::try_parse_from([
        "xtask",
        "mk-sfs",
        "--dir",
        "/tmp/in",
        "--image",
        "/tmp/out.img",
        "--size",
        "64",
    ])
    .expect("mk-sfs");
    match parsed.command {
        Commands::MkSfs(arg) => {
            assert_eq!(arg.dir, "/tmp/in");
            assert_eq!(arg.image, "/tmp/out.img");
            assert_eq!(arg.size, 64);
        }
        _ => panic!("parsed as another subcommand"),
    }
    for missing in [
        vec!["xtask", "mk-sfs", "--image", "/tmp/out.img", "--size", "64"],
        vec!["xtask", "mk-sfs", "--dir", "/tmp/in", "--size", "64"],
        vec![
            "xtask",
            "mk-sfs",
            "--dir",
            "/tmp/in",
            "--image",
            "/tmp/out.img",
        ],
    ] {
        assert!(
            Cli::try_parse_from(&missing).is_err(),
            "mk-sfs parsed without one of its arguments: {missing:?}"
        );
    }
    assert!(
        Cli::try_parse_from([
            "xtask",
            "mk-sfs",
            "--dir",
            "/tmp/in",
            "--image",
            "/tmp/out.img",
            "--size",
            "big",
        ])
        .is_err(),
        "a size that is not a number is not a size"
    );
}

/// The four subcommands that take nothing take nothing, and refuse the flags
/// their neighbours take. They are the ones with a side effect and no
/// argument to narrow it, so a flag silently ignored would be the worst kind
/// of accepted.
#[test]
fn the_subcommands_that_take_no_arguments_refuse_the_ones_they_do_not_have() {
    for sub in [
        "zircon-init",
        "update-all",
        "check-style",
        "libos-libc-test",
    ] {
        Cli::try_parse_from(["xtask", sub]).unwrap_or_else(|e| panic!("{sub}: {e}"));
        assert!(
            Cli::try_parse_from(["xtask", sub, "--arch", "riscv64"]).is_err(),
            "{sub} accepted an architecture it does nothing with"
        );
    }
}
