use {super::*, kernel_hal::MMUFlags, zircon_object::ipc::Socket, zircon_object::ipc::SocketFlags};

impl Syscall<'_> {
    /// Create a socket.
    ///
    /// Socket is a connected pair of bidirectional stream transports, that can move only data, and that have a maximum capacity.
    pub fn sys_socket_create(
        &self,
        options: u32,
        mut out0: UserOutPtr<HandleValue>,
        mut out1: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("socket.create: options={:#x?}", options);
        let (end0, end1) = Socket::create(options)?;
        let proc = self.thread.proc();
        install_handle_pair(
            proc,
            (
                Handle::new(end0, Rights::DEFAULT_SOCKET),
                Handle::new(end1, Rights::DEFAULT_SOCKET),
            ),
            (&mut out0, &mut out1),
        )
    }

    /// Write data to a socket.
    ///
    /// Attempts to write `count: usize` bytes to the socket specified by `handle_value`.
    pub fn sys_socket_write(
        &self,
        handle_value: HandleValue,
        options: u32,
        user_bytes: UserInPtr<u8>,
        count: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "socket.write: socket={:#x?}, options={:#x?}, buffer={:#x?}, size={:#x?}",
            handle_value, options, user_bytes, count,
        );
        if (count == 0 || !user_bytes.is_null()) && options == 0 {
            let proc = self.thread.proc();
            let socket = proc.get_object_with_rights::<Socket>(handle_value, Rights::WRITE)?;
            let write_size = socket.write_size(count)?;
            crate::user_memory::validate_user_range(
                proc,
                user_bytes.as_addr(),
                write_size,
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
            let actual_count = socket.write(user_bytes.as_slice(write_size)?)?;
            actual_count_ptr.write_if_not_null(actual_count)?;
            Ok(())
        } else {
            Err(ZxError::INVALID_ARGS)
        }
    }

    /// Read data from a socket.
    pub fn sys_socket_read(
        &self,
        handle_value: HandleValue,
        options: u32,
        mut user_bytes: UserOutPtr<u8>,
        count: usize,
        mut actual_count_ptr: UserOutPtr<usize>,
    ) -> ZxResult {
        info!(
            "socket.read: socket={:#x?}, options={:#x?}, buffer={:#x?}, size={:#x?}",
            handle_value, options, user_bytes, count,
        );
        if count > 0 && user_bytes.is_null() {
            return Err(ZxError::INVALID_ARGS);
        }
        let options = SocketFlags::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        if !(options - SocketFlags::SOCKET_PEEK).is_empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let socket = proc.get_object_with_rights::<Socket>(handle_value, Rights::READ)?;
        crate::user_memory::validate_user_range(
            proc,
            user_bytes.as_addr(),
            count,
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
        // Sized by the socket and not by `count`: see `Socket::read_buffer_len`.
        let mut data = vec![0; Socket::read_buffer_len(count)];
        let peek = options.contains(SocketFlags::SOCKET_PEEK);
        let actual_count = socket.read(peek, &mut data)?;
        user_bytes.write_array(&data[..actual_count])?;
        actual_count_ptr.write_if_not_null(actual_count)?;
        Ok(())
    }

    /// Change the write disposition of this endpoint and/or its peer.
    pub fn sys_socket_set_disposition(
        &self,
        handle: HandleValue,
        disposition: u32,
        peer_disposition: u32,
    ) -> ZxResult {
        const NONE: u32 = 0;
        const WRITE_DISABLED: u32 = 1;
        const WRITE_ENABLED: u32 = 2;
        if !matches!(disposition, NONE | WRITE_DISABLED | WRITE_ENABLED)
            || !matches!(peer_disposition, NONE | WRITE_DISABLED | WRITE_ENABLED)
        {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let socket = proc.get_object_with_rights::<Socket>(handle, Rights::MANAGE_SOCKET)?;
        let decode = |value| match value {
            WRITE_DISABLED => Some(true),
            WRITE_ENABLED => Some(false),
            _ => None,
        };
        socket.set_disposition(decode(disposition), decode(peer_disposition))
    }

    /// Prevent future reading or writing on a socket.
    pub fn sys_socket_shutdown(&self, socket: HandleValue, options: u32) -> ZxResult {
        let options = shutdown_options(options)?;
        info!(
            "socket.shutdown: socket={:#x?}, options={:#x?}",
            socket, options
        );
        let proc = self.thread.proc();
        let socket = proc.get_object_with_rights::<Socket>(socket, Rights::WRITE)?;
        let read = options.contains(SocketFlags::SHUTDOWN_READ);
        let write = options.contains(SocketFlags::SHUTDOWN_WRITE);
        socket.shutdown(read, write)?;
        Ok(())
    }
}

/// The `options` of `zx_socket_shutdown`, which are the two shutdown bits and
/// nothing else.
///
/// `from_bits_truncate` was quietly dropping every other bit, so a caller that
/// passed the wrong constant got a call that did something else instead of an
/// error -- and `SocketFlags` holds the bits of `socket_create` and
/// `socket_read` too, which are numbered over these.
fn shutdown_options(options: u32) -> ZxResult<SocketFlags> {
    let options = SocketFlags::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
    if !(options - SocketFlags::SHUTDOWN_MASK).is_empty() {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The two shutdown bits, in any combination, and nothing else: the flags
    /// of the other socket syscalls share this word's numbering, so accepting
    /// them here turns a caller's mistake into a different action.
    fn shutdown_takes_the_two_shutdown_bits_and_no_others() {
        let read = SocketFlags::SHUTDOWN_READ;
        let write = SocketFlags::SHUTDOWN_WRITE;
        assert_eq!(shutdown_options(0).unwrap(), SocketFlags::empty());
        assert_eq!(shutdown_options(read.bits()).unwrap(), read);
        assert_eq!(shutdown_options(write.bits()).unwrap(), write);
        assert_eq!(
            shutdown_options((read | write).bits()).unwrap(),
            read | write,
        );

        assert_eq!(
            shutdown_options(SocketFlags::SOCKET_PEEK.bits()).err(),
            Some(ZxError::INVALID_ARGS),
            "`peek` belongs to socket_read",
        );
        assert_eq!(
            shutdown_options(1 << 20).err(),
            Some(ZxError::INVALID_ARGS),
            "and a bit that is nothing at all",
        );
    }
}
