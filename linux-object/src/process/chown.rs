//! `chown(2)`: who may change a file's owner and group, and which of its
//! set-ID bits the change takes away, as Linux 6.18 decides it in
//! `chown_common`, `notify_change` and `setattr_prepare` (`chown-survey`, 24
//! checks, measured there with glibc and musl).
//!
//! Five answers were not Linux's. A `chown(f, -1, -1)` from somebody who
//! does not own `f` was `EPERM`; it asks for nothing and is 0. The owner
//! could not set a file's group to the group it already has without being in
//! that group. Every chown took the set-ID bits off a directory, where the
//! set-group-ID bit is what gives new files their group. A set-group-ID file
//! without group execute lost the bit even to a caller in its group. And a
//! process holding `CAP_CHOWN` but not `CAP_FOWNER` gave away a set-user-ID
//! file it did not own.

use super::*;

/// `S_IXGRP`: group execute, without which a set-group-ID bit is not about
/// running the file.
const MODE_GROUP_EXEC: u16 = 0o010;

impl LinuxProcess {
    /// Give `metadata` the owner `uid` and the group `gid`, either of which
    /// may be `-1` for "leave it as it is", or refuse with `EPERM` having
    /// changed nothing.
    pub fn chown_metadata(&self, metadata: &mut Metadata, uid: u32, gid: u32) -> LxResult {
        let creds = self.credentials();
        let (owner, group, mode) = (metadata.uid as u32, metadata.gid as u32, metadata.mode);
        let in_group_or_capable =
            |gid| Self::acts_as_group(&creds, gid, true) || creds.capable(CAP_FSETID);
        // `chown_ok`: the owner, by its filesystem uid, handing the file to
        // itself; or `CAP_CHOWN`.
        if uid != NO_ID && !(creds.fsuid == owner && uid == owner) && !creds.capable(CAP_CHOWN) {
            return Err(LxError::EPERM);
        }
        // `chgrp_ok`: the owner, to the group the file has or to one it is
        // in (`in_group_p`); or `CAP_CHOWN`.
        let owner_may =
            creds.fsuid == owner && (gid == group || Self::acts_as_group(&creds, gid, true));
        if gid != NO_ID && !owner_may && !creds.capable(CAP_CHOWN) {
            return Err(LxError::EPERM);
        }
        // `chown_common` kills the set-user-ID bit of anything but a
        // directory, and the set-group-ID bit when `setattr_should_drop_sgid`
        // says so: with group execute, or for a caller outside the group
        // without `CAP_FSETID`.
        let not_dir = metadata.type_ != FileType::Dir;
        let kill_suid = not_dir && mode & MODE_SET_UID != 0;
        let kill_sgid = not_dir
            && mode & MODE_SET_GID != 0
            && (mode & MODE_GROUP_EXEC != 0 || !in_group_or_capable(group));
        let mut new_mode = mode;
        if kill_suid || kill_sgid {
            // `notify_change` makes either a mode change, which
            // `setattr_prepare` lets only the owner or `CAP_FOWNER` make, and
            // which loses the set-group-ID bit outside the NEW group.
            if !Self::owner_or_capable(&creds, owner) {
                return Err(LxError::EPERM);
            }
            let new_group = if gid == NO_ID { group } else { gid };
            if kill_suid {
                new_mode &= !MODE_SET_UID;
            }
            if kill_sgid || !in_group_or_capable(new_group) {
                new_mode &= !MODE_SET_GID;
            }
        }
        if uid != NO_ID {
            metadata.uid = uid as _;
        }
        if gid != NO_ID {
            metadata.gid = gid as _;
        }
        metadata.mode = new_mode;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
