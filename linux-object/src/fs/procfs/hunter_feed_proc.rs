//! `/proc/hunter_feed`: installs a signature feed after the kernel has checked
//! its signature, and says what is installed. Split out of `procfs.rs` for the
//! size ratchet.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use rcore_fs::vfs::{FileSystem, FsError, INode, Metadata, PollStatus, Result};

use super::hunter_proc::may_control_hunter;
use super::{file_metadata, seq_read_at, ProcFS};
use crate::fs::hunter_feeds::{install_from, Install};

/// Read: the last install's outcome, `require-signed-feeds`, the trusted
/// keys and every installed feed with its serial, key and size (see
/// `hunter::feed::render`).
///
/// Written, by root, one command:
///
/// ```text
/// install  <feed> <file> [<file.sig>]
/// rollback <feed> <file> [<file.sig>]
/// ```
///
/// The kernel reads the files itself (resolved like the writer's own `open`),
/// verifies them and loads the feed whole, or refuses it: `EPERM` when a
/// check failed -- the reason is the `last:` line and a `CRIT` event --,
/// `EINVAL` when the command itself does not parse. `rollback` is the only way
/// to install a signed feed with an older serial than the one installed.
/// `hunterctl update-signatures` and `hunterctl feed-rollback` drive it, and
/// move the files into `/etc/hunter/signatures.d` only once it said yes.
pub(super) struct ProcHunterFeedINode;

impl ProcHunterFeedINode {
    const INODE: usize = 171;
}

fn proc_hunter_feed_content() -> String {
    hunter::feed::render()
}

/// One parsed command.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FeedCommand<'a> {
    pub how: Install,
    pub name: &'a str,
    pub body: &'a str,
    pub sig: Option<&'a str>,
}

/// Parses the text of one write. Paths have no spaces in them: hunterctl
/// stages under `/etc/hunter/signatures.d` and its own feed names have none.
pub(super) fn parse_command(text: &str) -> Option<FeedCommand<'_>> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let how = match words.first()? {
        &"install" => Install::Update,
        &"rollback" => Install::Rollback,
        _ => return None,
    };
    match words.len() {
        3 | 4 => Some(FeedCommand {
            how,
            name: words[1],
            body: words[2],
            sig: words.get(3).copied(),
        }),
        _ => None,
    }
}

/// Carries out `cmd`, resolving its paths with `lookup`.
pub(super) fn run(
    cmd: &FeedCommand<'_>,
    lookup: &dyn Fn(&str) -> Option<Arc<dyn INode>>,
    pid: u64,
) -> Result<()> {
    let missing = |what: &str, path: &str| {
        hunter::event_log::record(
            pid,
            hunter::Severity::Critical,
            "CONTROL",
            "REJECTED",
            alloc::format!(
                "feed {}: REJECTED: cannot open {} {}; nothing installed",
                cmd.name,
                what,
                path
            ),
        );
        FsError::NoPermission
    };
    let body = lookup(cmd.body).ok_or_else(|| missing("feed", cmd.body))?;
    let sig = match cmd.sig {
        Some(p) => Some(lookup(p).ok_or_else(|| missing("signature", p))?),
        None => None,
    };
    install_from(cmd.name, body.as_ref(), sig.as_deref(), cmd.how, pid)
        .map(|_| ())
        .map_err(|_| FsError::NoPermission)
}

impl INode for ProcHunterFeedINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(proc_hunter_feed_content, offset, buf)
    }

    fn seq_text(&self) -> Option<Result<Vec<u8>>> {
        Some(Ok(proc_hunter_feed_content().into_bytes()))
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let ucred = crate::process::current_ucred();
        if !may_control_hunter(ucred) {
            return Err(FsError::NoPermission);
        }
        let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        let cmd = parse_command(text).ok_or(FsError::InvalidParam)?;
        let pid = ucred.map(|(pid, _, _)| pid as u64).unwrap_or(0);
        let proc = super::current_process();
        run(
            &cmd,
            &|path| {
                use crate::process::ProcessExt;
                proc.as_ref()?.linux().lookup_inode(path).ok()
            },
            pid,
        )?;
        Ok(buf.len())
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(file_metadata(Self::INODE, 0o644))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcore_fs::vfs::FileType;

    #[test]
    fn one_command_per_write() {
        assert_eq!(
            parse_command("install Main /etc/hunter/signatures.d/.Main.new /x.sig\n"),
            Some(FeedCommand {
                how: Install::Update,
                name: "Main",
                body: "/etc/hunter/signatures.d/.Main.new",
                sig: Some("/x.sig"),
            })
        );
        assert_eq!(
            parse_command("rollback A /a").unwrap().how,
            Install::Rollback
        );
        for bad in [
            "",
            "install A",
            "install A /a /b /c",
            "remove A /a",
            "install A /a\ninstall B /b",
        ] {
            assert!(parse_command(bad).is_none(), "accepted: {:?}", bad);
        }
    }

    #[test]
    fn a_feed_is_installed_from_its_path_and_a_missing_one_is_eperm() {
        let fs = rcore_fs_ramfs::RamFS::new();
        let root = rcore_fs::vfs::FileSystem::root_inode(&*fs);
        root.create("t-proc-feed", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"bytes:c7c7c7c7c7c7:Test.ProcFeed\n")
            .unwrap();
        let lookup = |p: &str| root.lookup(p).ok();
        let cmd = parse_command("install t-proc-feed /t-proc-feed").unwrap();
        run(&cmd, &lookup, 0).unwrap();
        assert!(ProcHunterFeedINode
            .seq_text()
            .unwrap()
            .map(|t| String::from_utf8(t).unwrap())
            .unwrap()
            .contains("\nfeed t-proc-feed serial=- key=- signatures=1"));
        let cmd = parse_command("install t-proc-feed /t-proc-feed /nope.sig").unwrap();
        assert!(matches!(run(&cmd, &lookup, 0), Err(FsError::NoPermission)));
        assert!(matches!(
            ProcHunterFeedINode.write_at(0, b"bogus\n"),
            Err(FsError::InvalidParam)
        ));
    }
}
