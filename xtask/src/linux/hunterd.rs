//! hunterd as the image ships it: the wrapper `eclipse-init` runs and the
//! service file that starts it.
//!
//! The daemon itself is `hunterctl daemon` (`tools/hunterctl/hunterctl`,
//! installed as `/sbin/hunterctl` by `install_hunter`): every few seconds it
//! appends what is new in hunter's event ring to `/var/log/hunter/events.log`
//! and the programs hunter learned to `/var/lib/hunter/learned`, which the
//! kernel reads back at the next boot. It lives apart from `linux/mod.rs`
//! because that file sits at its size ceiling (see `xtask/src/tamano.rs`).

use super::LinuxRootfs;
use std::fs;
use std::path::Path;

impl LinuxRootfs {
    /// Writes `/usr/local/bin/eclipse-hunterd` and `hunterd.service`.
    ///
    /// The wrapper sleeps instead of exiting when there is nothing to keep (a
    /// kernel without hunter, an image without hunterctl): a respawn service
    /// that exits at once is a crash to eclipse-init, which would restart it
    /// twenty times and log every one.
    pub(super) fn write_hunterd(localbin: &Path, svc_dir: &Path) {
        let script = localbin.join("eclipse-hunterd");
        fs::write(
            &script,
            b"#!/bin/sh\n\
              # hunterd: hunter's events -> /var/log/hunter/events.log, its learned\n\
              # programs -> /var/lib/hunter/learned (read back at boot). Every 5s:\n\
              # eclipse-init does not signal services before reboot(2), so at most\n\
              # the last 5s of events are not on disk after a reboot.\n\
              if [ -r /proc/hunter ] && [ -x /sbin/hunterctl ]; then\n\
              \x20 exec /sbin/hunterctl daemon 5\n\
              fi\n\
              echo 'eclipse-hunterd: no /proc/hunter or /sbin/hunterctl; nothing to keep' >&2\n\
              exec sleep 2147483647\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fs::write(
            svc_dir.join("hunterd.service"),
            b"# Keep hunter's events and learned programs on disk. See\n\
              # /usr/local/bin/eclipse-hunterd and `hunterctl history`.\n\
              exec = /usr/local/bin/eclipse-hunterd\n\
              type = respawn\n\
              log = /tmp/hunterd.log\n",
        )
        .unwrap();
    }
}
