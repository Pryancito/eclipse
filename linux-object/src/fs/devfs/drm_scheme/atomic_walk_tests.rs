//! The walk over a `DRM_IOCTL_MODE_ATOMIC` request's property arrays.
//!
//! Its shape is the uAPI's and it is easy to get subtly wrong:
//! `props_ptr` and `prop_values_ptr` are **one** pair of arrays shared by
//! every object in the request, and `count_props_ptr[i]` says how many of
//! them object `i` takes, carrying on from where the previous object
//! stopped. Nothing states their total length, so each object's span is
//! what gets checked against the user mapping.
//!
//! Until this walk was pulled out there were two copies of it: the ioctl
//! arm that stages the commit, and the fence scan that runs first to find
//! the IN_FENCE_FD to sleep on. Two readers of the same arrays is a
//! standing invitation for one to see a property the other does not.
//!
//! One thing here cannot be tested from the host, and it is worth saying
//! so rather than leaving someone to wonder: the `access_ok()` on each
//! object's span always passes. On libos there is no user/kernel split,
//! so `user_range_ok` only refuses a null pointer with bytes to move,
//! and that case is already caught by the explicit check above it. Taking
//! both span checks out leaves every test here green. What *is* covered
//! is the arithmetic that feeds them: the running index, the two counts
//! that bound it, and `ucheck_n`'s refusal to wrap.

use super::*;
use alloc::vec::Vec;

/// Back a request with real arrays. Everything stays alive as long as the
/// `Request` does, which is what makes the raw pointers inside it sound.
struct Request {
    objs: Vec<u32>,
    counts: Vec<u32>,
    props: Vec<u32>,
    values: Vec<u64>,
}

impl Request {
    fn new(objs: &[u32], counts: &[u32], props: &[u32], values: &[u64]) -> Self {
        Self {
            objs: objs.to_vec(),
            counts: counts.to_vec(),
            props: props.to_vec(),
            values: values.to_vec(),
        }
    }

    fn req(&self) -> DrmModeAtomic {
        DrmModeAtomic {
            flags: 0,
            count_objs: self.objs.len() as u32,
            objs_ptr: self.objs.as_ptr() as u64,
            count_props_ptr: self.counts.as_ptr() as u64,
            props_ptr: self.props.as_ptr() as u64,
            prop_values_ptr: self.values.as_ptr() as u64,
            reserved: 0,
            user_data: 0,
        }
    }

    fn visited(&self) -> Result<Vec<(u32, u32, u64)>> {
        let mut seen = Vec::new();
        walk_atomic_props(&self.req(), |o, p, v| {
            seen.push((o, p, v));
            Ok(())
        })?;
        Ok(seen)
    }
}

#[test]
fn a_request_is_visited_object_by_object_in_order() {
    let r = Request::new(
        &[4, 1],
        &[2, 1],
        &[PROP_FB_ID, PROP_CRTC_ID, PROP_ACTIVE],
        &[7, 1, 1],
    );
    assert_eq!(
        r.visited().unwrap(),
        alloc::vec![
            (4, PROP_FB_ID, 7),
            (4, PROP_CRTC_ID, 1),
            (1, PROP_ACTIVE, 1),
        ],
    );
}

/// The one that bites. The shared arrays are indexed by a *running*
/// counter, not restarted per object: the second object's properties
/// begin where the first object's ended. Restarting at 0 would hand
/// object 1 object 0's properties -- a commit that looks well-formed and
/// programs the wrong thing.
#[test]
fn the_shared_arrays_run_on_from_one_object_to_the_next() {
    let r = Request::new(
        &[4, 1, 2],
        &[2, 1, 1],
        &[PROP_SRC_W, PROP_SRC_H, PROP_ACTIVE, PROP_CRTC_ID],
        &[100, 200, 1, 1],
    );
    let seen = r.visited().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen[2],
        (1, PROP_ACTIVE, 1),
        "the CRTC got the plane's properties: the index restarted",
    );
    assert_eq!(seen[3], (2, PROP_CRTC_ID, 1));
}

/// An object may name no properties at all, and then it consumes none of
/// the shared arrays -- the next object still starts where the last one
/// that had properties stopped.
#[test]
fn an_object_with_no_properties_consumes_none_of_the_shared_arrays() {
    let r = Request::new(&[4, 1, 2], &[1, 0, 1], &[PROP_FB_ID, PROP_CRTC_ID], &[7, 1]);
    assert_eq!(
        r.visited().unwrap(),
        alloc::vec![(4, PROP_FB_ID, 7), (2, PROP_CRTC_ID, 1)],
    );
}

#[test]
fn an_empty_request_visits_nothing() {
    let r = Request::new(&[], &[], &[], &[]);
    assert_eq!(r.visited().unwrap(), Vec::new());
}

/// Both counts are bounded before anything is read. The pipeline has three
/// objects with sixteen properties between them; the bound is well above
/// that and its job is to keep a hostile request from walking for a long
/// time inside the kernel.
#[test]
fn the_two_counts_are_bounded() {
    let objs: Vec<u32> = (0..65).collect();
    let counts: Vec<u32> = alloc::vec![0; 65];
    let r = Request::new(&objs, &counts, &[], &[]);
    assert_eq!(r.visited(), Err(FsError::InvalidParam), "65 objects");

    let objs: Vec<u32> = (0..64).collect();
    let counts: Vec<u32> = alloc::vec![0; 64];
    let r = Request::new(&objs, &counts, &[], &[]);
    assert!(r.visited().is_ok(), "64 objects is the last accepted");

    let props: Vec<u32> = alloc::vec![PROP_ACTIVE; 65];
    let values: Vec<u64> = alloc::vec![0; 65];
    let r = Request::new(&[1], &[65], &props, &values);
    assert_eq!(
        r.visited(),
        Err(FsError::InvalidParam),
        "65 properties on one object",
    );
    let r = Request::new(&[1], &[64], &props, &values);
    assert_eq!(
        r.visited().map(|v| v.len()),
        Ok(64),
        "64 on one object is the last accepted",
    );
}

/// A request that claims properties but hands no array to read them from.
/// Claiming none is fine: that is how a client names an object without
/// changing anything on it.
#[test]
fn an_absent_array_is_only_an_error_when_there_is_something_to_read() {
    let r = Request::new(&[4], &[1], &[PROP_FB_ID], &[7]);

    let mut req = r.req();
    req.props_ptr = 0;
    assert_eq!(
        walk_atomic_props(&req, |_, _, _| Ok(())),
        Err(FsError::InvalidParam),
    );

    let mut req = r.req();
    req.prop_values_ptr = 0;
    assert_eq!(
        walk_atomic_props(&req, |_, _, _| Ok(())),
        Err(FsError::InvalidParam),
    );

    // Same request with nothing claimed: the arrays are never touched.
    let empty = Request::new(&[4], &[0], &[], &[]);
    let mut req = empty.req();
    req.props_ptr = 0;
    req.prop_values_ptr = 0;
    assert_eq!(walk_atomic_props(&req, |_, _, _| Ok(())), Ok(()));

    // The object list itself is not optional.
    let mut req = r.req();
    req.objs_ptr = 0;
    assert_eq!(
        walk_atomic_props(&req, |_, _, _| Ok(())),
        Err(FsError::InvalidParam),
    );
    let mut req = r.req();
    req.count_props_ptr = 0;
    assert_eq!(
        walk_atomic_props(&req, |_, _, _| Ok(())),
        Err(FsError::InvalidParam),
    );
}

/// A property the pipeline refuses stops the commit there and then. The
/// walk must not carry on staging the rest: a partly-applied atomic commit
/// is the one thing the atomic uAPI promises cannot happen.
#[test]
fn a_refused_property_stops_the_walk_where_it_failed() {
    // One object claiming three properties, the middle one immutable.
    let r = Request::new(
        &[4],
        &[3],
        &[PROP_FB_ID, PROP_TYPE, PROP_CRTC_ID],
        &[7, 1, 1],
    );
    let mut seen = Vec::new();
    let got = walk_atomic_props(&r.req(), |o, p, v| {
        seen.push((o, p, v));
        // `type` is immutable, exactly as `atomic_stage_on` says.
        if p == PROP_TYPE {
            return Err(FsError::InvalidParam);
        }
        Ok(())
    });
    assert_eq!(got, Err(FsError::InvalidParam));
    assert_eq!(seen.len(), 2, "the third property was read anyway");
}

/// The reason the walk is shared. The fence scan runs before the commit
/// and picks the IN_FENCE_FD to sleep on; the commit then stages it. When
/// a request names the property twice, both have to land on the same one,
/// or the commit sleeps on a fence it will not use.
#[test]
fn the_fence_scan_and_the_staging_pick_the_same_in_fence() {
    // Both readings of the same request: `atomic_in_fence`'s fold, and
    // what the ioctl arm ends up staging.
    let both = |values: &[u64]| -> (Option<i32>, Option<i32>) {
        let props = alloc::vec![PROP_IN_FENCE_FD; values.len()];
        let counts = alloc::vec![1u32; values.len()];
        let objs = alloc::vec![4u32; values.len()];
        let r = Request::new(&objs, &counts, &props, values);

        let mut fd: Option<i32> = None;
        walk_atomic_props(&r.req(), |_, prop_id, value| {
            fold_in_fence(&mut fd, prop_id, value);
            Ok(())
        })
        .unwrap();

        let mut upd = drm::AtomicUpdate::default();
        walk_atomic_props(&r.req(), |_, prop_id, value| {
            atomic_stage_on(&mut upd, AtomicObject::Plane, prop_id, value)
        })
        .unwrap();
        (fd, upd.in_fence_fd)
    };

    for (values, want) in [
        (alloc::vec![3u64, 9], Some(9)),            // last one wins
        (alloc::vec![3u64, -1i64 as u64], Some(3)), // the sentinel keeps it
        (alloc::vec![0u64], Some(0)),               // fd 0 is a real fd
        (alloc::vec![-1i64 as u64], None),          // only the sentinel
    ] {
        let (scanned, staged) = both(&values);
        assert_eq!(scanned, want, "the fence scan read {:?} wrong", values);
        assert_eq!(
            staged, scanned,
            "the commit stages a different fence than it sleeps on",
        );
    }
}

/// `ucheck_n` multiplies a userspace count by a struct size. The bounds
/// above keep that far from overflowing, but the helper is the guard for
/// every nested array in this file, and some of those counts are not
/// bounded at all.
#[test]
fn a_count_that_overflows_its_byte_size_is_refused_not_wrapped() {
    let buf = [0u64; 4];
    let addr = buf.as_ptr() as usize;
    assert_eq!(ucheck_n::<u64>(addr, 4), Ok(()));
    // 2^61 u64s is 2^64 bytes: wrapping would make this a zero-length
    // range, which `user_range_ok` waves through.
    assert_eq!(
        ucheck_n::<u64>(addr, 1usize << 61),
        Err(FsError::InvalidParam),
    );
    assert_eq!(
        ucheck_n::<u64>(addr, usize::MAX),
        Err(FsError::InvalidParam)
    );
    // A zero-length range is fine from anywhere, a non-empty one is not
    // from a null pointer.
    assert_eq!(ucheck(0, 0), Ok(()));
    assert_eq!(ucheck(0, 1), Err(FsError::BadAddress));
}

/// The walk's own bounds are what keep the multiply above out of reach:
/// 64 objects x 64 properties x 8 bytes is nowhere near `usize`.
#[test]
fn the_walks_bounds_keep_the_span_far_from_overflowing() {
    let widest = 64usize * 64 * core::mem::size_of::<u64>();
    assert!(widest < 1 << 20, "the span a request can ask for grew");
}
