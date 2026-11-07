//! The VFS types, against the scripted server in [`super::fake`].
//!
//! What is worth measuring here is not the protocol — the batches below have
//! that — but the *shape of the traffic* the kernel's filesystem interface
//! produces. Three of this layer's four traps are invisible in the result
//! and visible only in the calls:
//!
//! - `get_entry(id)` is an index and NFSv3 directories are cookie chains, so
//!   a listing read once per index is both quadratic and wrong. The result
//!   looks right either way; the call count does not.
//! - the attribute cache is only correct if this client's own changes
//!   invalidate it, and a stale size after a write reads a file short.
//! - `unlink` has to choose `REMOVE` or `RMDIR`, and the choice is in the
//!   procedure number and nowhere else.
//!
//! So most of what follows asserts on [`Server::calls`].

use super::*;
use crate::fs::nfs::fake::*;
use crate::fs::nfs::nfs3::{Ftype3, PROC_GETATTR, PROC_LOOKUP, PROC_READLINK};
use crate::fs::nfs::nfs3_dir::PROC_READDIRPLUS;
use crate::fs::nfs::nfs3_write::{
    PROC_CREATE, PROC_LINK, PROC_MKDIR, PROC_REMOVE, PROC_RENAME, PROC_RMDIR, PROC_SETATTR,
    PROC_WRITE,
};
use crate::fs::nfs::xdr::XdrDecoder;
use alloc::vec;

/// A filesystem over `server`, with the cache timeouts of a real mount.
fn mounted(server: Server) -> Arc<NfsFileSystem<Server>> {
    NfsFileSystem::new(session(server), a_handle())
}

/// The same with both caches off, which is how a refetch is measured without
/// waiting out seconds of a real clock.
fn uncached(server: Server) -> Arc<NfsFileSystem<Server>> {
    NfsFileSystem::with_ttls(session(server), a_handle(), 0, 0)
}

/// The procedure number of every call made, in order.
fn procedures(fs: &Arc<NfsFileSystem<Server>>) -> Vec<u32> {
    fs.session()
        .lock()
        .transport()
        .calls
        .iter()
        .map(|c| c.procedure)
        .collect()
}

/// How many calls of `procedure` were made.
fn count_of(fs: &Arc<NfsFileSystem<Server>>, procedure: u32) -> usize {
    procedures(fs).iter().filter(|p| **p == procedure).count()
}

/// The arguments of the n-th call.
fn args_of(fs: &Arc<NfsFileSystem<Server>>, n: usize) -> Vec<u8> {
    fs.session().lock().transport().calls[n].args.clone()
}

/// A directory of three entries, all with attributes, in one reply.
fn three_entries() -> Vec<u8> {
    readdirplus_ok(
        &[
            PlusEntry {
                fileid: 10,
                name: "alpha",
                cookie: 1,
                ftype: Some(Ftype3::Reg),
                fh: Some(&[0xaa]),
            },
            PlusEntry {
                fileid: 11,
                name: "beta",
                cookie: 2,
                ftype: Some(Ftype3::Dir),
                fh: Some(&[0xbb]),
            },
            PlusEntry {
                fileid: 12,
                name: "gamma",
                cookie: 3,
                ftype: Some(Ftype3::Reg),
                fh: Some(&[0xcc]),
            },
        ],
        true,
    )
}

/// A server with one `GETATTR` queued, saying the root is a directory.
///
/// One and not many: the fake hands out its answers in order, whatever the
/// call was, so a test that queues more than it needs is a test whose later
/// calls are answered by the wrong reply.
fn dir_server() -> Server {
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server
}

// --------------------------------------------------------- the listing

#[test]
fn a_listing_is_read_once_however_many_entries_are_asked_for() {
    // The trap this layer exists to avoid. `get_entry` is an index and the
    // protocol is a cookie chain, so the obvious implementation -- walk the
    // chain to the id-th entry, per call -- is quadratic in the number of
    // names and asks the server n times for a directory of n files. The
    // names it returns are right, so only the call count shows it.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    let fs = mounted(server);
    let root = fs.root_inode();

    for id in 0..3 {
        root.get_entry(id).expect("an entry that is there");
    }

    assert_eq!(
        count_of(&fs, PROC_READDIRPLUS),
        1,
        "the directory was listed once per index"
    );
}

#[test]
fn the_names_come_back_in_the_order_the_server_sent_them() {
    // A listing is a sequence and the index means position in it. Sorting it
    // -- or building it from a map -- renames every entry for `readdir`,
    // which is how a program that remembers an offset comes back to a
    // different file.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    let fs = mounted(server);
    let root = fs.root_inode();

    let names: Vec<String> = (0..3).map(|i| root.get_entry(i).unwrap()).collect();

    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
}

#[test]
fn an_index_past_the_end_is_the_end_of_the_listing() {
    // `INode::list` walks indices until this error, so it has to be
    // `EntryNotFound` and not an I/O error: anything else turns the end of
    // every directory into a failure.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    let fs = mounted(server);

    let err = fs
        .root_inode()
        .get_entry(3)
        .expect_err("an index past the end");

    assert_eq!(err, FsError::EntryNotFound);
}

#[test]
fn a_fresh_listing_is_not_read_again_and_a_stale_one_is() {
    // The two halves of one claim. Without the first the cache does nothing;
    // without the second it never expires, and a directory somebody else
    // changed never shows the change.
    // The answers are a queue the fake empties in order, whatever the call
    // was, so a script is the exact sequence of calls the test expects. An
    // answer too many is an answer given to the wrong call, and the test
    // fails with a decode error rather than a useful one -- which is the
    // point: a fake that handed out replies by procedure would answer a
    // call this layer should never have made.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    let cached = mounted(server);
    // One inode and not two: a fresh `root_inode()` is a fresh inode with an
    // empty cache, so asking it twice would measure nothing.
    let root = cached.root_inode();
    root.get_entry(0).unwrap();
    root.get_entry(0).unwrap();
    assert_eq!(count_of(&cached, PROC_READDIRPLUS), 1);

    let mut server = Server::new();
    for _ in 0..2 {
        server.answer(getattr_of(Ftype3::Dir, 4096, 2));
        server.answer(three_entries());
    }
    let stale = uncached(server);
    let root = stale.root_inode();
    root.get_entry(0).unwrap();
    root.get_entry(0).unwrap();
    assert_eq!(count_of(&stale, PROC_READDIRPLUS), 2);
}

#[test]
fn an_entry_the_listing_brought_attributes_for_costs_no_second_call() {
    // The whole point of asking for `READDIRPLUS` rather than `READDIR`: a
    // `ls -l` of n files is one call, not one `LOOKUP` and one `GETATTR` per
    // file. Throwing the attributes away and looking each entry up would
    // pass every test about the *values* and undo the optimization.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    let fs = mounted(server);

    let (meta, name) = fs.root_inode().get_entry_with_metadata(0).unwrap();

    assert_eq!(name, "alpha");
    assert_eq!(meta.inode, 10);
    assert_eq!(count_of(&fs, PROC_LOOKUP), 0);
    assert_eq!(
        count_of(&fs, PROC_GETATTR),
        1,
        "only the is-a-directory one"
    );
}

#[test]
fn an_entry_the_server_sent_bare_is_looked_up_for_itself() {
    // A server may send an entry with no attributes, and that is legal. The
    // entry still has to come back with metadata, so this one falls back to
    // a lookup -- for that entry only.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(readdirplus_ok(
        &[PlusEntry {
            fileid: 10,
            name: "bare",
            cookie: 1,
            ftype: None,
            fh: None,
        }],
        true,
    ));
    // No second `GETATTR`: the lookup's own reply carries the entry's
    // attributes, and the directory's are already cached.
    server.answer(lookup_ok(&[0xaa], Ftype3::Reg, 7, 10));
    let fs = mounted(server);

    let (meta, name) = fs.root_inode().get_entry_with_metadata(0).unwrap();

    assert_eq!(name, "bare");
    assert_eq!(meta.size, 7);
    assert_eq!(count_of(&fs, PROC_LOOKUP), 1);
}

// --------------------------------------------------------- the symlink

#[test]
fn reading_a_symlink_answers_with_its_target() {
    // `read_symlink` goes through `read_at`, so a symlink that answered with
    // `READ` would resolve to whatever the server has as the link's *data* --
    // nothing, on every server -- and every symlink on the mount would point
    // at the empty path.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Lnk, 11, 5));
    server.answer(readlink_ok(b"../elsewhere"));
    let fs = mounted(server);
    let link = fs.root_inode();

    let mut buf = [0u8; 32];
    let n = link.read_at(0, &mut buf).unwrap();

    assert_eq!(&buf[..n], b"../elsewhere");
    assert_eq!(count_of(&fs, PROC_READLINK), 1);
}

#[test]
fn a_symlink_read_from_past_its_target_is_empty_and_not_an_error() {
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Lnk, 11, 5));
    server.answer(readlink_ok(b"short"));
    let fs = mounted(server);

    let mut buf = [0u8; 8];
    assert_eq!(fs.root_inode().read_at(99, &mut buf).unwrap(), 0);
}

// ----------------------------------------------------- the attribute cache

#[test]
fn a_fresh_stat_is_not_asked_twice_and_a_stale_one_is() {
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 7, 3));
    let cached = mounted(server);
    let file = cached.root_inode();
    file.metadata().unwrap();
    file.metadata().unwrap();
    assert_eq!(count_of(&cached, PROC_GETATTR), 1);

    let mut server = Server::new();
    server.answer_times(2, getattr_of(Ftype3::Reg, 7, 3));
    let stale = uncached(server);
    let file = stale.root_inode();
    file.metadata().unwrap();
    file.metadata().unwrap();
    assert_eq!(count_of(&stale, PROC_GETATTR), 2);
}

#[test]
fn a_write_of_this_clients_own_does_not_leave_the_old_size_cached() {
    // The failure this prevents is not a slow `stat` but a wrong one: an
    // archiver writes a file, asks how big it is, and is told the size from
    // before the write. It then reads the file short, and the copy is
    // silently truncated.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 0, 3));
    server.answer(write_ok(4, 2));
    server.answer(getattr_of(Ftype3::Reg, 4, 3));
    let fs = mounted(server);
    let file = fs.root_inode();

    assert_eq!(file.metadata().unwrap().size, 0);
    file.write_at(0, b"abcd").unwrap();

    assert_eq!(
        file.metadata().unwrap().size,
        4,
        "the size from before the write"
    );
    assert_eq!(count_of(&fs, PROC_WRITE), 1);
}

#[test]
fn creating_a_name_throws_the_directorys_listing_away() {
    // Appending the new name instead would put it wherever this client
    // guessed rather than where the server's cookie order puts it, and every
    // reader would see the wrong order until the timeout.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    server.answer(created(Some(&[0xdd]), Ftype3::Reg, 13));
    server.answer(three_entries());
    let fs = mounted(server);
    let root = fs.root_inode();

    root.get_entry(0).unwrap();
    root.create("delta", FileType::File, 0o644).unwrap();
    root.get_entry(0).unwrap();

    assert_eq!(
        count_of(&fs, PROC_READDIRPLUS),
        2,
        "the listing survived a create"
    );
}

// ------------------------------------------------------------- unlink

#[test]
fn unlinking_a_directory_is_rmdir_and_unlinking_a_file_is_remove() {
    // The same arguments and two procedures. A server will not remove a
    // directory with `REMOVE`, nor a file with `RMDIR`, so getting it wrong
    // makes every `rmdir` on the mount fail -- or every `rm`.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    server.answer(wcc_only(1)); // the RMDIR
    server.answer(three_entries()); // the listing a removal threw away
    server.answer(wcc_only(1)); // the REMOVE
    let fs = mounted(server);
    let root = fs.root_inode();

    root.get_entry(0).unwrap();
    root.unlink("beta").expect("the directory");
    // Listed again on purpose: the removal threw the listing away, and
    // without one the type would come from a `LOOKUP` instead -- which is
    // the next test, not this one.
    root.get_entry(0).unwrap();
    root.unlink("alpha").expect("the file");

    // The names and not just the procedures: both calls are made either
    // way, so "an RMDIR happened" and "a REMOVE happened" hold just as well
    // when the two are swapped. What has to be true is which *name* each one
    // carried.
    assert_eq!(removal_of(&fs, PROC_RMDIR), Some(String::from("beta")));
    assert_eq!(removal_of(&fs, PROC_REMOVE), Some(String::from("alpha")));
}

/// The name the first `procedure` call asked to remove, if it was made.
///
/// `REMOVE`, `RMDIR` and the rest of `diropargs3` are a handle and a name.
fn removal_of(fs: &Arc<NfsFileSystem<Server>>, procedure: u32) -> Option<String> {
    let session = fs.session().lock();
    let call = session
        .transport()
        .calls
        .iter()
        .find(|c| c.procedure == procedure)?;
    let mut dec = XdrDecoder::new(&call.args);
    dec.opaque().expect("the handle");
    Some(String::from_utf8(dec.string_bytes().expect("the name").to_vec()).expect("utf-8"))
}

#[test]
fn unlink_takes_the_type_from_the_listing_it_already_has() {
    // The caller of `unlink` has almost always just listed the directory, so
    // the type is already here. One extra `LOOKUP` per removed file is what
    // makes `rm -r` of a big tree twice the traffic it has to be.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(three_entries());
    server.answer(wcc_only(1));
    let fs = mounted(server);
    let root = fs.root_inode();

    root.get_entry(0).unwrap();
    root.unlink("alpha").unwrap();

    assert_eq!(count_of(&fs, PROC_LOOKUP), 0);
}

#[test]
fn unlink_without_a_listing_to_go_on_looks_the_name_up() {
    // The other half: with no fresh listing the type is not known, and
    // guessing is an error either way round.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    server.answer(lookup_ok(&[0xaa], Ftype3::Reg, 7, 10));
    server.answer(wcc_only(1));
    let fs = mounted(server);

    fs.root_inode().unlink("alpha").unwrap();

    assert_eq!(count_of(&fs, PROC_LOOKUP), 1);
    assert!(procedures(&fs).contains(&PROC_REMOVE));
}

// -------------------------------------------------------- lookup and find

#[test]
fn finding_dot_costs_no_lookup_and_finding_dotdot_asks_the_server() {
    // `.` is this inode and the caller already has it. `..` is *not*: on a
    // server the parent of a directory is whatever the server says, and the
    // parent of an export's root is outside the export. Short-circuiting it
    // to self would make `cd ..` out of a mount's root land back in the
    // mount.
    let fs = mounted(dir_server());
    let root = fs.root_inode();
    root.find(".").expect("dot");
    assert_eq!(count_of(&fs, PROC_LOOKUP), 0);

    let mut server = dir_server();
    server.answer(lookup_ok(&[0x11], Ftype3::Dir, 4096, 1));
    let fs = mounted(server);
    fs.root_inode().find("..").expect("dotdot");
    assert_eq!(count_of(&fs, PROC_LOOKUP), 1);
}

#[test]
fn a_create_the_server_answered_without_a_handle_looks_the_name_up() {
    // A `CREATE` reply may legally carry no filehandle: the name has to be
    // looked up to get one. Unwrapping it would panic in kernel context on a
    // reply the protocol allows.
    let mut server = dir_server();
    server.answer(created(None, Ftype3::Reg, 13));
    server.answer(lookup_ok(&[0xdd], Ftype3::Reg, 0, 13));
    let fs = mounted(server);

    fs.root_inode()
        .create("delta", FileType::File, 0o644)
        .unwrap();

    assert_eq!(count_of(&fs, PROC_LOOKUP), 1);
    assert!(procedures(&fs).contains(&PROC_CREATE));
}

#[test]
fn a_directory_is_made_with_mkdir_and_not_with_create() {
    let mut server = dir_server();
    server.answer(created(Some(&[0xee]), Ftype3::Dir, 14));
    let fs = mounted(server);

    fs.root_inode().create("sub", FileType::Dir, 0o755).unwrap();

    assert!(procedures(&fs).contains(&PROC_MKDIR));
    assert_eq!(count_of(&fs, PROC_CREATE), 0);
}

// ------------------------------------------------------------- metadata

#[test]
fn a_stale_handle_comes_out_as_a_stale_handle() {
    // `ESTALE` is the ordinary outcome on a network filesystem and the
    // caller's answer to it is to look the name up again. Reported as a
    // device error there is nothing to retry and the open fails for good.
    let mut server = Server::new();
    server.answer(failure(70)); // NFS3ERR_STALE
    let fs = mounted(server);

    let err = fs.root_inode().metadata().expect_err("a stale handle");

    assert_eq!(err, FsError::StaleHandle);
}

#[test]
fn the_type_stays_out_of_the_mode() {
    // NFSv3 keeps the type in its own field and so does the kernel's
    // `Metadata`. Packing the type bits into `mode` as well is how a
    // directory comes out of `stat` as a file with a strange mode.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Dir, 4096, 2));
    let fs = mounted(server);

    let meta = fs.root_inode().metadata().unwrap();

    assert_eq!(meta.type_, FileType::Dir);
    assert_eq!(meta.mode, 0o644, "the type leaked into the mode");
}

#[test]
fn a_file_smaller_than_a_sector_still_uses_a_block() {
    // `used / 512` rounded down is zero blocks for every file under half a
    // kilobyte, and `du` of a source tree then reports nothing at all.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 1, 3));
    let fs = mounted(server);

    assert_eq!(fs.root_inode().metadata().unwrap().blocks, 1);
}

#[test]
fn setting_the_metadata_never_sends_a_size() {
    // `set_metadata` is called with the metadata a caller read and changed
    // one field of. A `SETATTR` carrying a size is a truncate, so sending
    // the size along with a mode change destroys the file whenever the
    // cached size was stale. `resize` is the operation for that.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 4096, 3));
    server.answer(setattr_ok(4096));
    let fs = mounted(server);
    let file = fs.root_inode();
    let mut meta = file.metadata().unwrap();
    meta.mode = 0o600;

    file.set_metadata(&meta).unwrap();

    // mode, uid, gid, then the size's "is it set" boolean.
    let args = args_of(&fs, 1);
    let mut dec = XdrDecoder::new(&args);
    dec.opaque().expect("handle");
    for _ in 0..3 {
        assert_eq!(dec.u32().expect("set"), 1);
        dec.u32().expect("value");
    }
    assert_eq!(
        dec.u32().expect("the size's flag"),
        0,
        "a SETATTR with a size"
    );
    assert!(procedures(&fs).contains(&PROC_SETATTR));
}

// ---------------------------------------------------------------- statfs

#[test]
fn statfs_reports_the_servers_counters_in_blocks() {
    let mut server = Server::new();
    server.answer(fsstat_ok(8192, 4096, 2048, 100, 40));
    let fs = mounted(server);

    let info = fs.info();

    assert_eq!(info.blocks, 8192 / info.bsize);
    assert_eq!(info.bfree, 4096 / info.bsize);
    assert_eq!(info.bavail, 2048 / info.bsize);
    assert_eq!(info.files, 100);
    assert_eq!(info.ffree, 40);
}

#[test]
fn a_server_that_will_not_answer_fsstat_still_has_a_mounted_filesystem() {
    // `statfs` failing would make `df` fail for every mount on the machine,
    // and the mount itself works. Zeros are what Linux reports for a
    // filesystem that does not know its own size.
    let mut server = Server::new();
    server.answer(failure(5)); // NFS3ERR_IO
    let fs = mounted(server);

    let info = fs.info();

    assert_eq!(info.blocks, 0);
    assert_eq!(info.ftype, NFS_SUPER_MAGIC);
}

#[test]
fn the_filesystem_magic_is_the_one_userspace_checks_for() {
    // Build systems and lock libraries branch on exactly this value to
    // decide whether they are standing on a network filesystem.
    let mut server = Server::new();
    server.answer(fsstat_ok(0, 0, 0, 0, 0));
    let fs = mounted(server);

    assert_eq!(fs.info().ftype, 0x6969);
}

// ------------------------------------------------------- across mounts

#[test]
fn a_hard_link_to_another_mount_is_exdev_and_not_a_type_error() {
    // What `mv` across a mount point branches on: `EXDEV` means "copy and
    // delete instead", and anything else means the move just failed.
    let here = mounted(dir_server());
    let there = mounted(dir_server());

    let err = here
        .root_inode()
        .link("name", &there.root_inode())
        .expect_err("a link across two mounts");

    assert_eq!(err, FsError::NotSameFs);
}

#[test]
fn a_hard_link_within_one_mount_is_sent() {
    let mut server = dir_server();
    server.answer(lookup_ok(&[0xaa], Ftype3::Reg, 7, 10));
    server.answer(link_ok());
    let fs = mounted(server);
    let root = fs.root_inode();
    // One `root` throughout: the second call's "is this a directory" comes
    // from the cache the first one filled, so the script holds one `GETATTR`.
    let file = root.find("alpha").unwrap();

    root.link("second", &file).expect("a link within one mount");

    assert!(procedures(&fs).contains(&PROC_LINK));
}

#[test]
fn a_rename_across_two_mounts_is_refused_before_the_wire() {
    let here = mounted(dir_server());
    let there = mounted(dir_server());

    let err = here
        .root_inode()
        .move_("a", &there.root_inode(), "b")
        .expect_err("a rename across two mounts");

    assert_eq!(err, FsError::NotSameFs);
    assert_eq!(count_of(&here, PROC_RENAME), 0);
}

// ------------------------------------------------------------ not a dir

#[test]
fn a_file_is_not_a_directory_for_the_calls_that_need_one() {
    // Reached through `openat` on a path whose middle component is a file.
    // Answering `EntryNotFound` instead would make the shell report "no
    // such file" for a path that exists.
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 7, 3));
    let fs = mounted(server);
    // One inode, so one `GETATTR`: the three refusals below all come from
    // the same cached answer.
    let file = fs.root_inode();

    assert_eq!(file.get_entry(0).unwrap_err(), FsError::NotDir);
    assert_eq!(file.find("x").err(), Some(FsError::NotDir));
    assert_eq!(file.unlink("x").unwrap_err(), FsError::NotDir);
}

#[test]
fn a_read_only_mount_polls_as_not_writable() {
    let mut server = Server::new();
    server.answer(getattr_of(Ftype3::Reg, 7, 3));
    let fs = NfsFileSystem::new(read_only_session(server), a_handle());

    assert!(!fs.root_inode().poll().unwrap().write);
}
