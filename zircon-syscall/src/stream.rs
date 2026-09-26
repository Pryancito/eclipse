use {
    super::*,
    bitflags::bitflags,
    kernel_hal::MMUFlags,
    zircon_object::{task::Process, vm::*},
};

fn read_iovecs<P: kernel_hal::user::Policy>(
    proc: &Process,
    vector: UserInPtr<kernel_hal::user::IoVec<P>>,
    vector_size: usize,
) -> ZxResult<kernel_hal::user::IoVecs<P>> {
    crate::user_memory::validate_user_range(
        proc,
        vector.as_addr(),
        vector_size
            .checked_mul(core::mem::size_of::<kernel_hal::user::IoVec<P>>())
            .ok_or(ZxError::INVALID_ARGS)?,
        MMUFlags::READ,
    )?;
    Ok(vector.read_iovecs(vector_size)?)
}

fn validate_iovec_buffers<P: kernel_hal::user::Policy>(
    proc: &Process,
    iovecs: &kernel_hal::user::IoVecs<P>,
    access: MMUFlags,
) -> ZxResult {
    for iovec in iovecs.iter() {
        iovec
            .addr()
            .checked_add(iovec.len())
            .ok_or(ZxError::NOT_FOUND)?;
        proc.vmar()
            .check_user_range(iovec.addr(), iovec.len(), access)
            .map_err(|_| ZxError::NOT_FOUND)?;
    }
    Ok(())
}

/// What a gather does with a write the VMO refused for lack of room once
/// earlier iovecs went in: nothing, since `zx_stream_writev` answers the
/// bytes it wrote and a caller retries from there. It used to answer
/// `OUT_OF_RANGE` for the whole call after the VMO filled part-way through
/// the vector, so the bytes already written were reported as not written.
/// A refusal before anything went in, or for any other reason, is still the
/// caller's error.
fn stream_full(error: ZxError, written: usize) -> ZxResult {
    if error == ZxError::OUT_OF_RANGE && written > 0 {
        Ok(())
    } else {
        Err(error)
    }
}

/// Feed the iovecs of a gather to `write` one by one and answer the bytes
/// that went in. A write the VMO cut short (it ran out of room part-way
/// through an iovec) ends the gather there, with what was written so far:
/// the bytes after it have no room either, and `writev_at` used to advance
/// its offset by the iovec's full length past such a cut, so the next iovec
/// would have started beyond the bytes actually written. A refusal is
/// `stream_full`'s to judge.
///
/// `write` gets each iovec's bytes and the bytes written before it, which is
/// the distance from the gather's starting offset.
fn write_gather<'a>(
    iovecs: impl Iterator<Item = ZxResult<&'a [u8]>>,
    mut write: impl FnMut(&[u8], usize) -> ZxResult<usize>,
) -> ZxResult<usize> {
    let mut written = 0;
    for data in iovecs {
        let data = data?;
        let count = match write(data, written) {
            Ok(count) => count,
            Err(error) => {
                stream_full(error, written)?;
                break;
            }
        };
        written += count;
        if count < data.len() {
            break;
        }
    }
    Ok(written)
}

impl Syscall<'_> {
    /// Create a stream from a VMO.
    ///   
    /// Stream for reads and writes the data in an underlying VMO.  
    pub fn sys_stream_create(
        &self,
        options: u32,
        vmo_handle: HandleValue,
        seek: usize,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "stream.create: options={:#x?}, vmo_handle={:#x?}, seek={:#x?}",
            options, vmo_handle, seek
        );
        let options = StreamOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        let mut rights = Rights::DEFAULT_STREAM;
        let mut vmo_rights = Rights::empty();
        if options.contains(StreamOptions::MODE_READ) {
            rights |= Rights::READ;
            vmo_rights |= Rights::READ;
        }
        if options.contains(StreamOptions::MODE_WRITE) {
            rights |= Rights::WRITE;
            vmo_rights |= Rights::WRITE;
        }
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(vmo_handle, vmo_rights)?;
        let stream = Stream::create(vmo, seek, options.bits());
        install_handle(proc, Handle::new(stream, rights), &mut out)
    }

    /// Write data to a stream at the current seek offset.   
    pub fn sys_stream_writev(
        &self,
        handle_value: HandleValue,
        options: u32,
        vector: UserInPtr<IoVecIn>,
        vector_size: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "stream.write: stream={:#x?}, options={:#x?}, vector=({:#x?}; {:#x?})",
            handle_value, options, vector, vector_size,
        );
        bitflags! {
            struct WriteOptions: u32 {
                const APPEND = 1;
            }
        }
        let options = WriteOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        let proc = self.thread.proc();
        let stream = proc.get_object_with_rights::<Stream>(handle_value, Rights::WRITE)?;
        let data = read_iovecs(proc, vector, vector_size)?;
        stream.check_write_size(
            data.total_len(),
            options.contains(WriteOptions::APPEND),
            None,
        )?;
        validate_iovec_buffers(proc, &data, MMUFlags::READ)?;
        let append = options.contains(WriteOptions::APPEND);
        let actual_count = write_gather(
            data.iter()
                .map(|io_vec| io_vec.as_slice().map_err(ZxError::from)),
            |bytes, _| stream.write(bytes, append),
        )?;
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Write data to a stream at the given offset.   
    pub fn sys_stream_writev_at(
        &self,
        handle_value: HandleValue,
        options: u32,
        offset: usize,
        vector: UserInPtr<IoVecIn>,
        vector_size: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "stream.write_at: stream={:#x?}, options={:#x?}, offset={:#x?}, vector=({:#x?}; {:#x?})",
            handle_value, options, offset, vector, vector_size,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let stream = proc.get_object_with_rights::<Stream>(handle_value, Rights::WRITE)?;
        let data = read_iovecs(proc, vector, vector_size)?;
        stream.check_write_size(data.total_len(), false, Some(offset))?;
        validate_iovec_buffers(proc, &data, MMUFlags::READ)?;
        // Each iovec goes right after the bytes written before it, not after
        // the bytes asked for: they differ once the VMO cut a write short.
        let actual_count = write_gather(
            data.iter()
                .map(|io_vec| io_vec.as_slice().map_err(ZxError::from)),
            |bytes, written| stream.write_at(bytes, offset + written),
        )?;
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Read data from a stream at the current seek offset.   
    pub fn sys_stream_readv(
        &self,
        handle_value: HandleValue,
        options: u32,
        vector: UserInPtr<IoVecOut>,
        vector_size: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "stream.read: stream={:#x?}, options={:#x?}, vector=({:#x?}; {:#x?})",
            handle_value, options, vector, vector_size,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let stream = proc.get_object_with_rights::<Stream>(handle_value, Rights::READ)?;
        let mut data = read_iovecs(proc, vector, vector_size)?;
        validate_iovec_buffers(proc, &data, MMUFlags::WRITE)?;
        let mut actual_count = 0usize;
        for io_vec in data.iter_mut() {
            actual_count += stream.read(io_vec.as_mut_slice()?)?;
        }
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Read data from a stream at the given offset.   
    pub fn sys_stream_readv_at(
        &self,
        handle_value: HandleValue,
        options: u32,
        mut offset: usize,
        vector: UserInPtr<IoVecOut>,
        vector_size: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "stream.read_at: stream={:#x?}, options={:#x?}, offset={:#x?}, vector=({:#x?}; {:#x?})",
            handle_value, options, offset, vector, vector_size,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let stream = proc.get_object_with_rights::<Stream>(handle_value, Rights::READ)?;
        let mut data = read_iovecs(proc, vector, vector_size)?;
        validate_iovec_buffers(proc, &data, MMUFlags::WRITE)?;
        let mut actual_count = 0usize;
        for io_vec in data.iter_mut() {
            actual_count += stream.read_at(io_vec.as_mut_slice()?, offset)?;
            offset += io_vec.len();
        }
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Modify the seek offset.  
    ///   
    /// Sets the seek offset of the stream to `offset` relative to `whence`.  
    pub fn sys_stream_seek(
        &self,
        handle_value: HandleValue,
        whence: usize,
        offset: isize,
        mut out_seek: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "stream.seek: stream={:#x?}, whence={:#x?}, offset={:#x?}",
            handle_value, whence, offset,
        );
        let proc = self.thread.proc();
        let (stream, rights) = proc.get_object_and_rights::<Stream>(handle_value)?;
        if !rights.contains(Rights::READ) && !rights.contains(Rights::WRITE) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let whence = SeekOrigin::try_from(whence).map_err(|_| ZxError::INVALID_ARGS)?;
        let new_seek = stream.seek(whence, offset)?;
        out_seek.write_if_not_null(new_seek)?;
        Ok(())
    }
}

#[cfg(test)]
mod write_gather_tests {
    //! `zx_stream_writev_at` advanced its offset by each iovec's full length,
    //! not by what the VMO took: after a write the VMO cut short, the next
    //! iovec would have been placed past the cut. Both gathers share one
    //! loop now, and it stops at the first short write.

    use super::*;
    use alloc::vec::Vec;

    fn iovecs<'a>(parts: &'a [&'a [u8]]) -> impl Iterator<Item = ZxResult<&'a [u8]>> {
        parts.iter().map(|part| Ok(*part))
    }

    /// A VMO of `room` bytes: each write takes what fits after `written`
    /// and refuses with `OUT_OF_RANGE` once nothing does. Records where
    /// each write was asked to go and how much went in, a refused one as
    /// 0 bytes.
    fn vmo_of(
        room: usize,
        placed: &mut Vec<(usize, usize)>,
    ) -> impl FnMut(&[u8], usize) -> ZxResult<usize> + '_ {
        move |bytes, written| {
            let count = bytes.len().min(room.saturating_sub(written));
            placed.push((written, count));
            if count == 0 {
                return Err(ZxError::OUT_OF_RANGE);
            }
            Ok(count)
        }
    }

    #[test]
    fn every_iovec_goes_right_after_the_bytes_the_one_before_it_wrote() {
        let mut placed = Vec::new();
        let got = write_gather(
            iovecs(&[&[1; 30], &[2; 30], &[3; 30]]),
            vmo_of(100, &mut placed),
        );
        assert_eq!(got, Ok(90));
        assert_eq!(placed, [(0, 30), (30, 30), (60, 30)]);
    }

    #[test]
    fn a_write_the_vmo_cut_short_ends_the_gather_with_what_went_in() {
        let mut placed = Vec::new();
        let got = write_gather(
            iovecs(&[&[1; 30], &[2; 30], &[3; 30]]),
            vmo_of(45, &mut placed),
        );
        assert_eq!(got, Ok(45));
        // The third iovec is never offered to the VMO: it would have been
        // asked to go at 60, past the 45 bytes that exist.
        assert_eq!(placed, [(0, 30), (30, 15)]);
    }

    #[test]
    fn a_refusal_after_some_bytes_is_a_short_answer_and_before_any_an_error() {
        let mut placed = Vec::new();
        let got = write_gather(iovecs(&[&[1; 30], &[2; 30]]), vmo_of(30, &mut placed));
        assert_eq!(got, Ok(30));
        assert_eq!(placed, [(0, 30), (30, 0)]);
        let got = write_gather(iovecs(&[&[1; 30]]), vmo_of(0, &mut placed));
        assert_eq!(got, Err(ZxError::OUT_OF_RANGE));
        let got = write_gather(iovecs(&[&[1; 30]]), |_, _| Err(ZxError::BAD_STATE));
        assert_eq!(got, Err(ZxError::BAD_STATE));
    }

    #[test]
    fn an_iovec_that_cannot_be_read_is_the_caller_s_error() {
        let parts: Vec<ZxResult<&[u8]>> = alloc::vec![Ok(&[1; 4]), Err(ZxError::INVALID_ARGS)];
        let got = write_gather(parts.into_iter(), |bytes, _| Ok(bytes.len()));
        assert_eq!(got, Err(ZxError::INVALID_ARGS));
    }
}
