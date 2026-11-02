use std::{ffi::OsStr, path::Path};

macro_rules! fetch_online {
    ($dst:expr, $f:expr) => {{
        use os_xtask_utils::{dir, CommandExt};
        use std::{fs, path::PathBuf};

        dir::rm(&$dst).unwrap();
        let tmp: usize = rand::random();
        let tmp = PathBuf::from("/tmp").join(tmp.to_string());
        let mut ext = $f(tmp.clone());
        let status = ext.status();
        if status.success() {
            dir::create_parent(&$dst).unwrap();
            if tmp.is_dir() {
                dircpy::copy_dir(&tmp, &$dst).unwrap();
            } else {
                fs::copy(&tmp, &$dst).unwrap();
            }
            dir::rm(tmp).unwrap();
        } else {
            dir::rm(tmp).unwrap();
            panic!(
                "Failed with code {} from {:?}",
                status.code().unwrap(),
                ext.info()
            );
        }
    }};
}

pub(crate) use fetch_online;

pub(crate) fn wget(url: impl AsRef<OsStr>, dst: impl AsRef<Path>) {
    use os_xtask_utils::Ext;

    let dst = dst.as_ref();
    if dst.exists() {
        println!("{dst:?} already exist. You can delete it manually to re-download.");
        return;
    }

    println!("wget {} from {:?}", dst.display(), url.as_ref());
    fetch_online!(dst, |tmp| {
        let mut wget = Ext::new("wget");
        wget.arg(&url).arg("-O").arg(tmp);
        wget
    });
}

/// Downloads `url` to `dst`, and reports whether it worked **without** dying.
///
/// [`wget`] is for the one place a file has to come from: it panics when the
/// download fails, which is right when there is nowhere else to look. The
/// musl toolchain has more than one place to look (see `Arch::linux_musl_cross`),
/// so a 404 from the first of them is an answer, not a build failure.
///
/// Nothing is left behind on failure: the download goes to a temporary file and
/// is only moved into place once `wget` has exited successfully, so a partial
/// transfer can never become a cached tarball that the next run trusts.
pub(crate) fn wget_optional(url: impl AsRef<OsStr>, dst: impl AsRef<Path>) -> bool {
    use os_xtask_utils::{dir, CommandExt, Ext};

    let dst = dst.as_ref();
    if dst.exists() {
        println!("{dst:?} already exist. You can delete it manually to re-download.");
        return true;
    }

    println!("wget {} from {:?}", dst.display(), url.as_ref());
    let tmp: usize = rand::random();
    let tmp = std::path::PathBuf::from("/tmp").join(tmp.to_string());
    let ok = optional_status(
        Ext::new("wget").arg(&url).arg("-O").arg(&tmp),
        "needed to download the musl toolchain",
    );
    if ok {
        dir::create_parent(dst).unwrap();
        std::fs::copy(&tmp, dst).unwrap();
    }
    let _ = dir::rm(&tmp);
    ok
}

/// Runs a host tool and reports whether it succeeded, **without** dying when
/// the tool is not installed.
///
/// `os_xtask_utils`' `CommandExt::status()` is `self.as_mut().status().unwrap()`
/// (`os-xtask-utils-0.0.0/src/lib.rs:79`). That `unwrap()` is on the *spawn*,
/// not on the exit code, so a host tool that is simply absent aborts the whole
/// build with
///
/// ```text
/// thread 'main' panicked at .../os-xtask-utils-0.0.0/src/lib.rs:79:32:
/// called `Result::unwrap()` on an `Err` value: Os { code: 2, kind: NotFound, ... }
/// ```
///
/// and never says *which* program it could not find. Observed on 2026-11-01:
/// `make image` on a plain Debian died right after "Compiling thr3", because
/// `install_silversurfer`'s `pkg-config --exists gstreamer-1.0` probe -- a
/// deliberately best-effort check, whose failure is supposed to mean "build
/// SilverSurfer without media" -- hit a host with no `pkg-config` installed.
///
/// Every *optional* host tool goes through here instead: a missing program
/// prints one line naming it (plus `hint`) and counts as a failed check, which
/// is exactly what a failing exit code already meant at those call sites.
///
/// Required tools still use `invoke()`, which keeps the same opaque panic; this
/// only covers the probes the build is designed to survive.
pub(crate) fn optional_status(cmd: &mut os_xtask_utils::Ext, hint: &str) -> bool {
    use os_xtask_utils::CommandExt;
    use std::process::Command;

    let info = cmd.info().to_string_lossy().into_owned();
    let program = AsRef::<Command>::as_ref(cmd)
        .get_program()
        .to_string_lossy()
        .into_owned();
    let result = AsMut::<Command>::as_mut(cmd).status();
    match result {
        Ok(status) => status.success(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("warning: `{program}` is not installed ({hint}); skipping `{info}`");
            false
        }
        Err(e) => {
            eprintln!("warning: could not run `{program}` ({info}): {e}");
            false
        }
    }
}

#[cfg(test)]
mod wget_optional_tests {
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xtask-wget-{tag}-{}", std::process::id()))
    }

    /// The regression this exists for: a download that does not work comes back
    /// as `false` so the next source gets its turn, instead of killing the
    /// build the way `wget` does.
    #[test]
    fn a_download_that_fails_is_false_and_leaves_nothing_behind() {
        let dst = scratch("fail");
        let _ = std::fs::remove_file(&dst);
        assert!(!super::wget_optional(
            "eclipse-no-such-scheme://x/y.tgz",
            &dst
        ));
        assert!(
            !dst.exists(),
            "a failed download left {} behind, and the next run would trust it",
            dst.display()
        );
    }

    /// A file already downloaded is not downloaded again, which is what makes
    /// the toolchain cache a cache. (No network is touched: the URL here is
    /// not a real one, and reaching it would fail.)
    #[test]
    fn a_file_already_there_counts_as_downloaded() {
        let dst = scratch("cached");
        std::fs::write(&dst, b"cached").unwrap();
        assert!(super::wget_optional(
            "eclipse-no-such-scheme://x/y.tgz",
            &dst
        ));
        assert_eq!(std::fs::read(&dst).unwrap(), b"cached");
        std::fs::remove_file(&dst).unwrap();
    }
}

#[cfg(test)]
mod optional_status_tests {
    use os_xtask_utils::{CommandExt, Ext};

    /// The regression this exists for: an absent host tool must come back as
    /// `false`, not as a panic from inside `os-xtask-utils`.
    #[test]
    fn missing_program_is_false_and_not_a_panic() {
        let mut cmd = Ext::new("eclipse-no-such-program-xtask-test");
        assert!(!super::optional_status(&mut cmd, "hint"));
    }

    #[test]
    fn exit_code_still_decides_when_the_program_exists() {
        assert!(super::optional_status(
            Ext::new("sh").arg("-c").arg("exit 0"),
            "hint"
        ));
        assert!(!super::optional_status(
            Ext::new("sh").arg("-c").arg("exit 3"),
            "hint"
        ));
    }
}
