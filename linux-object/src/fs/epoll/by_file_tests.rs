//! [`EpollInner::by_file`], the index a close finds its entries through.
//!
//! Its invariant is exact: the index holds exactly the (description, number)
//! pairs of the interest list. A pair left behind is an entry a later close
//! of that description drops although it watches something else now; a
//! pair missing is an entry no close ever drops, which is the bug
//! `forget_closed` exists to fix. So each test checks the index against the
//! interest list itself, and then what `forget_closed` makes of it.

use super::*;
use crate::fs::eventfd::EventFd;

const ADD: i32 = 1;
const DEL: i32 = 2;
const MOD: i32 = 3;
const IN: u32 = PollEvents::IN.bits() as u32;

fn evfd() -> Arc<dyn FileLike> {
    EventFd::new(0, OpenFlags::empty())
}

fn ev(events: u32) -> EpollEvent {
    EpollEvent { events, data: 0 }
}

/// Fails unless the index is exactly the pairs of the interest list.
fn assert_exact(ep: &Epoll) {
    let inner = ep.inner.lock();
    let pairs: BTreeSet<(usize, FileDesc)> = inner
        .interest_list
        .iter()
        .map(|(fd, (_, file))| (EpollInner::description_key(file), *fd))
        .collect();
    assert_eq!(
        inner.by_file, pairs,
        "the index is the interest list's pairs"
    );
}

#[test]
fn every_ctl_keeps_the_index_the_interest_list_s_pairs() {
    let ep = Epoll::new(OpenFlags::empty());
    let (a, b) = (evfd(), evfd());
    let (four, nine) = (FileDesc::from(4), FileDesc::from(9));
    ep.ctl(ADD, four, ev(IN), Some(a.clone())).unwrap();
    ep.ctl(ADD, nine, ev(IN), Some(a.clone())).unwrap();
    assert_exact(&ep);
    ep.ctl(MOD, four, ev(IN | EPOLLET), Some(a.clone()))
        .unwrap();
    assert_exact(&ep);
    assert_eq!(ep.inner.lock().by_file.len(), 2);
    ep.ctl(MOD, nine, ev(IN), Some(b.clone())).unwrap();
    assert_exact(&ep);
    ep.ctl(DEL, four, ev(0), Some(a)).unwrap();
    assert_exact(&ep);
    ep.ctl(DEL, nine, ev(0), Some(b)).unwrap();
    assert_exact(&ep);
    assert!(ep.inner.lock().by_file.is_empty());
}

/// An `EPOLL_CTL_MOD` that keeps the file keeps its pair. Taking the old
/// pair out after putting the new one in would take out the same pair, and
/// the close of that file would then leave its entry behind.
#[test]
fn a_mod_of_the_same_file_is_still_forgotten_at_its_close() {
    let ep = Epoll::new(OpenFlags::empty());
    let file = evfd();
    let fd = FileDesc::from(4);
    ep.ctl(ADD, fd, ev(IN), Some(file.clone())).unwrap();
    ep.ctl(MOD, fd, ev(IN | EPOLLONESHOT), Some(file.clone()))
        .unwrap();
    assert_exact(&ep);
    assert_eq!(ep.forget_closed(&file), 1);
    assert!(ep.inner.lock().interest_list.is_empty());
}

/// `EPOLL_CTL_MOD` on a number that now holds another description moves the
/// entry to that description (`ctl` documents why it is not `ENOENT` here),
/// so it is the new description's close that drops it, and the old one's
/// must not: the old one is gone from this number.
#[test]
fn a_mod_onto_another_file_moves_the_entry_to_that_file() {
    let ep = Epoll::new(OpenFlags::empty());
    let (old, new) = (evfd(), evfd());
    let fd = FileDesc::from(4);
    ep.ctl(ADD, fd, ev(IN), Some(old.clone())).unwrap();
    ep.ctl(MOD, fd, ev(IN), Some(new.clone())).unwrap();
    assert_exact(&ep);
    assert_eq!(ep.forget_closed(&old), 0, "the old file left this number");
    assert_eq!(ep.inner.lock().interest_list.len(), 1);
    assert_eq!(ep.forget_closed(&new), 1);
}

/// `EPOLL_CTL_ADD` on a number whose old entry watches a description the
/// number no longer holds replaces that entry, so the old description's
/// close must find nothing to drop under it.
#[test]
fn an_add_over_a_reused_number_leaves_nothing_for_the_old_file() {
    let ep = Epoll::new(OpenFlags::empty());
    let (old, new) = (evfd(), evfd());
    let fd = FileDesc::from(4);
    ep.ctl(ADD, fd, ev(IN), Some(old.clone())).unwrap();
    ep.ctl(ADD, fd, ev(IN), Some(new.clone())).unwrap();
    assert_exact(&ep);
    assert_eq!(ep.forget_closed(&old), 0);
    assert_eq!(ep.forget_closed(&new), 1);
}

/// An `EPOLL_CTL_DEL` takes its pair out, so a close afterwards drops
/// nothing, and in particular not an entry added later under that number.
#[test]
fn a_del_leaves_nothing_for_the_close() {
    let ep = Epoll::new(OpenFlags::empty());
    let file = evfd();
    let fd = FileDesc::from(4);
    ep.ctl(ADD, fd, ev(IN), Some(file.clone())).unwrap();
    ep.ctl(DEL, fd, ev(0), Some(file.clone())).unwrap();
    assert_exact(&ep);
    assert_eq!(ep.forget_closed(&file), 0);
}

/// A close drops its description's entries and no neighbour's, whichever
/// side of it in the index the neighbours' addresses fall: eight files,
/// each under two numbers, and the one in the middle of the address order
/// is closed.
#[test]
fn a_close_takes_only_its_own_run_of_the_index() {
    let ep = Epoll::new(OpenFlags::empty());
    let mut files: Vec<Arc<dyn FileLike>> = (0..8).map(|_| evfd()).collect();
    files.sort_by_key(EpollInner::description_key);
    for (i, file) in files.iter().enumerate() {
        ep.ctl(ADD, FileDesc::from(10 + i), ev(IN), Some(file.clone()))
            .unwrap();
        ep.ctl(ADD, FileDesc::from(30 + i), ev(IN), Some(file.clone()))
            .unwrap();
    }
    assert_eq!(ep.forget_closed(&files[4]), 2);
    assert_exact(&ep);
    let left: Vec<FileDesc> = ep.inner.lock().interest_list.keys().copied().collect();
    let expected: Vec<FileDesc> = (0..8)
        .filter(|&i| i != 4)
        .flat_map(|i| [FileDesc::from(10 + i), FileDesc::from(30 + i)])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(left, expected);
}
