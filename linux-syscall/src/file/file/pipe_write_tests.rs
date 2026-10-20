//! The two decisions `write(2)` makes about a pipe or a unix socket
//! that Linux makes and this kernel did not: a blocking writer waits
//! for room, and a writer with no reader gets `SIGPIPE` with its
//! `EPIPE`.

use super::{after_write_error, grows_past_fsize, sigpipe_due, AfterWriteError, LxError};

#[test]
fn the_calls_that_write_a_file_send_sigxfsz_with_efbig() {
    use crate::Sys;
    for sys in [Sys::WRITE, Sys::PWRITE64, Sys::WRITEV, Sys::PWRITEV] {
        assert!(super::writes_a_file(&sys), "{:?}", sys);
    }
    // `truncate` sends its own (`check_fsize_growth`), and a read none.
    for sys in [Sys::READ, Sys::FTRUNCATE, Sys::TRUNCATE, Sys::IOCTL] {
        assert!(!super::writes_a_file(&sys), "{:?}", sys);
    }
}

#[test]
fn only_growing_a_file_past_its_limit_breaks_rlimit_fsize() {
    assert!(grows_past_fsize(4096, 0, 8192));
    assert!(grows_past_fsize(4096, 4096, 4097));
    assert!(!grows_past_fsize(4096, 0, 4096), "up to the limit is fine");
    assert!(
        !grows_past_fsize(4096, 8192, 6000),
        "cutting a file down is fine, whatever is left"
    );
    assert!(!grows_past_fsize(4096, 8192, 8192));
    assert!(!grows_past_fsize(
        linux_object::process::RLIM_INFINITY,
        0,
        u64::MAX
    ));
}

#[test]
fn a_blocking_pipe_writer_waits_on_eagain_wherever_it_is() {
    // Before or after some bytes went in: `pipe_write` blocks until every
    // byte is written, so neither a partial count nor EAGAIN comes back.
    assert_eq!(
        after_write_error(LxError::EAGAIN, true, 0),
        AfterWriteError::Wait
    );
    assert_eq!(
        after_write_error(LxError::EAGAIN, true, 4096),
        AfterWriteError::Wait
    );
}

#[test]
fn a_non_blocking_pipe_and_every_other_fd_keep_the_partial_write_rule() {
    // `O_NONBLOCK` on the pipe: EAGAIN is the answer when nothing went
    // in, and the count when something did.
    assert_eq!(
        after_write_error(LxError::EAGAIN, false, 0),
        AfterWriteError::Fail
    );
    assert_eq!(
        after_write_error(LxError::EAGAIN, false, 100),
        AfterWriteError::Partial
    );
    // Only EAGAIN is a reason to wait: a broken pipe fails at once even
    // for a blocking writer, and after a partial write it is reported
    // next time.
    assert_eq!(
        after_write_error(LxError::EPIPE, true, 0),
        AfterWriteError::Fail
    );
    assert_eq!(
        after_write_error(LxError::EPIPE, true, 8),
        AfterWriteError::Partial
    );
    assert_eq!(
        after_write_error(LxError::EBADF, true, 0),
        AfterWriteError::Fail
    );
}

#[test]
fn sigpipe_comes_with_epipe_from_a_pipe_or_socket_and_nothing_else() {
    assert!(sigpipe_due(LxError::EPIPE, true));
    // ALSA's EPIPE is an underrun report, not a broken pipe: a player
    // that hits an XRUN recovers, it must not be killed.
    assert!(!sigpipe_due(LxError::EPIPE, false));
    assert!(!sigpipe_due(LxError::EAGAIN, true));
    assert!(!sigpipe_due(LxError::EBADF, true));
}

#[test]
fn a_single_byte_already_out_is_still_a_partial_write() {
    // The boundary of "any bytes at all", which is one. `write(2)` reports
    // that byte; a caller told `EAGAIN` instead has no count at all, so it
    // resends from the same offset and the peer receives the byte twice --
    // the bug the whole partial-write rule exists to stop. Every case
    // below it used to be measured with 0 or with a comfortable 100.
    assert_eq!(
        after_write_error(LxError::EAGAIN, false, 1),
        AfterWriteError::Partial
    );
    assert_eq!(
        after_write_error(LxError::EPIPE, false, 1),
        AfterWriteError::Partial
    );
    // And nothing out is still the error itself.
    assert_eq!(
        after_write_error(LxError::EAGAIN, false, 0),
        AfterWriteError::Fail
    );
}
