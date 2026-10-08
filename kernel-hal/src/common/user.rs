// 来自用户空间的裸指针
//! Raw pointer from user land.

use crate::VirtAddr;
use alloc::{string::String, vec::Vec};
use core::{
    fmt::{Debug, Formatter},
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

/// DEBUG: escanea los bytes que el kernel está a punto de copiar a memoria de
/// usuario buscando el patrón LE `00 80 ff ff` (= la mitad alta `0xffff8000` de
/// un puntero physmap del kernel). Si aparece, el kernel está FILTRANDO un
/// puntero de kernel a usuario — la causa de que apk acabe con `rbp =
/// 0xffff8000_xxxxxxxx`. Escaneo acotado a 4 KiB para no frenar copias grandes.
///
/// Gated behind the `uleak-scan` feature: the scan is a byte-stride pass over
/// up to 4 KiB on the return path of EVERY data-returning syscall (read,
/// readv, getdents64, fstat, recvfrom, ...), which is far too expensive to
/// leave in the default build now that the original leak is fixed. Re-enable
/// the feature to chase a regression.
#[cfg(all(not(feature = "libos"), feature = "uleak-scan"))]
fn dbg_scan_physmap_leak(bytes: &[u8], dst: usize, who: &str) {
    let n = bytes.len().min(4096);
    let mut i = 0;
    while i + 4 <= n {
        if bytes[i] == 0x00 && bytes[i + 1] == 0x80 && bytes[i + 2] == 0xff && bytes[i + 3] == 0xff
        {
            warn!(
                "[uleak] physmap-high 0xffff8000 -> usuario dst={:#x} off={} via={} len={}",
                dst,
                i,
                who,
                bytes.len()
            );
            return;
        }
        i += 1;
    }
}

// 来自用户空间的裸指针
/// Raw pointer from user land.
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct UserPtr<T, P: Policy>(*mut T, PhantomData<P>);

// 标识用户指针功能的基特征。
/// Base trait for Markers of user pointer policy.
pub trait Policy {}

// 标记一个用于输入的指针。
/// Marks a pointer used to read.
pub trait Read: Policy {}

// 标记一个用于输出的指针。
/// Marks a pointer used to write.
pub trait Write: Policy {}

// 输入指针的类型参数。
/// Type argument for user pointer used to read.
pub struct In;

// 输出指针的类型参数。
/// Type argument for user pointer used to write.
pub struct Out;

// 既用于输入有用于输出的指针的类型参数。
/// Type argument for user pointer used to both read and write.
pub struct InOut;

impl Policy for In {}
impl Policy for Out {}
impl Policy for InOut {}
impl Read for In {}
impl Write for Out {}
impl Read for InOut {}
impl Write for InOut {}

pub type UserInPtr<T> = UserPtr<T, In>;
pub type UserOutPtr<T> = UserPtr<T, Out>;
pub type UserInOutPtr<T> = UserPtr<T, InOut>;

// 用户指针操作的异常类型。
/// The error type which is returned from user pointer operation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidUtf8,
    InvalidPointer,
    BufferTooSmall,
    InvalidLength,
    InvalidVectorAddress,
}

// 本模块用到的只是用户指针操作结果的类型。
type Result<T> = core::result::Result<T, Error>;

impl<T, P: Policy> Debug for UserPtr<T, P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        // 打印用户指针就是打印裸指针
        write!(f, "{:?}", self.0)
    }
}

/// First address above the user half. Canonical x86_64 splits at
/// `0x0000_8000_0000_0000`, and Sv39/Sv48/aarch64 user ranges all sit below it,
/// so one constant covers every bare-metal target.
const USER_MAX: usize = 0x0000_8000_0000_0000;

/// Whether `[addr, addr + bytes)` fits below `max` without wrapping.
///
/// Split out of [`in_user_half`] so the arithmetic — which is the part that
/// can regress — is compiled and unit-tested on the host, where the only
/// buildable configuration is `libos` and [`in_user_half`] itself is a
/// constant `true`.
#[inline]
fn range_within(addr: usize, bytes: usize, max: usize) -> bool {
    match addr.checked_add(bytes) {
        Some(end) => end <= max,
        None => false,
    }
}

/// Whether `[addr, addr + bytes)` lies entirely in the user half.
///
/// The kernel is mapped into EVERY address space, so a pointer that arrived
/// from userspace naming a kernel address is not a fault waiting to happen —
/// it resolves, and the copy lands in kernel memory. `check()` previously
/// tested only null and alignment, which made every syscall out-pointer an
/// arbitrary kernel-memory write and every in-pointer an arbitrary kernel-memory
/// read. Linux rejects these with EFAULT via `access_ok()`; so do we.
///
/// `libos` builds run in a host process where "user" addresses are ordinary
/// host addresses, so the bound does not apply there.
#[inline]
fn in_user_half(addr: usize, bytes: usize) -> bool {
    // `cfg!` rather than `#[cfg]` so the bare-metal arm is type-checked in
    // every configuration -- including the only one that builds on a
    // developer's host and in the unit-test job, which is `libos`. The
    // constant makes it fold away there.
    in_user_half_with(cfg!(feature = "libos"), addr, bytes)
}

/// [`in_user_half`] with the exemption as an argument rather than a `cfg!`.
///
/// The exemption is the whole decision, and the only build this suite can
/// compile is the one that takes it: with the flag folded in, every host test
/// sees a constant `true` and nothing can tell an exemption from a bound that
/// simply never fires. Passed in, both answers are reachable from a test.
#[inline]
fn in_user_half_with(libos: bool, addr: usize, bytes: usize) -> bool {
    libos || range_within(addr, bytes, USER_MAX)
}

/// `access_ok()` for a raw `(addr, bytes)` pair that a device ioctl is about
/// to dereference directly: non-null when `bytes > 0`, and entirely inside
/// the user half (see [`in_user_half`]). Device `io_control` handlers receive
/// the ioctl argument as a bare `usize` and used to cast it straight to a
/// `&mut T`, which made every ioctl an arbitrary kernel-memory read/write for
/// a caller that passed a kernel address (and a kernel #PF for a NULL one).
/// Alignment is deliberately not required here: userspace structs arrive
/// however libdrm laid them out, and the callers read them with plain loads
/// on x86_64 where unaligned access is legal.
pub fn user_range_ok(addr: usize, bytes: usize) -> bool {
    if bytes == 0 {
        return true;
    }
    addr != 0 && in_user_half(addr, bytes)
}

// FIXME: this is a workaround for `clear_child_tid`.
unsafe impl<T, P: Policy> Send for UserPtr<T, P> {}
unsafe impl<T, P: Policy> Sync for UserPtr<T, P> {}

impl<T, P: Policy> From<usize> for UserPtr<T, P> {
    fn from(ptr: usize) -> Self {
        UserPtr(ptr as _, PhantomData)
    }
}

impl<T, P: Policy> UserPtr<T, P> {
    // 检查 `size` 是否足够放下一个 `T` 的值，
    // 并从 `addr` 构造一个用户指针。
    /// Checks if `size` is enough to save a value of `T`,
    /// then constructs a user pointer from its value `addr`.
    pub fn from_addr_size(addr: usize, size: usize) -> Result<Self> {
        if size >= core::mem::size_of::<T>() {
            Ok(Self::from(addr))
        } else {
            Err(Error::BufferTooSmall)
        }
    }

    // 如果指针为空，返回 `true`。
    /// Returns `true` if the pointer is null.
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    // 偏移指针。
    // `count` 表示 `T` 的数量；
    // 例如，`count` 为 3 表示将指针移动 `3 * size_of::<T>()` 个字节。
    /// Calculates the offset from a pointer.
    /// `count` is in units of `T`;
    /// e.g., a `count` of 3 represents a pointer offset of `3 * size_of::<T>()` bytes.
    pub fn add(&self, count: usize) -> Self {
        Self(unsafe { self.0.add(count) }, PhantomData)
    }

    // 返回指针对应的虚地址。
    /// Returns the virtual address represented by the pointer.
    pub fn as_addr(&self) -> VirtAddr {
        self.0 as _
    }

    // 检查用户指针是否合法。
    //
    // 如果指针非空且对齐则返回 `OK(())`。
    /// Checks avaliability of the user pointer.
    ///
    /// Returns [`Ok(())`] if it is neither null nor unaligned, and lies in the
    /// user half of the address space.
    pub fn check(&self) -> Result<()> {
        self.check_len(1)
    }

    /// [`check`](Self::check) for a run of `count` elements starting here, so a
    /// slice that STARTS in the user half cannot run off its top end into the
    /// kernel.
    pub fn check_len(&self, count: usize) -> Result<()> {
        let bytes = count
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(Error::InvalidLength)?;
        if !self.0.is_null()
            && (self.0 as usize).is_multiple_of(core::mem::align_of::<T>())
            && in_user_half(self.0 as usize, bytes)
        {
            // Being in the user half says nothing about anything being mapped
            // there, and this pointer is about to be dereferenced with no fault
            // fixup behind it. Ask the address space. See
            // `KernelHandler::check_user_range` for why an unmapped range here
            // used to kill the machine rather than return EFAULT.
            if !crate::KHANDLER.check_user_range(self.0 as usize, bytes) {
                return Err(Error::InvalidPointer);
            }
            Ok(())
        } else {
            Err(Error::InvalidPointer)
        }
    }
}

impl<T, P: Read> UserPtr<T, P> {
    // 取出指针值的引用（不要用于小于 8 字节的类型）。
    /// Converts to reference.
    #[allow(clippy::should_implement_trait)]
    pub fn as_ref(&self) -> &'static T {
        unsafe { &*self.0 }
    }

    // 读取但不移动指针所指的值（通过逐字节拷贝，但不需要 `Copy` 特征）。
    // 指针所指的值保持不变。
    /// Reads the value from `self` without moving it.
    /// This leaves the memory in self unchanged.
    pub fn read(&self) -> Result<T> {
        self.check()?;
        Ok(unsafe { self.0.read() })
    }

    // 和读取一样，
    // 但若指针为空，返回 `None`。
    /// Same as [`read`](Self::read),
    /// but returns [`None`] when pointer is null.
    pub fn read_if_not_null(&self) -> Result<Option<T>> {
        if !self.0.is_null() {
            Ok(Some(self.read()?))
        } else {
            Ok(None)
        }
    }

    // 构造一个从指针指向开始，长度为 `len` 的切片。
    /// Forms a slice from a user pointer and a `len`.
    pub fn as_slice(&self, len: usize) -> Result<&'static [T]> {
        if len == 0 {
            Ok(&[])
        } else {
            self.check_len(len)?;
            Ok(unsafe { core::slice::from_raw_parts(self.0, len) })
        }
    }

    // 拷贝对象来构造一个 `Vec`。
    //
    // `len` 是成员的数量，而不是字节数。
    /// Copies elements into a new [`Vec`].
    ///
    /// The `len` argument is the number of **elements**, not the number of bytes.
    #[allow(clippy::uninit_vec)]
    pub fn read_array(&self, len: usize) -> Result<Vec<T>> {
        if len == 0 {
            Ok(Vec::default())
        } else {
            self.check_len(len)?;
            // The total number of bytes to copy must not overflow `usize`,
            // otherwise the allocation would be smaller than `set_len` claims
            // and the following copy would write out of bounds.
            len.checked_mul(core::mem::size_of::<T>())
                .ok_or(Error::InvalidLength)?;
            let mut ret = Vec::<T>::with_capacity(len);
            unsafe {
                ret.set_len(len);
                ret.as_mut_ptr().copy_from_nonoverlapping(self.0, len);
            }
            Ok(ret)
        }
    }
}

impl<P: Read> UserPtr<u8, P> {
    // 构造一个从指针指向开始，长度为 `len` 的字符切片。
    /// Forms an utf-8 string slice from a user pointer and a `len`.
    pub fn as_str(&self, len: usize) -> Result<&'static str> {
        core::str::from_utf8(self.as_slice(len)?).map_err(|_| Error::InvalidUtf8)
    }

    // 从一个 C 风格的零结尾字符串构造一个字符切片。
    /// Forms a zero-terminated string slice from a user pointer to a c style string.
    ///
    /// The scan for the terminating `'\0'` is bounded by [`MAX_C_STR_LEN`] so a
    /// malicious or buggy user pointer that is not null-terminated cannot make
    /// the kernel walk an unbounded amount of memory (and the previous
    /// `unwrap()` could panic the kernel).
    pub fn as_c_str(&self) -> Result<&'static str> {
        self.check()?;
        let len = (0..MAX_C_STR_LEN)
            .find(|&i| unsafe { *self.0.add(i) == 0 })
            .ok_or(Error::InvalidLength)?;
        self.as_str(len)
    }
}

/// Upper bound for the length of a C string read from user space.
const MAX_C_STR_LEN: usize = 4 * 1024 * 1024;

/// Upper bound for the number of entries in a user-supplied pointer array
/// (e.g. `argv`/`envp`), to bound kernel work and allocations.
const MAX_C_STR_ARRAY_LEN: usize = 1 << 20;

impl<P: 'static + Read> UserPtr<UserPtr<u8, P>, P> {
    // 拷贝一组 C 风格的零结尾字符串到 `String`，
    // 并收集到一个 `Vec` 中。
    /// Copies a group of zero-terminated string into [`String`]s,
    /// and collect them into a [`Vec`].
    pub fn read_cstring_array(&self) -> Result<Vec<String>> {
        self.check()?;
        let mut result = Vec::new();
        let mut pptr = self.0;
        for _ in 0..MAX_C_STR_ARRAY_LEN {
            let sptr = unsafe { pptr.read() };
            if sptr.is_null() {
                return Ok(result);
            }
            result.push(sptr.as_c_str()?.into());
            pptr = unsafe { pptr.add(1) };
        }
        Err(Error::InvalidLength)
    }
}

impl<T, P: Write> UserPtr<T, P> {
    // 用指定的值覆盖指针位置。
    // 旧的值直接被覆盖，不会调用释放逻辑。
    /// Overwrites a memory location with the given `value`
    /// **without** reading or dropping the old value.
    pub fn write(&mut self, value: T) -> Result<()> {
        self.check()?;
        #[cfg(all(not(feature = "libos"), feature = "uleak-scan"))]
        dbg_scan_physmap_leak(
            unsafe {
                core::slice::from_raw_parts(
                    &value as *const T as *const u8,
                    core::mem::size_of::<T>(),
                )
            },
            self.0 as usize,
            "write",
        );
        unsafe { self.0.write(value) };
        Ok(())
    }

    // 类似于写，
    // 但指针为空时返回 `Ok(())`。
    /// Same as [`write`](Self::write),
    /// but does nothing and returns [`Ok`] when pointer is null.
    pub fn write_if_not_null(&mut self, value: T) -> Result<()> {
        if !self.0.is_null() {
            self.write(value)
        } else {
            Ok(())
        }
    }

    // 写入 `values.len() * size_of<T>` 字节到指针位置。
    // 写入的区间与目标区间不可重叠。
    /// Copies `values.len() * size_of<T>` bytes from `values` to `self`.
    /// The source and destination may not overlap.
    pub fn write_array(&mut self, values: &[T]) -> Result<()> {
        if !values.is_empty() {
            self.check_len(values.len())?;
            #[cfg(all(not(feature = "libos"), feature = "uleak-scan"))]
            dbg_scan_physmap_leak(
                unsafe {
                    core::slice::from_raw_parts(
                        values.as_ptr() as *const u8,
                        core::mem::size_of_val(values),
                    )
                },
                self.0 as usize,
                "write_array",
            );
            unsafe {
                self.0
                    .copy_from_nonoverlapping(values.as_ptr(), values.len())
            };
        }
        Ok(())
    }
}

impl<P: Write> UserPtr<u8, P> {
    // 拷贝指定字符串到目标位置并写入一个 `\0` 来模拟 C 风格零结尾字符串。
    /// Copies `s` to `self`, then write a `'\0'` for c style string.
    pub fn write_cstring(&mut self, s: &str) -> Result<()> {
        let bytes = s.as_bytes();
        // +1: the NUL below is written past the array, so it must be bounded too.
        self.check_len(bytes.len() + 1)?;
        self.write_array(bytes)?;
        unsafe { self.0.add(bytes.len()).write(0) };
        Ok(())
    }
}

#[repr(C)]
pub struct IoVec<P: Policy> {
    /// Starting address
    ptr: UserPtr<u8, P>,
    /// Number of bytes to transfer
    len: usize,
}

impl<P: Policy> core::fmt::Debug for IoVec<P> {
    fn fmt(&self, f: &mut Formatter) -> core::fmt::Result {
        write!(f, "IoVec ptr:{:?} len :{:?}", self.ptr.0, self.len)
    }
}
pub type IoVecIn = IoVec<In>;
pub type IoVecOut = IoVec<Out>;
pub type IoVecsOut = IoVecs<Out>;

/// A valid IoVecs request from user
pub struct IoVecs<P: 'static + Policy> {
    vec: Vec<IoVec<P>>,
}

impl<P: Policy> Debug for IoVecs<P> {
    fn fmt(&self, f: &mut Formatter) -> core::fmt::Result {
        write!(f, "IoVec len :{:?}", self.vec.len())
    }
}

// `IoVecs::<Out>::new` used to live here: a constructor that did
// `read_iovecs(count).unwrap()`. Every one of `read_iovecs`'s errors comes
// from userspace -- a null pointer, more than `IOV_MAX` entries, lengths that
// sum past `usize` -- so that `unwrap` was a kernel panic reachable from any
// `readv`-shaped syscall that happened to use it. Nothing did, which is why it
// never fired. It is removed rather than fixed: `read_iovecs` is the same call
// with the error kept, and an infallible-looking constructor over fallible
// user input is a trap for whoever reaches for it next.
impl<P: Policy> UserInPtr<IoVec<P>> {
    pub fn read_iovecs(&self, count: usize) -> Result<IoVecs<P>> {
        if self.0.is_null() {
            return Err(Error::InvalidPointer);
        }
        // Linux caps the number of iovecs at `IOV_MAX` (1024); reject anything
        // larger so a huge `count` cannot trigger an unbounded allocation.
        if count > 1024 {
            return Err(Error::InvalidLength);
        }
        let vec = self.read_array(count)?;
        // The sum of length should not overflow.
        let mut total_count = 0usize;
        for io_vec in vec.iter() {
            let (result, overflow) = total_count.overflowing_add(io_vec.len());
            if overflow {
                return Err(Error::InvalidLength);
            }
            total_count = result;
        }
        Ok(IoVecs { vec })
    }
}

impl<P: Policy> IoVecs<P> {
    pub fn total_len(&self) -> usize {
        self.vec.iter().map(|vec| vec.len).sum()
    }
}

impl<P: Read> IoVecs<P> {
    pub fn read_to_vec(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        for vec in self.vec.iter() {
            buf.extend_from_slice(vec.ptr.as_slice(vec.len)?);
        }
        Ok(buf)
    }

    /// Copy up to `buf.len()` bytes of the gathered iovec byte-stream into
    /// `buf`, starting at stream offset `offset`. Returns the number of bytes
    /// copied — 0 only at end-of-stream. Zero-length iovecs are legal and
    /// contribute nothing (POSIX).
    ///
    /// This is what lets `writev` process an arbitrarily large gather list
    /// through a bounded kernel buffer, the way Linux iterates the iov array
    /// with a bounded copy per step, instead of `read_to_vec`'s
    /// caller-controlled single allocation of the full total.
    pub fn read_bytes_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let mut skip = offset;
        let mut copied = 0usize;
        for vec in self.vec.iter() {
            if copied == buf.len() {
                break;
            }
            if skip >= vec.len {
                skip -= vec.len;
                continue;
            }
            let n = (vec.len - skip).min(buf.len() - copied);
            let src = vec.ptr.add(skip).as_slice(n)?;
            buf[copied..copied + n].copy_from_slice(src);
            copied += n;
            skip = 0;
        }
        Ok(copied)
    }
}

impl<P: Write> IoVecs<P> {
    pub fn write_from_buf(&mut self, mut buf: &[u8]) -> Result<usize> {
        let buf_len = buf.len();
        for vec in self.vec.iter_mut() {
            let copy_len = vec.len.min(buf.len());
            if copy_len == 0 {
                continue;
            }
            vec.ptr.write_array(&buf[..copy_len])?;
            buf = &buf[copy_len..];
        }
        Ok(buf_len - buf.len())
    }
}

impl<P: Policy> Deref for IoVecs<P> {
    type Target = [IoVec<P>];

    fn deref(&self) -> &Self::Target {
        self.vec.as_slice()
    }
}

impl<P: Write> DerefMut for IoVecs<P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.vec.as_mut_slice()
    }
}

impl<P: Policy> IoVec<P> {
    pub fn addr(&self) -> VirtAddr {
        self.ptr.as_addr()
    }

    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn check(&self) -> Result<()> {
        self.ptr.check()
    }

    pub fn as_slice(&self) -> Result<&[u8]> {
        if self.len == 0 {
            Ok(&[])
        } else if !self.ptr.is_null() {
            Ok(unsafe { core::slice::from_raw_parts(self.ptr.0, self.len) })
        } else {
            Err(Error::InvalidVectorAddress)
        }
    }

    pub fn as_mut_slice(&mut self) -> Result<&mut [u8]> {
        if self.len == 0 {
            Ok(&mut [])
        } else if !self.ptr.is_null() {
            Ok(unsafe { core::slice::from_raw_parts_mut(self.ptr.0, self.len) })
        } else {
            Err(Error::InvalidVectorAddress)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the user-pointer copy primitives.
    //!
    //! Every syscall that moves bytes between the kernel and a process goes
    //! through this module, so a regression here is either an arbitrary
    //! kernel-memory read/write reachable from userspace (the bound checks) or
    //! silent data corruption in `read`/`write`/`readv`/`writev` (the copy
    //! helpers). Both are invisible until something far away misbehaves, which
    //! is exactly what a unit test is for.
    //!
    //! The host's own heap and stack live below `USER_MAX`
    //! (`0x0000_8000_0000_0000`) on x86_64 Linux, so a plain `Vec` is a valid
    //! stand-in for a user buffer and the copies below run for real.

    use super::*;

    /// `check()` consults `KHANDLER`, which has no default outside `libos`.
    /// `init_once_by` is a `call_once`, so every test may call this -- and
    /// because it is a `call_once`, the whole test binary shares whichever
    /// handler lands first. So there is exactly one, in
    /// [`crate::utils::test_frames`], and it answers frame allocations too.
    fn init_handler() {
        crate::utils::test_frames::install();
    }

    // ---- bounds ---------------------------------------------------------

    /// The bound arithmetic itself. `in_user_half` is a constant `true` under
    /// `libos`, the only configuration `kernel-hal` builds in on the host, so
    /// the bare-metal behaviour is tested through [`range_within`] — the
    /// function `in_user_half` delegates to when the bound applies.
    #[test]
    fn range_within_refuses_to_cross_or_wrap_past_the_limit() {
        // Wholly below the limit.
        assert!(range_within(0x1000, 0x100, USER_MAX));
        // The last byte is in; the first byte above it is out.
        assert!(range_within(USER_MAX - 1, 1, USER_MAX));
        assert!(!range_within(USER_MAX, 1, USER_MAX));
        // A zero-length range exactly at the limit still fits.
        assert!(range_within(USER_MAX, 0, USER_MAX));
        // A range that STARTS below the limit but runs past it is rejected as
        // a whole -- the case that made every syscall out-pointer an
        // arbitrary kernel-memory write.
        assert!(range_within(USER_MAX - 8, 8, USER_MAX));
        assert!(!range_within(USER_MAX - 8, 9, USER_MAX));
        // `addr + bytes` must not be allowed to wrap around back into range.
        assert!(!range_within(usize::MAX, 1, USER_MAX));
        assert!(!range_within(0x1000, usize::MAX, USER_MAX));
        // A kernel-half address, which is what a malicious pointer looks like.
        assert!(!range_within(0xffff_8000_0000_0000, 1, USER_MAX));
    }

    #[test]
    fn user_range_ok_lets_zero_length_through_but_never_a_null_pointer() {
        // A zero-length range is always fine, even from a null pointer: that
        // is what `read(fd, buf, 0)` and an empty ioctl payload pass.
        assert!(user_range_ok(0, 0));
        // A null pointer with bytes to move is not.
        assert!(!user_range_ok(0, 1));
        // An ordinary buffer is.
        let buf = [0u8; 16];
        assert!(user_range_ok(buf.as_ptr() as usize, buf.len()));
        // The bound itself only applies on bare metal; assert it where it does.
        #[cfg(not(feature = "libos"))]
        {
            assert!(!user_range_ok(USER_MAX, 1));
            assert!(!user_range_ok(USER_MAX - 8, 9));
        }
    }

    #[test]
    fn check_rejects_null_and_unaligned_pointers() {
        init_handler();
        assert_eq!(
            UserInPtr::<u64>::from(0).check().err(),
            Some(Error::InvalidPointer)
        );
        // Aligned: fine.
        let v = [0u64; 2];
        let p = UserInPtr::<u64>::from(v.as_ptr() as usize);
        assert_eq!(p.check(), Ok(()));
        // Same buffer off by one byte: `u64` needs 8-byte alignment.
        assert_eq!(
            UserInPtr::<u64>::from(v.as_ptr() as usize + 1)
                .check()
                .err(),
            Some(Error::InvalidPointer)
        );
        // A kernel-half address is refused however well aligned it is.
        #[cfg(not(feature = "libos"))]
        assert_eq!(
            UserInPtr::<u64>::from(0xffff_8000_0000_0000).check().err(),
            Some(Error::InvalidPointer)
        );
    }

    #[test]
    fn check_len_refuses_a_count_whose_byte_size_overflows() {
        init_handler();
        // `count * size_of::<T>()` must not be allowed to wrap: a count that
        // overflows is a length error, never a pass.
        let v = [0u32; 4];
        let p = UserInPtr::<u32>::from(v.as_ptr() as usize);
        assert_eq!(p.check_len(4), Ok(()));
        assert_eq!(p.check_len(usize::MAX).err(), Some(Error::InvalidLength));
        // And an array that runs off the top of the user half is refused.
        #[cfg(not(feature = "libos"))]
        {
            let top = UserInPtr::<u32>::from(USER_MAX - 0x1000);
            assert_eq!(top.check_len(0x400), Ok(()));
            assert_eq!(top.check_len(0x401).err(), Some(Error::InvalidPointer));
        }
    }

    #[test]
    fn from_addr_size_requires_room_for_the_whole_value() {
        // `size` is the buffer userspace offered, in bytes.
        assert!(UserInPtr::<u64>::from_addr_size(0x1000, 8).is_ok());
        assert!(UserInPtr::<u64>::from_addr_size(0x1000, 9).is_ok());
        assert_eq!(
            UserInPtr::<u64>::from_addr_size(0x1000, 7).err(),
            Some(Error::BufferTooSmall)
        );
    }

    // ---- copies ---------------------------------------------------------

    #[test]
    fn read_array_and_as_slice_copy_the_requested_elements() {
        init_handler();
        let src: Vec<u32> = (0..16).collect();
        let p = UserInPtr::<u32>::from(src.as_ptr() as usize);
        assert_eq!(p.read_array(4).unwrap(), [0, 1, 2, 3]);
        assert_eq!(p.add(12).as_slice(4).unwrap(), &[12, 13, 14, 15]);
        // A zero-length read is legal and allocates nothing, even though the
        // pointer is never checked on that path.
        assert!(p.read_array(0).unwrap().is_empty());
        assert!(UserInPtr::<u32>::from(0).as_slice(0).unwrap().is_empty());
    }

    #[test]
    fn as_c_str_stops_at_the_nul_and_validates_utf8() {
        init_handler();
        let s = b"hola\0resto ignorado\0";
        let p = UserInPtr::<u8>::from(s.as_ptr() as usize);
        assert_eq!(p.as_c_str().unwrap(), "hola");
        // An empty C string is a valid one.
        let empty = b"\0";
        assert_eq!(
            UserInPtr::<u8>::from(empty.as_ptr() as usize)
                .as_c_str()
                .unwrap(),
            ""
        );
        // Bytes that are not UTF-8 are rejected rather than transmuted.
        let bad = b"\xff\xfe\0";
        assert_eq!(
            UserInPtr::<u8>::from(bad.as_ptr() as usize).as_c_str(),
            Err(Error::InvalidUtf8)
        );
    }

    #[test]
    fn write_cstring_bounds_the_trailing_nul() {
        init_handler();
        let mut buf = [0xAAu8; 8];
        let mut p = UserOutPtr::<u8>::from(buf.as_mut_ptr() as usize);
        p.write_cstring("abc").unwrap();
        assert_eq!(&buf[..5], b"abc\0\xAA");
    }

    #[test]
    fn read_cstring_array_stops_at_the_null_entry() {
        init_handler();
        let a = b"uno\0";
        let b = b"dos\0";
        // argv-shaped: pointers to C strings, terminated by a null pointer.
        let argv: Vec<usize> = vec![a.as_ptr() as usize, b.as_ptr() as usize, 0];
        let p = UserInPtr::<UserInPtr<u8>>::from(argv.as_ptr() as usize);
        assert_eq!(p.read_cstring_array().unwrap(), vec!["uno", "dos"]);
    }

    // ---- iovecs ---------------------------------------------------------

    /// Build a user-visible `struct iovec[]` over `slices` and read it back
    /// through the same path `readv`/`writev` use. The returned `Vec` must
    /// outlive the `IoVecs`, hence the tuple.
    fn iovecs_over<P: Policy>(slices: &[(usize, usize)]) -> (Vec<IoVec<P>>, IoVecs<P>) {
        let raw: Vec<IoVec<P>> = slices
            .iter()
            .map(|&(ptr, len)| IoVec {
                ptr: UserPtr::from(ptr),
                len,
            })
            .collect();
        let vecs = UserInPtr::<IoVec<P>>::from(raw.as_ptr() as usize)
            .read_iovecs(raw.len())
            .unwrap();
        (raw, vecs)
    }

    #[test]
    fn read_iovecs_enforces_iov_max_and_a_non_overflowing_total() {
        init_handler();
        // A null iov array is EFAULT-shaped, not an empty gather list.
        assert_eq!(
            UserInPtr::<IoVecIn>::from(0).read_iovecs(0).err(),
            Some(Error::InvalidPointer)
        );
        let one = [0u8; 1];
        let raw = vec![IoVecIn {
            ptr: UserPtr::from(one.as_ptr() as usize),
            len: 1,
        }];
        let p = UserInPtr::<IoVecIn>::from(raw.as_ptr() as usize);
        // IOV_MAX is 1024; a larger count is refused before any allocation.
        assert_eq!(p.read_iovecs(1025).err(), Some(Error::InvalidLength));
        // Two iovecs whose lengths sum past `usize` are refused too: the sum
        // is what the caller sizes its kernel buffer with.
        let huge = vec![
            IoVecIn {
                ptr: UserPtr::from(one.as_ptr() as usize),
                len: usize::MAX,
            },
            IoVecIn {
                ptr: UserPtr::from(one.as_ptr() as usize),
                len: 2,
            },
        ];
        assert_eq!(
            UserInPtr::<IoVecIn>::from(huge.as_ptr() as usize)
                .read_iovecs(2)
                .err(),
            Some(Error::InvalidLength)
        );
    }

    #[test]
    fn read_bytes_at_walks_the_gather_list_through_a_bounded_buffer() {
        init_handler();
        let a = b"abcd";
        let empty: [u8; 0] = [];
        let b = b"efghij";
        // A zero-length iovec in the middle is legal and contributes nothing.
        let (_raw, vecs) = iovecs_over::<In>(&[
            (a.as_ptr() as usize, a.len()),
            (empty.as_ptr() as usize, 0),
            (b.as_ptr() as usize, b.len()),
        ]);
        assert_eq!(vecs.total_len(), 10);
        assert_eq!(vecs.read_to_vec().unwrap(), b"abcdefghij");

        // Whole stream through a buffer smaller than any single iovec.
        let mut out = Vec::new();
        let mut off = 0;
        loop {
            let mut chunk = [0u8; 3];
            let n = vecs.read_bytes_at(off, &mut chunk).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            off += n;
        }
        assert_eq!(out, b"abcdefghij");

        // An offset that lands inside the second non-empty iovec.
        let mut chunk = [0u8; 4];
        assert_eq!(vecs.read_bytes_at(6, &mut chunk).unwrap(), 4);
        assert_eq!(&chunk, b"ghij");
        // Past the end of the stream is 0 bytes, not an error.
        assert_eq!(vecs.read_bytes_at(10, &mut chunk).unwrap(), 0);
        assert_eq!(vecs.read_bytes_at(99, &mut chunk).unwrap(), 0);
    }

    #[test]
    fn write_from_buf_scatters_and_reports_what_it_placed() {
        init_handler();
        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        let (_raw, mut vecs) =
            iovecs_over::<Out>(&[(a.as_mut_ptr() as usize, 4), (b.as_mut_ptr() as usize, 4)]);
        // A buffer shorter than the scatter list fills it partially and
        // reports the bytes actually placed.
        assert_eq!(vecs.write_from_buf(b"12345").unwrap(), 5);
        assert_eq!(&a, b"1234");
        assert_eq!(&b[..1], b"5");
        // A buffer longer than the scatter list stops at its capacity.
        assert_eq!(vecs.write_from_buf(b"ABCDEFGHIJ").unwrap(), 8);
        assert_eq!(&a, b"ABCD");
        assert_eq!(&b, b"EFGH");
    }

    #[test]
    fn the_user_half_bound_is_what_libos_turns_off_and_nothing_else() {
        // On bare metal the kernel is mapped into EVERY address space, so a
        // pointer that arrived from userspace naming a kernel address is not
        // a fault waiting to happen: it resolves, and the copy lands in
        // kernel memory. `libos` runs in a host process where "user"
        // addresses are ordinary host addresses, so the bound does not apply
        // there -- and that exemption is the only build this suite compiles,
        // which is why nothing here could ever see the bound refuse anything.
        const KERNEL: usize = 0xffff_8000_0000_0000;
        assert!(
            !in_user_half_with(false, KERNEL, 8),
            "una direccion de kernel paso el limite"
        );
        assert!(in_user_half_with(false, 0x1000, 8));
        assert!(
            !in_user_half_with(false, USER_MAX - 4, 8),
            "un rango a caballo del limite paso entero"
        );
        // Under the exemption every address clears it, kernel ones included.
        assert!(in_user_half_with(true, KERNEL, 8));
        assert!(in_user_half_with(true, usize::MAX, usize::MAX));
        // ...and the build this suite runs in is the exempt one, which is
        // what lets the tests below use host addresses as user pointers.
        assert!(in_user_half(KERNEL, 8));
    }

    #[test]
    fn a_range_the_address_space_does_not_map_is_refused_before_it_is_read() {
        // Being in the user half says nothing about anything being mapped
        // there, and the pointer is about to be dereferenced with no fault
        // fixup behind it: an unmapped range is a kernel page fault the
        // machine does not survive, reachable from any syscall by any
        // process. `check_user_range` is the question that turns it into
        // EFAULT, and every handler answers it `true` by default -- so until
        // one could say `false`, nothing told whether it was asked.
        init_handler();
        let buf = [0u8; 32];
        let addr = buf.as_ptr() as usize;
        let ptr = UserInPtr::<u8>::from(addr);
        assert!(ptr.check_len(32).is_ok());
        crate::utils::test_frames::refuse_user_range(addr, 32);
        assert_eq!(
            ptr.check_len(32).err(),
            Some(Error::InvalidPointer),
            "el rango sin mapear paso la comprobacion"
        );
        crate::utils::test_frames::allow_user_range(addr, 32);
        assert!(ptr.check_len(32).is_ok());
    }

    #[test]
    fn a_zero_length_read_is_legal_even_from_a_null_pointer() {
        // `read(fd, NULL, 0)` is a legal call and a zero-length iovec is a
        // legal entry, and neither touches the pointer. Checked like a
        // one-element read they become EFAULT, and the caller did nothing
        // wrong.
        init_handler();
        assert!(UserInPtr::<u8>::from(0).read_array(0).unwrap().is_empty());
        assert!(UserInPtr::<u8>::from(0).as_slice(0).unwrap().is_empty());
        // Not even the alignment matters with no elements to align.
        assert!(UserInPtr::<u64>::from(1).read_array(0).is_ok());
    }

    #[test]
    fn a_null_pointer_is_refused_before_anything_walks_it() {
        // Both of these walk memory with raw reads and no fault fixup behind
        // them, so the check is not a formality: without it a NULL path or a
        // NULL `argv` is a kernel page fault, from any process, on purpose.
        init_handler();
        assert_eq!(
            UserInPtr::<u8>::from(0).as_c_str().err(),
            Some(Error::InvalidPointer)
        );
        assert_eq!(
            UserInPtr::<UserInPtr<u8>>::from(0)
                .read_cstring_array()
                .err(),
            Some(Error::InvalidPointer)
        );
    }

    #[test]
    fn the_gather_list_stops_exactly_at_iov_max() {
        // 1024 is `IOV_MAX`, and the count is what sizes the kernel's own
        // allocation before a single byte is copied. The test above asks for
        // 1025 entries out of an array of one, so the lengths it reads are
        // whatever follows in memory and their sum overflows: the SAME error
        // the bound gives, from the other mechanism, whatever the bound says.
        // Here the array really has 1025 entries, so only the bound can
        // refuse it.
        init_handler();
        let one = [0u8; 1];
        let raw: Vec<IoVecIn> = (0..1025)
            .map(|_| IoVecIn {
                ptr: UserPtr::from(one.as_ptr() as usize),
                len: 1,
            })
            .collect();
        let p = UserInPtr::<IoVecIn>::from(raw.as_ptr() as usize);
        assert_eq!(
            p.read_iovecs(1024).unwrap().total_len(),
            1024,
            "IOV_MAX entero tiene que pasar"
        );
        assert_eq!(
            p.read_iovecs(1025).err(),
            Some(Error::InvalidLength),
            "una entrada mas que IOV_MAX"
        );
    }
}

#[cfg(test)]
mod benches {
    //! What the copies cost.
    //!
    //! Three batches of syscall rows ended on the same sentence: what a
    //! syscall costs is "the entry, the copies and the subsystem". The
    //! subsystem has rows and the argument work turned out to be free, but
    //! **the copies are this module** and had never been measured, so that
    //! sentence was a budget with its largest line item missing. These rows
    //! are that line item.
    //!
    //! Under `libos` a host `Vec` is a valid user buffer (the module comment
    //! on the tests above says why), so every copy here runs for real.
    //!
    //! Two things about this harness are worth knowing before reading a
    //! number, both in `docs/README-benchmarks.md`: `black_box` goes around
    //! the **inputs**, not only the result, or the compiler folds the work
    //! away; and a row of the shape "out-of-line call returning a `Result`"
    //! has a floor of about 6.4 ns that is the harness and not work.
    //!
    //! One caveat specific to this module, and it is a real limit on what
    //! these rows can say: **`in_user_half` is a constant `true` under
    //! `libos`**, which is the only configuration `kernel-hal` builds in on
    //! the host. So no row here measures the bare-metal user-half bound. The
    //! rows that ask about it call [`in_user_half_with`] with the exemption
    //! passed in, which is the seam the code already grew for the tests.

    use super::*;
    use test::{black_box, Bencher};

    /// `check_len` consults `KHANDLER`, which has no default outside `libos`;
    /// without this every row below returns `InvalidPointer` instead of
    /// copying, and the whole module would read as a few nanoseconds of
    /// nothing.
    fn init_handler() {
        crate::utils::test_frames::install();
    }

    /// A host buffer of `bytes` bytes, and the user pointer that names it.
    /// Leaked on purpose: `UserPtr` hands out `&'static` slices, and a row
    /// that freed its buffer each iteration would be measuring the allocator.
    fn user_buf(bytes: usize) -> UserInPtr<u8> {
        let buf = alloc::vec![0xa5u8; bytes].leak();
        UserInPtr::from(buf.as_ptr() as usize)
    }

    // ---- the bound arithmetic -------------------------------------------

    /// The bare-metal bound, with the `libos` exemption passed in as `false`
    /// so the arithmetic actually runs. This is the only row here that says
    /// anything about what a bare-metal build pays to reject a kernel
    /// pointer.
    #[bench]
    fn in_user_half_bare_metal_accepts(b: &mut Bencher) {
        b.iter(|| {
            black_box(in_user_half_with(
                black_box(false),
                black_box(0x1000usize),
                black_box(4096usize),
            ))
        });
    }

    /// The same bound refusing a kernel-half pointer: the case that used to
    /// make every syscall out-pointer an arbitrary kernel-memory write.
    #[bench]
    fn in_user_half_bare_metal_refuses_a_kernel_pointer(b: &mut Bencher) {
        b.iter(|| {
            black_box(in_user_half_with(
                black_box(false),
                black_box(0xffff_8000_0000_0000usize),
                black_box(4096usize),
            ))
        });
    }

    /// What a `libos` build pays for the same question, which is the
    /// exemption folding: the contrast with the two rows above is the whole
    /// cost of the bound.
    #[bench]
    fn in_user_half_under_libos(b: &mut Bencher) {
        b.iter(|| {
            black_box(in_user_half_with(
                black_box(true),
                black_box(0x1000usize),
                black_box(4096usize),
            ))
        });
    }

    /// `user_range_ok` is what a device `io_control` handler calls before
    /// dereferencing an ioctl argument, so it is on the hot path of every
    /// DRM/KMS ioctl the compositor issues.
    #[bench]
    fn user_range_ok_accepts(b: &mut Bencher) {
        b.iter(|| black_box(user_range_ok(black_box(0x1000usize), black_box(4096usize))));
    }

    // ---- the per-call check ---------------------------------------------

    /// `check_len` is the part of every copy that is NOT the copy: the null
    /// and alignment tests, the bound, and then an indirect call into the
    /// address space (`KHANDLER.check_user_range`) to ask whether anything is
    /// mapped there. Every row below pays this once; `as_c_str` pays it
    /// twice. This row is what to subtract.
    #[bench]
    fn check_len_of_one_byte(b: &mut Bencher) {
        init_handler();
        let ptr = user_buf(4096);
        b.iter(|| black_box(black_box(&ptr).check_len(black_box(1))));
    }

    /// The same check over a 4 KiB range. If this matches the row above, the
    /// address-space question is O(1) in the length and the check is a fixed
    /// cost per call rather than per byte.
    #[bench]
    fn check_len_of_4_kib(b: &mut Bencher) {
        init_handler();
        let ptr = user_buf(4096);
        b.iter(|| black_box(black_box(&ptr).check_len(black_box(4096))));
    }

    /// The refusal, which costs whatever the tests before the address-space
    /// question cost.
    #[bench]
    fn check_len_refuses_a_null_pointer(b: &mut Bencher) {
        init_handler();
        let ptr = UserInPtr::<u8>::from(0usize);
        b.iter(|| black_box(black_box(&ptr).check_len(black_box(1))));
    }

    // ---- one value ------------------------------------------------------

    /// The single-value read at the bottom of almost every syscall: one
    /// `timespec`, one `sockaddr`, one `int`. If this is dominated by the
    /// check above rather than by the eight bytes, then the cost of a small
    /// copy is the permission question and not the memory.
    #[bench]
    fn read_one_u64(b: &mut Bencher) {
        init_handler();
        let buf = alloc::vec![0u64; 1].leak();
        let ptr = UserInPtr::<u64>::from(buf.as_ptr() as usize);
        b.iter(|| black_box(black_box(&ptr).read()));
    }

    /// The matching write, which is every syscall's out-parameter.
    #[bench]
    fn write_one_u64(b: &mut Bencher) {
        init_handler();
        let buf = alloc::vec![0u64; 1].leak();
        let mut ptr = UserOutPtr::<u64>::from(buf.as_mut_ptr() as usize);
        b.iter(|| black_box(black_box(&mut ptr).write(black_box(0x1234_5678u64))));
    }

    // ---- a buffer: the copy, and the copy that need not happen ----------

    /// `read_array` allocates a `Vec` and copies into it. This family says
    /// what a `write(2)`-sized buffer costs, and the slope says how much of
    /// it is per byte.
    fn bench_read_array(b: &mut Bencher, bytes: usize) {
        init_handler();
        let ptr = user_buf(bytes);
        b.iter(|| black_box(black_box(&ptr).read_array(black_box(bytes))));
    }

    #[bench]
    fn read_array_of_64_bytes(b: &mut Bencher) {
        bench_read_array(b, 64);
    }

    #[bench]
    fn read_array_of_4_kib(b: &mut Bencher) {
        bench_read_array(b, 4096);
    }

    #[bench]
    fn read_array_of_64_kib(b: &mut Bencher) {
        bench_read_array(b, 64 * 1024);
    }

    /// `as_slice` answers the same question -- "give me these bytes" -- with
    /// no allocation and no copy: it checks the range and forms a slice over
    /// the caller's own memory. Against `read_array` at the same length, the
    /// difference is everything a syscall pays for taking a copy it may not
    /// need.
    fn bench_as_slice(b: &mut Bencher, bytes: usize) {
        init_handler();
        let ptr = user_buf(bytes);
        b.iter(|| black_box(black_box(&ptr).as_slice(black_box(bytes))));
    }

    #[bench]
    fn as_slice_of_64_bytes(b: &mut Bencher) {
        bench_as_slice(b, 64);
    }

    #[bench]
    fn as_slice_of_4_kib(b: &mut Bencher) {
        bench_as_slice(b, 4096);
    }

    #[bench]
    fn as_slice_of_64_kib(b: &mut Bencher) {
        bench_as_slice(b, 64 * 1024);
    }

    /// The write side of a buffer, which is what `read(2)` ends in.
    fn bench_write_array(b: &mut Bencher, bytes: usize) {
        init_handler();
        let src = alloc::vec![0x5au8; bytes];
        let dst = alloc::vec![0u8; bytes].leak();
        let mut ptr = UserOutPtr::<u8>::from(dst.as_mut_ptr() as usize);
        b.iter(|| black_box(black_box(&mut ptr).write_array(black_box(&src))));
    }

    #[bench]
    fn write_array_of_64_bytes(b: &mut Bencher) {
        bench_write_array(b, 64);
    }

    #[bench]
    fn write_array_of_4_kib(b: &mut Bencher) {
        bench_write_array(b, 4096);
    }

    #[bench]
    fn write_array_of_64_kib(b: &mut Bencher) {
        bench_write_array(b, 64 * 1024);
    }

    // ---- a path string --------------------------------------------------

    /// A NUL-terminated user buffer holding `len` bytes of name.
    fn user_c_str(len: usize) -> UserInPtr<u8> {
        let mut buf = alloc::vec![b'a'; len];
        buf.push(0);
        let buf = buf.leak();
        UserInPtr::from(buf.as_ptr() as usize)
    }

    /// `as_c_str` is how every path-taking syscall gets its path, so this is
    /// on the hot path of `open`, `stat`, `execve` and the rest.
    ///
    /// It is three passes over the same bytes, not one: `check()` for the
    /// first byte, a byte-at-a-time scan for the NUL, then `as_str` ->
    /// `as_slice` -> a SECOND `check_len` over the length just found, and
    /// `from_utf8` walking the bytes again to validate them. This family's
    /// slope is what that costs per byte of name.
    fn bench_as_c_str(b: &mut Bencher, len: usize) {
        init_handler();
        let ptr = user_c_str(len);
        b.iter(|| black_box(black_box(&ptr).as_c_str()));
    }

    /// About the length of a real path: `/usr/lib/libvulkan.so.1`.
    #[bench]
    fn as_c_str_of_24_bytes(b: &mut Bencher) {
        bench_as_c_str(b, 24);
    }

    /// `PATH_MAX`-ish, which is what a deep build tree hands in.
    #[bench]
    fn as_c_str_of_256_bytes(b: &mut Bencher) {
        bench_as_c_str(b, 256);
    }

    /// Not a path any more, but it is what the slope needs to be readable,
    /// and `MAX_C_STR_LEN` allows four megabytes of it.
    #[bench]
    fn as_c_str_of_4_kib(b: &mut Bencher) {
        bench_as_c_str(b, 4096);
    }

    /// `as_str` with the length already known: `as_c_str` minus the scan for
    /// the NUL, so the pair says what finding the terminator costs against
    /// validating the bytes.
    #[bench]
    fn as_str_of_256_bytes_with_the_length_known(b: &mut Bencher) {
        init_handler();
        let ptr = user_c_str(256);
        b.iter(|| black_box(black_box(&ptr).as_str(black_box(256))));
    }

    /// `execve`'s `argv`: a user array of pointers to C strings, each one
    /// read with the three passes above. The question is whether the per-
    /// entry cost is the string or the array walk.
    fn bench_cstring_array(b: &mut Bencher, count: usize) {
        init_handler();
        let mut ptrs: alloc::vec::Vec<UserInPtr<u8>> = (0..count).map(|_| user_c_str(24)).collect();
        ptrs.push(UserInPtr::from(0usize));
        let ptrs = ptrs.leak();
        let argv = UserInPtr::<UserInPtr<u8>>::from(ptrs.as_ptr() as usize);
        b.iter(|| black_box(black_box(&argv).read_cstring_array()));
    }

    /// What a shell hands `execve`.
    #[bench]
    fn read_cstring_array_of_8_entries(b: &mut Bencher) {
        bench_cstring_array(b, 8);
    }

    /// What a build system hands it.
    #[bench]
    fn read_cstring_array_of_256_entries(b: &mut Bencher) {
        bench_cstring_array(b, 256);
    }

    // ---- the gather list ------------------------------------------------

    /// A user `iovec` array of `count` entries, each naming `each` bytes.
    fn user_iovecs(count: usize, each: usize) -> UserInPtr<IoVec<In>> {
        let vecs: alloc::vec::Vec<IoVec<In>> = (0..count)
            .map(|_| IoVec {
                ptr: user_buf(each),
                len: each,
            })
            .collect();
        let vecs = vecs.leak();
        UserInPtr::from(vecs.as_ptr() as usize)
    }

    /// `read_iovecs` is `readv`/`writev`/`sendmsg`'s door: one `read_array`
    /// of the descriptor array, then a second pass over it summing the
    /// lengths to refuse an overflow. Two walks of the same array, so the
    /// slope should be twice the array's own cost.
    fn bench_read_iovecs(b: &mut Bencher, count: usize) {
        init_handler();
        let ptr = user_iovecs(count, 64);
        b.iter(|| black_box(black_box(&ptr).read_iovecs(black_box(count))));
    }

    #[bench]
    fn read_iovecs_of_1_entry(b: &mut Bencher) {
        bench_read_iovecs(b, 1);
    }

    #[bench]
    fn read_iovecs_of_16_entries(b: &mut Bencher) {
        bench_read_iovecs(b, 16);
    }

    /// `IOV_MAX`, which `read_iovecs` is what enforces.
    #[bench]
    fn read_iovecs_of_1024_entries(b: &mut Bencher) {
        bench_read_iovecs(b, 1024);
    }

    /// `read_to_vec` gathers the whole list into one allocation whose size
    /// userspace chooses. This is the call `read_bytes_at` was added to
    /// avoid, so it is the baseline that decides whether avoiding it helped.
    fn bench_read_to_vec(b: &mut Bencher, count: usize) {
        init_handler();
        let ptr = user_iovecs(count, 64);
        let iovecs = ptr.read_iovecs(count).expect("a well-formed gather list");
        b.iter(|| black_box(black_box(&iovecs).read_to_vec()));
    }

    #[bench]
    fn read_to_vec_of_16_entries(b: &mut Bencher) {
        bench_read_to_vec(b, 16);
    }

    #[bench]
    fn read_to_vec_of_1024_entries(b: &mut Bencher) {
        bench_read_to_vec(b, 1024);
    }

    /// One `read_bytes_at` at the START of the stream, where the walk has
    /// nothing to skip. This is the cheap end, and the baseline for the row
    /// below.
    fn bench_read_bytes_at_front(b: &mut Bencher, count: usize) {
        init_handler();
        let ptr = user_iovecs(count, 64);
        let iovecs = ptr.read_iovecs(count).expect("a well-formed gather list");
        let mut buf = alloc::vec![0u8; 64];
        b.iter(|| black_box(black_box(&iovecs).read_bytes_at(black_box(0), black_box(&mut buf))));
    }

    #[bench]
    fn read_bytes_at_the_front_of_16_entries(b: &mut Bencher) {
        bench_read_bytes_at_front(b, 16);
    }

    #[bench]
    fn read_bytes_at_the_front_of_1024_entries(b: &mut Bencher) {
        bench_read_bytes_at_front(b, 1024);
    }

    /// The same call asking for the LAST entry's bytes, which is the
    /// question that matters: `read_bytes_at` restarts its walk at `vec[0]`
    /// every time and skips forward, so reaching entry *n* walks the *n*
    /// entries before it. A `writev` driven through a bounded buffer -- the
    /// very thing this function exists for -- makes one such call per step,
    /// so if this row grows with the list then the bounded loop is
    /// **quadratic** in the number of iovecs and the unbounded
    /// `read_to_vec` it replaced was linear.
    fn bench_read_bytes_at_back(b: &mut Bencher, count: usize) {
        init_handler();
        let ptr = user_iovecs(count, 64);
        let iovecs = ptr.read_iovecs(count).expect("a well-formed gather list");
        let mut buf = alloc::vec![0u8; 64];
        let last = (count - 1) * 64;
        b.iter(|| {
            black_box(black_box(&iovecs).read_bytes_at(black_box(last), black_box(&mut buf)))
        });
    }

    #[bench]
    fn read_bytes_at_the_back_of_16_entries(b: &mut Bencher) {
        bench_read_bytes_at_back(b, 16);
    }

    #[bench]
    fn read_bytes_at_the_back_of_1024_entries(b: &mut Bencher) {
        bench_read_bytes_at_back(b, 1024);
    }

    /// `total_len` is the sum `read_iovecs` already computed once to check
    /// for overflow and then threw away, so every caller that wants it pays
    /// a third walk of the array.
    #[bench]
    fn total_len_of_1024_entries(b: &mut Bencher) {
        init_handler();
        let ptr = user_iovecs(1024, 64);
        let iovecs = ptr.read_iovecs(1024).expect("a well-formed gather list");
        b.iter(|| black_box(black_box(&iovecs).total_len()));
    }

    /// The whole point, measured rather than fitted: drain the entire gather
    /// list through a bounded buffer, which is what `read_bytes_at` was
    /// added to let `writev` do, and compare against `read_to_vec` of the
    /// same list. One call per step, each restarting the walk at `vec[0]`.
    ///
    /// Against `read_to_vec_of_1024_entries` this is the answer to "did the
    /// bounded buffer help": it bounds the ALLOCATION, which was the point,
    /// and the walk is what it costs.
    fn bench_drain_through_bounded_buf(b: &mut Bencher, count: usize) {
        init_handler();
        let ptr = user_iovecs(count, 64);
        let iovecs = ptr.read_iovecs(count).expect("a well-formed gather list");
        let total = count * 64;
        // One iovec's worth, so the number of steps is the number of
        // entries: the shape a bounded kernel buffer actually has.
        let mut buf = alloc::vec![0u8; 64];
        b.iter(|| {
            let mut offset = 0usize;
            while offset < black_box(total) {
                let n = black_box(&iovecs)
                    .read_bytes_at(black_box(offset), black_box(&mut buf))
                    .expect("the list is well-formed");
                if n == 0 {
                    break;
                }
                offset += n;
            }
            black_box(offset)
        });
    }

    #[bench]
    fn drain_16_entries_through_a_bounded_buf(b: &mut Bencher) {
        bench_drain_through_bounded_buf(b, 16);
    }

    #[bench]
    fn drain_1024_entries_through_a_bounded_buf(b: &mut Bencher) {
        bench_drain_through_bounded_buf(b, 1024);
    }
}
