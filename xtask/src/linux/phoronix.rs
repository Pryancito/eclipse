//! Install the Phoronix Test Suite submodule into a Linux rootfs.

use crate::PROJECT_DIR;
use os_xtask_utils::{CommandExt, Ext, Git};
use std::{fs, path::PathBuf};

impl super::LinuxRootfs {
    /// Path of the in-tree PTS git submodule (`tools/phoronix-test-suite`).
    fn phoronix_src() -> PathBuf {
        PROJECT_DIR.join("tools").join("phoronix-test-suite")
    }

    /// Ensure the submodule is checked out; clone/init if the tree is empty.
    fn ensure_phoronix_submodule() -> PathBuf {
        let src = Self::phoronix_src();
        let marker = src.join("pts-core").join("phoronix-test-suite.php");
        if !marker.is_file() {
            println!("Initializing tools/phoronix-test-suite submodule...");
            let mut git = Git::submodule_update(true);
            git.args(["--", "tools/phoronix-test-suite"])
                .current_dir(*PROJECT_DIR)
                .invoke();
        }
        if !marker.is_file() {
            panic!(
                "phoronix-test-suite submodule missing at {} — run \
                 `git submodule update --init tools/phoronix-test-suite`",
                src.display()
            );
        }
        src
    }

    /// Install Phoronix Test Suite into this arch's rootfs (no `make` recursion).
    ///
    /// Uses upstream `install-sh` with `DESTDIR=<rootfs>` and prefix `/usr`.
    /// The guest needs PHP to run benchmarks; this only lays down the framework.
    pub fn install_phoronix(&self) {
        let src = Self::ensure_phoronix_submodule();
        let rootfs = self.path();
        fs::create_dir_all(rootfs.join("usr")).unwrap();
        fs::create_dir_all(rootfs.join("etc")).unwrap();

        println!(
            "Installing Phoronix Test Suite into {} ...",
            rootfs.display()
        );
        Ext::new("./install-sh")
            .current_dir(&src)
            .arg("/usr")
            .env("DESTDIR", rootfs.canonicalize().unwrap())
            .invoke();

        let launcher = rootfs.join("usr").join("bin").join("phoronix-test-suite");
        assert!(
            launcher.is_file(),
            "install-sh did not create {}",
            launcher.display()
        );
        println!("Installed {}", launcher.display());
    }

    /// Ensure rootfs exists (which installs PTS) or refresh the install.
    ///
    /// # Example
    ///
    /// ```bash
    /// cargo phoronix --arch x86_64
    /// ```
    pub fn put_phoronix(&self) {
        // `make(false)` already calls `install_phoronix` on both the
        // incremental and from-scratch paths; if the rootfs was skipped for
        // some reason, install explicitly.
        self.make(false);
        let launcher = self
            .path()
            .join("usr")
            .join("bin")
            .join("phoronix-test-suite");
        if !launcher.is_file() {
            self.install_phoronix();
        }
    }
}