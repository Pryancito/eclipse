use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// `_IOWR('d', DRM_ECLIPSE_COMPUTE_NR, size)` -- the command word a client
/// builds, with the encoded size under the test's control.
fn compute_cmd(size: u32) -> u32 {
    IOC_WRITE_DIR | IOC_READ_DIR | (size << 16) | (b'd' as u32) << 8 | DRM_ECLIPSE_COMPUTE_NR
}

/// A request with recognisable contents, so "was not written" is a real
/// assertion rather than "happens to be zero".
fn blank_request() -> DrmEclipseCompute {
    DrmEclipseCompute {
        op: 0,
        status: 0x5A5A_5A5A,
        elapsed_ns: 0x5A5A_5A5A_5A5A_5A5A,
        grid_threads: 0x5A5A_5A5A,
        reserved: 0,
        summary: [0x5A; 512],
    }
}

fn summary_str(req: &DrmEclipseCompute) -> alloc::string::String {
    let end = req.summary.iter().position(|&b| b == 0).unwrap_or(0);
    alloc::string::String::from_utf8_lossy(&req.summary[..end]).into_owned()
}

/// A client that encodes a struct smaller than the kernel's is refused
/// outright. The handler writes 536 bytes at the address it is given, and
/// the only bound anything checked was the size in the client's own command
/// word, so accepting a short encoding is half a kilobyte written past the
/// end of the caller's buffer -- from an unprivileged ioctl.
#[test]
fn a_short_size_encoding_is_refused_instead_of_writing_past_the_caller() {
    let _screen = kms_emu::headless();
    let c = Client::open(0);
    let mut req = blank_request();

    let err = c
        .ioctl(compute_cmd(8), &mut req)
        .expect_err("a short encoding must not reach the handler");
    assert_eq!(err, FsError::InvalidParam);
    // And it was refused BEFORE anything was written.
    assert_eq!(req.status, 0x5A5A_5A5A, "the handler ran anyway");
    assert!(
        req.summary.iter().all(|&b| b == 0x5A),
        "the summary was written"
    );
}

/// One byte short is still short. The floor is `<`, and an off-by-one here
/// is the whole bug it exists to prevent.
#[test]
fn an_encoding_one_byte_short_is_still_refused() {
    let _screen = kms_emu::headless();
    let c = Client::open(0);
    let mut req = blank_request();
    let exact = core::mem::size_of::<DrmEclipseCompute>() as u32;

    assert_eq!(
        c.ioctl(compute_cmd(exact - 1), &mut req),
        Err(FsError::InvalidParam)
    );
    // The exact size is accepted, so the refusal above is the size check
    // and not the command being unknown.
    assert_eq!(c.ioctl(compute_cmd(exact), &mut req), Ok(0));
}

/// With no GPU at all the answer is in-band: the ioctl SUCCEEDS and the
/// status field carries `-ENODEV`. Returning an ioctl error instead would
/// be indistinguishable from "this kernel has no compute ioctl", which is
/// what a probing client falls back on.
#[test]
fn a_node_with_no_gpu_answers_enodev_in_band_not_with_an_ioctl_error() {
    let _screen = kms_emu::headless();
    let c = Client::open(0);
    let mut req = blank_request();

    assert_eq!(
        c.ioctl(
            compute_cmd(core::mem::size_of::<DrmEclipseCompute>() as u32),
            &mut req
        ),
        Ok(0)
    );
    assert_eq!(
        req.status, -19,
        "-ENODEV belongs in the reply, not in errno"
    );
    assert_eq!(summary_str(&req), "no compute GPU");
}

/// With a driver present the driver's own answer is passed through --
/// including its refusal. `EmuGpu` does not implement `compute_launch`, so
/// it gives the trait default, which is exactly what a driver that has not
/// wired compute up returns on real hardware.
#[test]
fn a_driver_without_compute_support_answers_with_its_own_status() {
    let screen = kms_emu::headless();
    let _gpu = screen.attach_gpu(EmuGpu::new("emu-gpu"));
    let c = Client::open(0);
    let mut req = blank_request();

    assert_eq!(
        c.ioctl(
            compute_cmd(core::mem::size_of::<DrmEclipseCompute>() as u32),
            &mut req
        ),
        Ok(0)
    );
    assert_eq!(
        req.status, -38,
        "-ENOSYS from the driver, not the core's -ENODEV"
    );
    assert_ne!(summary_str(&req), "no compute GPU");
    assert!(
        !summary_str(&req).is_empty(),
        "the driver's report was dropped"
    );
    // The reply's other fields are always written, so a client cannot read
    // a previous launch's numbers.
    assert_eq!(req.elapsed_ns, 0);
    assert_eq!(req.grid_threads, 0);
}

/// The summary is always NUL-terminated, even when the driver's report is
/// longer than the field. It is read by C as a string, so a report that
/// filled all 512 bytes would run off the end of the struct.
#[test]
fn the_summary_is_nul_terminated_even_when_the_report_overflows_it() {
    let mut dst = [0xFFu8; 512];
    let long = alloc::string::String::from_utf8(alloc::vec![b'x'; 600]).unwrap();
    fill_summary(&mut dst, &long);
    assert_eq!(dst[511], 0, "no room left for the terminator");
    assert!(dst[..511].iter().all(|&b| b == b'x'));

    // And a short report clears what a previous, longer one left behind.
    fill_summary(&mut dst, "ok");
    assert_eq!(&dst[..3], b"ok\0");
    assert!(dst[3..].iter().all(|&b| b == 0), "stale bytes survived");
}
