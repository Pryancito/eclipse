use super::*;
use zircon_object::dev::*;

/// The most one `zx_debug_read` will take from the console in a single call.
///
/// The caller learns how much it actually got from `actual`, and reads again for
/// the rest.
const DEBUG_READ_MAX: usize = 4096;

impl Syscall<'_> {
    /// Write debug info to the serial port.
    pub fn sys_debug_write(&self, buf: UserInPtr<u8>, len: usize) -> ZxResult {
        info!("debug.write: buf=({:?}; {:#x})", buf, len);
        kernel_hal::console::console_write_str(buf.as_str(len)?);
        Ok(())
    }

    /// Read debug info from the serial port.
    pub async fn sys_debug_read(
        &self,
        handle: HandleValue,
        mut buf: UserOutPtr<u8>,
        buf_size: u32,
        mut actual: UserOutPtr<u32>,
    ) -> ZxResult {
        info!(
            "debug.read: handle={:#x}, buf=({:?}; {:#x})",
            handle, buf, buf_size
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(handle)?
            .validate(ResourceKind::ROOT)?;
        // Bounded, and not `buf_size` as the caller gave it: a `u32` is up to
        // four gigabytes of kernel heap, and an allocation the heap cannot
        // serve is a panic and not an error. A short read is what the caller is
        // already told through `actual`.
        let mut vec = vec![0u8; (buf_size as usize).min(DEBUG_READ_MAX)];
        let len = kernel_hal::console::console_read(&mut vec).await;
        buf.write_array(&vec[..len])?;
        actual.write(len as u32)?;
        Ok(())
    }
}
