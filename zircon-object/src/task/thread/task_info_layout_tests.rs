//! The three `zx_object_get_info` answers this file defines go out to the
//! caller's buffer byte for byte, so the order of their fields is the ABI and
//! not an implementation detail. Swapping two neighbours of the same width
//! compiles clean and changes nothing any behaviour test can reach: every read
//! and write in the kernel goes through the field *name*, so the kernel stays
//! consistent with itself and only the bytes the caller parses move.

use super::*;
use core::mem::{align_of, offset_of, size_of};

/// `zx_info_task_runtime_t`: four `zx_duration_t`, which is an `int64_t`.
/// A caller that reads them in the wrong order reports page-fault time as
/// CPU time, which is the kind of wrong that looks like a plausible
/// measurement instead of like a bug.
#[test]
fn zx_info_task_runtime_t_is_four_durations_in_a_fixed_order() {
    assert_eq!(size_of::<TaskRuntimeInfo>(), 32);
    assert_eq!(align_of::<TaskRuntimeInfo>(), 8);
    assert_eq!(offset_of!(TaskRuntimeInfo, cpu_time), 0);
    assert_eq!(offset_of!(TaskRuntimeInfo, queue_time), 8);
    assert_eq!(offset_of!(TaskRuntimeInfo, page_fault_time), 16);
    assert_eq!(offset_of!(TaskRuntimeInfo, lock_contention_time), 24);
}

/// The V1 topic is the first half of the current one, and that is the
/// whole reason both exist: a caller built against V1 reads the first
/// sixteen bytes of the same record. The offsets have to agree, not just
/// the field names that `From` copies across.
#[test]
fn the_v1_topic_is_the_first_two_durations_of_the_current_one() {
    assert_eq!(size_of::<TaskRuntimeInfoV1>(), 16);
    assert_eq!(align_of::<TaskRuntimeInfoV1>(), 8);
    assert_eq!(offset_of!(TaskRuntimeInfoV1, cpu_time), 0);
    assert_eq!(offset_of!(TaskRuntimeInfoV1, queue_time), 8);

    assert_eq!(
        offset_of!(TaskRuntimeInfoV1, cpu_time),
        offset_of!(TaskRuntimeInfo, cpu_time),
        "a V1 caller would read this out of the current record"
    );
    assert_eq!(
        offset_of!(TaskRuntimeInfoV1, queue_time),
        offset_of!(TaskRuntimeInfo, queue_time)
    );
}

/// `zx_info_thread_t`: two `uint32_t` and then a `zx_cpu_set_t`, which is
/// `uint64_t mask[8]`. The two words come first and are the same width, so
/// swapping them leaves the size and the alignment exactly as they were.
#[test]
fn zx_info_thread_t_is_two_words_and_then_the_cpu_set() {
    assert_eq!(size_of::<ThreadInfo>(), 72);
    assert_eq!(align_of::<ThreadInfo>(), 8);
    assert_eq!(offset_of!(ThreadInfo, state), 0);
    assert_eq!(offset_of!(ThreadInfo, wait_exception_channel_type), 4);
    assert_eq!(offset_of!(ThreadInfo, cpu_affinity_mask), 8);

    // zx_cpu_set_t is eight masks of sixty-four bits: 512 CPUs.
    assert_eq!(size_of::<[u64; 8]>(), 64);
    assert_eq!(
        size_of::<ThreadInfo>() - offset_of!(ThreadInfo, cpu_affinity_mask),
        64,
        "the cpu set is the whole tail, with no padding after it"
    );
}
