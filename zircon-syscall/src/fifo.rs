use {super::*, kernel_hal::MMUFlags, zircon_object::ipc::Fifo};

impl Syscall<'_> {
    /// Creates a fifo, which is actually a pair of fifos of `elem_count` entries of `elem_size` bytes.
    pub fn sys_fifo_create(
        &self,
        elem_count: usize,
        elem_size: usize,
        options: u32,
        mut out0: UserOutPtr<HandleValue>,
        mut out1: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "fifo.create: count={:#x}, item_size={:#x}, options={:#x}",
            elem_count, elem_size, options,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        if elem_count == 0
            || elem_size == 0
            || elem_count
                .checked_mul(elem_size)
                .is_none_or(|size| size > 4096)
        {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let (end0, end1) = Fifo::create(elem_count, elem_size);
        let proc = self.thread.proc();
        install_handle_pair(
            proc,
            (
                Handle::new(end0, Rights::DEFAULT_FIFO),
                Handle::new(end1, Rights::DEFAULT_FIFO),
            ),
            (&mut out0, &mut out1),
        )
    }

    /// Write data to a fifo.
    pub fn sys_fifo_write(
        &self,
        handle_value: HandleValue,
        elem_size: usize,
        user_bytes: UserInPtr<u8>,
        count: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "fifo.write: handle={:?}, item_size={}, count={:#x}",
            handle_value, elem_size, count
        );
        let proc = self.thread.proc();
        // Validate the handle before rejecting count==0 so BAD_HANDLE/WRONG_TYPE
        // are not masked by OUT_OF_RANGE.
        let fifo = proc.get_object_with_rights::<Fifo>(handle_value, Rights::WRITE)?;
        if count == 0 {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let byte_count = count.checked_mul(elem_size).ok_or(ZxError::INVALID_ARGS)?;
        crate::user_memory::validate_user_range(
            proc,
            user_bytes.as_addr(),
            byte_count,
            MMUFlags::READ,
        )?;
        if !actual_count_ptr.is_null() {
            crate::user_memory::validate_user_range(
                proc,
                actual_count_ptr.as_addr(),
                core::mem::size_of::<usize>(),
                MMUFlags::WRITE,
            )?;
        }
        let data = user_bytes.as_slice(byte_count)?;
        let actual_count = fifo.write(elem_size, data, count)?;
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Read data from a fifo.
    pub fn sys_fifo_read(
        &self,
        handle_value: HandleValue,
        elem_size: usize,
        mut user_bytes: UserOutPtr<u8>,
        count: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "fifo.read: handle={:?}, item_size={}, count={:#x}",
            handle_value, elem_size, count
        );
        let proc = self.thread.proc();
        // Handle first: count==0 used to hide BAD_HANDLE / WRONG_TYPE.
        let fifo = proc.get_object_with_rights::<Fifo>(handle_value, Rights::READ)?;
        if count == 0 {
            return Err(ZxError::OUT_OF_RANGE);
        }
        let byte_count = count.checked_mul(elem_size).ok_or(ZxError::INVALID_ARGS)?;
        crate::user_memory::validate_user_range(
            proc,
            user_bytes.as_addr(),
            byte_count,
            MMUFlags::WRITE,
        )?;
        if !actual_count_ptr.is_null() {
            crate::user_memory::validate_user_range(
                proc,
                actual_count_ptr.as_addr(),
                core::mem::size_of::<usize>(),
                MMUFlags::WRITE,
            )?;
        }
        // The buffer is sized by the fifo, not by `count`: see
        // `Fifo::read_buffer_elems`. Clamping cannot change the answer, since a
        // read never returns more elements than the fifo holds.
        let count = fifo.read_buffer_elems(elem_size, count)?;
        // The product cannot overflow: `byte_count` above is the same one with a
        // count at least this large, and it was checked. Checked again all the
        // same, so nobody reading this has to go and prove it.
        let data_len = count.checked_mul(elem_size).ok_or(ZxError::INVALID_ARGS)?;
        // TODO: uninit buffer
        let mut data = vec![0; data_len];
        let actual_count = fifo.read(elem_size, &mut data, count)?;
        // Checked for the same reason as `data_len`, which this can only be at
        // most: a read never returns more elements than it was given room for.
        let actual_len = actual_count
            .checked_mul(elem_size)
            .ok_or(ZxError::INVALID_ARGS)?;
        actual_count_ptr.write_if_not_null(actual_count)?;
        user_bytes.write_array(&data[..actual_len])?;
        Ok(())
    }
}
