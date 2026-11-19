//! A dependent keeps the fence its source carried when the source is reset
//! or re-armed.
//!
//! In Linux a `sync_file`, a transferred point and a `dma_fence_array` hold
//! the `dma_fence` itself, so what happens to the syncobj afterwards is
//! nothing to them. Here a link names its source BY HANDLE. `export_fence`
//! covers the case of a lone hardware fence in flight (it is copied onto the
//! carrier); these are the cases it does not: a source whose fence is itself
//! a link, a chain, a merge made straight on the source, and a link that is
//! satisfied but queued behind a lower node of its own chain. In every one
//! of them the dependent was left waiting for the source's NEXT signal: a
//! frame late at best, and for as long as the source stayed idle at worst --
//! the hardware fences "landed" tens of seconds late on a GPU desktop with
//! explicit sync.

use super::*;

/// The source's fence is a link (an import still waiting for its own
/// source). Exported, then reset in the same breath, as Mesa does with a
/// binary semaphore: the file follows what the source followed.
#[test]
fn a_file_exported_from_an_importer_outlives_the_reset_of_the_importer() {
    let _g = test_lock();
    arm_hooks();
    let up = create(false);
    let src = create(false);
    assert!(import_snapshot(src, up, 1));
    let fd = export_fence(src).expect("a live source");
    assert!(reset(src));
    assert_eq!(query(fd), Some(0), "what it waits for has not happened");
    assert!(signal(up));
    assert_eq!(query(fd), Some(1), "the file kept the fence of the export");
    assert_eq!(query(src), Some(0), "the reset source did not");
    destroy(fd);
    destroy(src);
    destroy(up);
    assert_eq!(table_len_of(&[fd, src, up]), 0);
}

/// The same, with the source re-armed by its next submit instead of reset:
/// the file is the first fence, not the second.
#[test]
fn a_file_exported_from_an_importer_is_not_the_importers_next_submit() {
    let _g = test_lock();
    arm_hooks();
    let up = create(false);
    let src = create(false);
    assert!(import_snapshot(src, up, 1));
    let fd = export_fence(src).unwrap();
    let next = Landing::new();
    assert!(attach_hw_fence(src, 1, next.va(), 0, 5, 2, true));
    assert!(signal(up));
    assert_eq!(query(fd), Some(1), "the fence of the export");
    assert_eq!(query(src), Some(0), "the source waits for its new submit");
    destroy(fd);
    destroy(src);
    destroy(up);
}

/// A point of a chain (two fences in flight on one timeline) transferred
/// away, and the source reset before they land.
#[test]
fn a_transfer_of_a_chain_outlives_the_reset_of_its_source() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let (mut a, mut b) = (Landing::new(), Landing::new());
    assert!(attach_hw_fence(src, 1, a.va(), 0, 3, 1, false));
    assert!(attach_hw_fence(src, 2, b.va(), 0, 4, 2, false));
    let dst = create(false);
    assert!(transfer(dst, 1, src, 2));
    assert!(reset(src));
    assert_eq!(query(dst), Some(0));
    b.land(4);
    assert_eq!(query(dst), Some(0), "the node below is still in flight");
    a.land(3);
    assert_eq!(query(dst), Some(1), "the chain it was given has landed");
    assert_eq!(query(src), Some(0), "the reset source re-signals nothing");
    assert!(timeouts().is_empty());
    destroy(dst);
    destroy(src);
}

/// A merge made straight on a syncobj with a fence in flight, and the
/// syncobj re-armed by the next submit before that fence lands.
#[test]
fn a_merge_follows_the_fence_it_was_made_of_and_not_the_next_one() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let mut first = Landing::new();
    assert!(attach_hw_fence(src, 1, first.va(), 0, 8, 3, true));
    let merged = merge_fences(&[(src, 1)]);
    let second = Landing::new();
    assert!(attach_hw_fence(src, 1, second.va(), 0, 9, 3, true));
    first.land(8);
    assert_eq!(query(merged), Some(1), "the first submit is what it merged");
    assert_eq!(query(src), Some(0));
    // And through a reset.
    let merged2 = merge_fences(&[(src, 1)]);
    assert!(reset(src));
    assert_eq!(query(merged2), Some(0));
    let mut second = second;
    second.land(9);
    assert_eq!(query(merged2), Some(1));
    destroy(merged);
    destroy(merged2);
    destroy(src);
}

/// A link whose source HAS been reached, but which is queued behind a lower
/// node of its own chain, does not go back to waiting when the source is
/// re-armed for the next frame: a fence once signaled stays signaled.
#[test]
fn a_reached_link_held_behind_its_chain_stays_reached_when_its_source_is_re_armed() {
    let _g = test_lock();
    arm_hooks();
    let gate = create(false);
    let src = create(false);
    let dst = create(false);
    assert!(transfer(dst, 1, gate, 1), "the node below: still waiting");
    let mut one = Landing::new();
    assert!(attach_hw_fence(src, 1, one.va(), 0, 5, 1, true));
    assert!(transfer(dst, 2, src, 1));
    one.land(5);
    assert_eq!(query(src), Some(1));
    assert_eq!(query(dst), Some(0), "held behind point 1");
    // The producer's next frame re-arms its binary semaphore.
    let two = Landing::new();
    assert!(attach_hw_fence(src, 1, two.va(), 0, 6, 1, true));
    assert_eq!(query(src), Some(0));
    assert!(signal(gate));
    assert_eq!(
        query(dst),
        Some(2),
        "point 2 was reached before the re-arm; it does not wait for the next frame"
    );
    // The same through a reset.
    let dst2 = create(false);
    let gate2 = create(false);
    assert!(transfer(dst2, 1, gate2, 1));
    assert!(signal(src));
    assert!(transfer(dst2, 2, src, 1));
    assert!(reset(src));
    assert!(signal(gate2));
    assert_eq!(query(dst2), Some(2));
    for h in [dst, dst2, gate, gate2, src] {
        destroy(h);
    }
}

/// What is kept is an orphan like any other: collected when nothing names
/// it, and an unsignaled, fence-less source is still "its next signal".
#[test]
fn the_kept_fence_is_collected_and_a_fenceless_source_is_still_its_next_signal() {
    let _g = test_lock();
    arm_hooks();
    let before = table_len();
    let up = create(false);
    let src = create(false);
    assert!(import_snapshot(src, up, 1));
    let fd = export_fence(src).unwrap();
    assert!(reset(src));
    assert_eq!(
        table_len(),
        before + 4,
        "up, src, the file and what it kept"
    );
    assert!(signal(up));
    assert_eq!(query(fd), Some(1));
    assert_eq!(
        table_len(),
        before + 3,
        "the kept fence is gone with its use"
    );
    destroy(fd);
    // No fence at all on the source: nothing to keep, the next signal counts.
    let fd = export_fence(src).unwrap();
    assert!(reset(src));
    assert_eq!(table_len(), before + 3, "nothing was kept");
    assert!(signal(src));
    assert_eq!(query(fd), Some(1));
    destroy(fd);
    destroy(src);
    destroy(up);
    assert_eq!(table_len(), before);
}

/// How many of `handles` are still in the table.
fn table_len_of(handles: &[u32]) -> usize {
    handles.iter().filter(|&&h| exists(h)).count()
}
