use {
    super::*,
    zircon_object::{debuglog::*, dev::*},
};

impl Syscall<'_> {
    /// Create a kernel managed debuglog reader or writer.
    pub fn sys_debuglog_create(
        &self,
        rsrc: HandleValue,
        options: u32,
        mut target: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "debuglog.create: resource_handle={:#x?}, options={:#x?}",
            rsrc, options,
        );
        let proc = self.thread.proc();
        if rsrc != 0 {
            proc.get_object::<Resource>(rsrc)?
                .validate_system(SystemResource::Debuglog)?;
        }
        let dlog = DebugLog::create(options);
        const FLAG_READABLE: u32 = 0x4000_0000u32;
        let dlog_right = if options & FLAG_READABLE == 0 {
            Rights::DEFAULT_DEBUGLOG
        } else {
            Rights::DEFAULT_DEBUGLOG | Rights::READ
        };
        let dlog_handle = proc.add_handle(Handle::new(dlog, dlog_right));
        target.write(dlog_handle)?;
        Ok(())
    }

    /// Write log entry to debuglog.
    pub fn sys_debuglog_write(
        &self,
        handle_value: HandleValue,
        options: u32,
        buf: UserInPtr<u8>,
        len: usize,
    ) -> ZxResult {
        info!(
            "debuglog.write: handle={:#x?}, options={:#x?}, buf=({:#x?}; {:#x?})",
            handle_value, options, buf, len,
        );
        const LOG_FLAGS_MASK: u32 = 0x10;
        if options & !LOG_FLAGS_MASK != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        // The record takes bytes, not text: a line cut at `DLOG_MAX_DATA` in
        // the middle of a multi-byte character, or one with a stray byte,
        // used to be refused whole as invalid UTF-8.
        let datalen = len.min(DLOG_MAX_DATA);
        let data = buf.as_slice(datalen)?;
        let proc = self.thread.proc();
        let dlog = proc.get_object_with_rights::<DebugLog>(handle_value, Rights::WRITE)?;
        dlog.write(Severity::Info, options, self.thread.id(), proc.id(), data);
        // print to kernel console
        kernel_hal::console::console_write_str(&alloc::string::String::from_utf8_lossy(data));
        if data.last() != Some(&b'\n') {
            kernel_hal::console::console_write_str("\n");
        }
        Ok(())
    }

    /// Read log entries from debuglog.
    ///
    /// Answers with the number of bytes read, which `zx_debuglog_read` returns
    /// as a positive status. It used to be smuggled out as
    /// `Err(transmute::<u32, ZxError>(len))`: a `ZxError` holding a value that
    /// is not one of its variants, which is undefined behaviour the moment
    /// anything looks at it -- and the dispatcher's own `info!` line formats
    /// every result with `{:?}`.
    pub fn sys_debuglog_read(
        &self,
        handle_value: HandleValue,
        options: u32,
        mut buf: UserOutPtr<u8>,
        len: usize,
    ) -> ZxResult<usize> {
        info!(
            "debuglog.read: handle={:#x?}, options={:#x?}, buf=({:#x?}; {:#x?})",
            handle_value, options, buf, len,
        );
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        let mut buffer = [0; DLOG_MAX_LEN];
        let dlog = proc.get_object_with_rights::<DebugLog>(handle_value, Rights::READ)?;
        let actual_len = dlog.read(&mut buffer).min(len);
        if actual_len == 0 {
            return Err(ZxError::SHOULD_WAIT);
        }
        buf.write_array(&buffer[..actual_len])?;
        Ok(actual_len)
    }
}
