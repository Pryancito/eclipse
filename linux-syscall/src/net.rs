use super::*;
use crate::outparams::hand_out_pair;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::mem::size_of;
use kernel_hal::user::UserInOutPtr;
use linux_object::{
    fs::{split_path, FileLike, OpenFlags},
    net::*,
};

const MSG_DONTWAIT: usize = 0x40;
const MSG_PEEK: usize = 0x2;

/// `struct mmsghdr` from `<sys/socket.h>`: one batch entry for
/// `sendmmsg`/`recvmmsg` — a plain `msghdr` plus the per-message transfer
/// count the kernel writes back. Only its layout is used (stride and the
/// `msg_len` offset); the per-entry `msg_hdr` is re-read by the wrapped
/// single-message syscalls straight from user memory.
#[repr(C)]
#[allow(dead_code)]
struct MMsgHdr {
    msg_hdr: MsgHdr,
    msg_len: u32,
    _pad: u32,
}

/// Read a `sockaddr` from user space, honoring the user-supplied `addrlen`.
///
/// `SockAddr` is a union whose alignment (4, coming from the `u32` fields of
/// the IP/netlink variants) is stricter than a C `struct sockaddr_un` (only
/// 2-byte aligned), and whose size (~110 B) is larger than most concrete
/// address structs. Reading it directly with `UserInPtr::<SockAddr>::read()`
/// therefore had two bugs:
///   (a) it rejected perfectly valid 2-byte-aligned `sockaddr_un` pointers with
///       `EFAULT` (the alignment `check()` requires 4-byte alignment) — this is
///       exactly why connecting to the X11 unix socket failed with
///       "unable to connect to X server: Bad address"; and
///   (b) it always read the full union size regardless of `addrlen`, over-
///       reading past a short user buffer that sits near the end of a mapping.
///
/// Copy exactly `addrlen` bytes (capped at the union size) byte-wise, at
/// 1-byte alignment, into a zeroed buffer instead, matching Linux
/// `move_addr_to_kernel` semantics.
#[allow(unsafe_code)]
fn read_sockaddr(addr: usize, addrlen: usize) -> Result<SockAddr, LxError> {
    if addr == 0 {
        return Err(LxError::EFAULT);
    }
    let n = addrlen.min(size_of::<SockAddr>());
    // Zeroed so any address bytes the user did not supply (`addrlen` shorter
    // than the concrete struct) read back as zero, as Linux does.
    let mut storage: SockAddr = unsafe { core::mem::zeroed() };
    if n > 0 {
        let bytes: UserInPtr<u8> = addr.into();
        let src = bytes.read_array(n)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                &mut storage as *mut SockAddr as *mut u8,
                n,
            );
        }
    }
    Ok(storage)
}

/// Copy an option value out to a `getsockopt` caller.
///
/// `optlen` is a value-result argument (like Linux's `optlen`): the kernel must
/// write at most the caller-supplied `*optlen` bytes and then store the true
/// size back. Previously `optlen` was write-only and the input size was ignored,
/// so an option larger than the caller's buffer (e.g. the 12-byte SO_PEERCRED
/// `ucred` written into a 4-byte buffer) overflowed adjacent user memory.
fn write_sockopt_out(
    optval: UserOutPtr<u32>,
    mut optlen: UserInOutPtr<u32>,
    value: &[u8],
) -> SysResult {
    let max = optlen.read()? as usize;
    let n = sockopt_out_len(max, value.len());
    if n > 0 {
        let mut dst: UserOutPtr<u8> = optval.as_addr().into();
        dst.write_array(&value[..n])?;
    }
    // Report the number of bytes actually written, not the option's full size.
    // `sock_getsockopt()` clamps (`if (len > lv) len = lv;`) before its
    // `put_user(len, optlen)`, and a caller that believes an `optlen` larger
    // than the buffer it supplied goes on to read bytes the kernel never
    // wrote — its own uninitialised stack.
    optlen.write(n as u32)?;
    Ok(0)
}

/// How many bytes of an option value `getsockopt` copies out, which is also
/// what it reports back through `optlen`.
///
/// `sock_getsockopt()` clamps (`if (len > lv) len = lv;`) before its
/// `put_user(len, optlen)`. Reporting the option's *full* size instead, which
/// is what this used to do, tells a caller with a smaller buffer that the
/// kernel wrote more than it did: the caller then reads its own uninitialised
/// stack as part of the answer. `SO_PEERCRED` into a 4-byte `int` is the case
/// that turns up, since a `struct ucred` is 12.
fn sockopt_out_len(requested: usize, actual: usize) -> usize {
    actual.min(requested)
}

/// `struct cmsghdr` is `{ size_t cmsg_len; int cmsg_level; int cmsg_type; }`:
/// 16 bytes on a 64-bit target, followed by the payload, with each message
/// `CMSG_ALIGN`ed to 8.
const CMSG_HDR_LEN: usize = 16;
/// `SOL_SOCKET`, as it appears in `cmsg_level`.
const SOL_SOCKET_LEVEL: i32 = 1;
/// `SCM_RIGHTS`, as it appears in `cmsg_type`.
const SCM_RIGHTS: i32 = 1;
/// Most file descriptors one `sendmsg` may carry (`SCM_MAX_FD`,
/// include/net/scm.h). Linux answers `EINVAL` above it; without a cap, one
/// `sendmsg` with a 64 KiB control buffer asks the receiver to install 16000
/// descriptors.
const SCM_MAX_FD: usize = 253;

/// Walk a control buffer and return the file descriptor numbers its
/// `SCM_RIGHTS` messages carry, in order.
///
/// Everything here comes from userspace, including `cmsg_len`, so every step
/// is checked: a length that wraps past the end of the buffer, one shorter
/// than the header it claims to be, and an aligned step that would not move
/// forward all stop the walk instead of slicing out of range or spinning.
/// A malformed *tail* is simply where the walk ends, which is what
/// `__cmsg_nxthdr()` does; only the fd count is an error, because Linux makes
/// it one.
fn parse_scm_rights_fds(ctrl: &[u8]) -> Result<Vec<i32>, LxError> {
    let mut fds: Vec<i32> = Vec::new();
    let mut off = 0usize;
    while off + CMSG_HDR_LEN <= ctrl.len() {
        let cmsg_len = u64::from_ne_bytes(ctrl[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_ne_bytes(ctrl[off + 8..off + 12].try_into().unwrap());
        let typ = i32::from_ne_bytes(ctrl[off + 12..off + 16].try_into().unwrap());
        let cmsg_end = match off.checked_add(cmsg_len) {
            Some(end) if cmsg_len >= CMSG_HDR_LEN && end <= ctrl.len() => end,
            _ => break,
        };
        if level == SOL_SOCKET_LEVEL && typ == SCM_RIGHTS {
            // A trailing partial fd is not one: `as_chunks` drops it, as the
            // kernel's `(cmsg_len - sizeof(cmsghdr)) / sizeof(int)` does.
            for chunk in ctrl[off + CMSG_HDR_LEN..cmsg_end].as_chunks::<4>().0 {
                if fds.len() == SCM_MAX_FD {
                    return Err(LxError::EINVAL);
                }
                fds.push(i32::from_ne_bytes(*chunk));
            }
        }
        // CMSG_ALIGN(cmsg_len); checked, and required to move forward so a
        // wrapped or zero step cannot spin forever.
        let step = match cmsg_len.checked_add(7).map(|v| v & !7) {
            Some(s) if s > 0 => s,
            _ => break,
        };
        off = match off.checked_add(step) {
            Some(n) => n,
            None => break,
        };
    }
    Ok(fds)
}

/// Build the single `SCM_RIGHTS` control message `recvmsg` hands back for
/// `fds`, in the layout [`parse_scm_rights_fds`] reads.
///
/// The two are the same 16-byte header written twice, once in each direction,
/// which is why the tests below round-trip them against each other rather than
/// each against a hand-written blob.
fn build_scm_rights_cmsg(fds: &[i32]) -> Vec<u8> {
    let cmsg_len = CMSG_HDR_LEN + fds.len() * 4;
    let mut buf = Vec::with_capacity(cmsg_len);
    buf.extend_from_slice(&(cmsg_len as u64).to_ne_bytes());
    buf.extend_from_slice(&SOL_SOCKET_LEVEL.to_ne_bytes());
    buf.extend_from_slice(&SCM_RIGHTS.to_ne_bytes());
    for fd in fds {
        buf.extend_from_slice(&fd.to_ne_bytes());
    }
    buf
}

impl Syscall<'_> {
    /// creates an endpoint for communication and returns a file descriptor that refers to that endpoint.
    pub fn sys_socket(&mut self, domain: usize, _type: usize, protocol: usize) -> SysResult {
        info!(
            "sys_socket: domain:{}, type:{}, protocol:{}",
            domain, _type, protocol
        );
        let domain = match Domain::try_from(domain) {
            Ok(domain) => domain,
            Err(_) => {
                warn!("sys_socket: invalid domain: {}", domain);
                return Err(LxError::EAFNOSUPPORT);
            }
        };
        let socket_type_val = _type & SOCKET_TYPE_MASK;
        let socket_type = match SocketType::try_from(socket_type_val) {
            Ok(t) => t,
            Err(_) => {
                warn!(
                    "sys_socket: invalid socket type: {:#x} (masked: {:#x})",
                    _type, socket_type_val
                );
                return Err(LxError::EINVAL);
            }
        };
        // socket flags: SOCK_CLOEXEC SOCK_NONBLOCK
        let flags = OpenFlags::from_bits_truncate(_type & !SOCKET_TYPE_MASK);
        let protocol_num = protocol;
        let protocol = Protocol::try_from(protocol_num).ok();

        info!(
            "sys_socket: domain:{:?}, type:{:?}, protocol:{:?}",
            domain, socket_type, protocol
        );

        let socket: Arc<dyn FileLike> = match (domain, socket_type, protocol) {
            (Domain::AF_INET, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_TCP)) => {
                Arc::new(TcpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET6, SocketType::SOCK_STREAM, Some(Protocol::IPPROTO_TCP)) => {
                Arc::new(TcpSocketState::new(true)?)
            }
            (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_UDP)) => {
                Arc::new(UdpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_IP))
            | (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_UDP)) => {
                Arc::new(UdpSocketState::new(true)?)
            }
            // Linux ping(8) uses SOCK_DGRAM + IPPROTO_ICMP (ping_socket).
            (Domain::AF_INET, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_ICMP)) => {
                Arc::new(IcmpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_DGRAM, Some(Protocol::IPPROTO_ICMPV6)) => {
                Arc::new(IcmpSocketState::new(true)?)
            }
            // Be tolerant for AF_INET/AF_INET6 datagram sockets.
            // Some userlands pass unexpected protocol numbers; for DHCP we only need UDP semantics.
            (Domain::AF_INET, SocketType::SOCK_DGRAM, None) => {
                Arc::new(UdpSocketState::new(false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_DGRAM, None) => {
                Arc::new(UdpSocketState::new(true)?)
            }
            // AF_INET/AF_INET6 raw sockets (some userlands probe these)
            (Domain::AF_INET, SocketType::SOCK_RAW, _) => {
                Arc::new(RawSocketState::new((protocol_num & 0xff) as u8, false)?)
            }
            (Domain::AF_INET6, SocketType::SOCK_RAW, _) => {
                Arc::new(RawSocketState::new((protocol_num & 0xff) as u8, true)?)
            }
            // AF_NETLINK sockets for interface/address discovery (iproute-style)
            (Domain::AF_NETLINK, SocketType::SOCK_RAW, _)
            | (Domain::AF_NETLINK, SocketType::SOCK_DGRAM, _) => {
                Arc::new(NetlinkSocketState::default())
            }
            // AF_PACKET sockets (used by udhcpc for raw ethernet operations)
            (Domain::AF_PACKET, SocketType::SOCK_RAW, _)
            | (Domain::AF_PACKET, SocketType::SOCK_DGRAM, _) => {
                PacketSocketState::new(socket_type, u16::from_be(protocol_num as u16))?
            }
            // AF_UNIX sockets
            (Domain::AF_UNIX, _, _) => {
                let s = UnixSocketState::new();
                // Record our PID so a peer (e.g. seatd) can read it via
                // SO_PEERCRED when it accepts our connection.
                s.set_owner_pid(self.zircon_process().id() as i32);
                // This arm takes EVERY AF_UNIX type, and the implementation is
                // a byte stream whichever one was asked for. Record the request
                // anyway: `sendmsg` must not truncate an oversized message on a
                // socket the user created as a datagram or seqpacket.
                s.set_socket_type(socket_type);
                s
            }
            (_, _, _) => {
                info!(
                    "sys_socket: unsupported socket type: domain={:?}, type={:?}, protocol={:?}",
                    domain, socket_type, protocol_num
                );
                return Err(LxError::ENOSYS);
            }
        };

        socket.set_flags(flags)?;
        let fd = self.linux_process().add_socket(socket)?; // dyn FileLike
        Ok(fd.into())
    }

    ///  connects the socket referred to by the file descriptor sockfd to the address specified by addr.
    pub async fn sys_connect(
        &mut self,
        sockfd: usize,
        addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_connect: sockfd:{}, addr:{:?}, addrlen:{}",
            sockfd, addr, addrlen
        );
        let endpoint = sockaddr_to_endpoint(read_sockaddr(addr.as_addr(), addrlen)?, addrlen)?;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(sockfd.into())?;

        if let Endpoint::Unix(path) = &endpoint {
            if let Ok(client) = file_like.clone().downcast_arc::<UnixSocketState>() {
                // ENOENT / ECONNREFUSED exactly as `UnixSocketState::
                // resolve_listener` decides (pathname vs abstract, bound
                // but not listening): this fast path is the one every
                // AF_UNIX connect(2) takes, so the rule must live in the
                // shared helper, not be re-derived here.
                let server = UnixSocketState::resolve_listener(path)?;
                // Establish the connection now: create the server's end,
                // wire it to the client, and queue it for accept(). Wiring
                // at connect time (rather than in accept) lets the client
                // send its first bytes — e.g. the X11 handshake — before
                // the server has accepted, instead of getting ENOTCONN.
                let server_side = UnixSocketState::new();
                server_side.set_path(server.bound_path());
                UnixSocketState::connect_pair(&client, &server_side);
                server.push_accept(server_side);
                return Ok(0);
            }
        }

        file_like.clone().as_socket()?.connect(endpoint).await?;
        Ok(0)
    }

    /// set options for the socket referred to by the file descriptor sockfd.
    pub fn sys_setsockopt(
        &mut self,
        sockfd: usize,
        level: usize,
        optname: usize,
        optval: UserInPtr<u8>,
        optlen: usize,
    ) -> SysResult {
        info!(
            "sys_setsockopt: sockfd:{}, level:{}, optname:{}, optval:{:?} , optlen:{}",
            sockfd, level, optname, optval, optlen
        );
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like
            .clone()
            .as_socket()?
            .setsockopt(level, optname, optval.as_slice(optlen)?)
    }

    /// get options for the socket referred to by the file descriptor sockfd.
    pub fn sys_getsockopt(
        &mut self,
        sockfd: usize,
        level: usize,
        optname: usize,
        optval: UserOutPtr<u32>,
        optlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getsockopt: sockfd:{}, level:{}, optname:{}, optval:{:?} , optlen:{:?}",
            sockfd, level, optname, optval, optlen
        );
        let level = match Level::try_from(level) {
            Ok(level) => level,
            Err(_) => {
                // Unknown levels (e.g. SOL_PACKET=263) — return a zeroed int.
                warn!("getsockopt: unsupported level: {}", level);
                return write_sockopt_out(optval, optlen, &0u32.to_ne_bytes());
            }
        };
        if optval.is_null() {
            return Err(LxError::EINVAL);
        }
        match level {
            Level::SOL_SOCKET => {
                // SO_PEERCRED (17): `struct ucred { pid_t pid; uid_t uid;
                // gid_t gid; }` — the credentials of the process on the other end
                // of a connected (unix) socket. seatd reads this to authorize a
                // Wayland client (labwc); without it the call returned ENOPROTOOPT
                // ("invalid optname: 17") and seatd refused the client. Eclipse is
                // single-user root, so report root uid/gid (which is what seatd
                // checks) and the peer's pid when the socket tracks it.
                const SO_PEERCRED: usize = 17;
                if optname == SO_PEERCRED {
                    let file_like = self.linux_process().get_file_like(sockfd.into())?;
                    let pid = file_like
                        .as_socket()
                        .ok()
                        .and_then(|s| s.peer_pid())
                        .unwrap_or(1);
                    let ucred: [u32; 3] = [pid as u32, 0, 0];
                    let mut bytes = [0u8; 12];
                    for (i, w) in ucred.iter().enumerate() {
                        bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
                    }
                    return write_sockopt_out(optval, optlen, &bytes);
                }
                let optname = match SolOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        // ENOPROTOOPT is the right answer, and for some of
                        // these it is the answer mainline Linux gives too --
                        // so do not shout about it. dbus, polkit and logind
                        // ask every AF_UNIX peer for its security context
                        // (SO_PEERSEC) and supplementary groups
                        // (SO_PEERGROUPS) on connect; a kernel with no LSM
                        // returns ENOPROTOOPT for the first and one older
                        // than 4.13 for the second, and every caller falls
                        // back cleanly. Logging those at `error!` filled the
                        // console with a repeating "invalid optname: 31 /
                        // invalid optname: 59" that reads like a fault and is
                        // not one.
                        const SO_PEERSEC: usize = 31;
                        const SO_PEERGROUPS: usize = 59;
                        match optname {
                            SO_PEERSEC | SO_PEERGROUPS => debug!(
                                "getsockopt: SOL_SOCKET optname {} (peer identity) not supported",
                                optname
                            ),
                            _ => warn!("getsockopt: unsupported SOL_SOCKET optname: {}", optname),
                        }
                        return Err(LxError::ENOPROTOOPT);
                    }
                };

                let file_like = self.linux_process().get_file_like(sockfd.into())?;
                // Only the buffer-size options need the capacities, and only
                // TCP and UDP report any: the trait default is `None`, so
                // asking unconditionally and unwrapping panicked the KERNEL on
                // a plain `getsockopt(SO_SNDBUF)` against a Unix socket --
                // which is what Firefox does to its own IPC channels.
                // Linux answers with net.core.{w,r}mem_default for a socket
                // with no queue of its own, so answer that.
                const MEM_DEFAULT: usize = 212_992;
                let buffer_capacity = || {
                    file_like
                        .clone()
                        .as_socket()
                        .ok()
                        .and_then(|s| s.get_buffer_capacity())
                        .unwrap_or((MEM_DEFAULT, MEM_DEFAULT))
                };

                match optname {
                    SolOptname::SNDBUF => {
                        let (_, send_buf_ca) = buffer_capacity();
                        write_sockopt_out(optval, optlen, &(send_buf_ca as u32).to_ne_bytes())
                    }
                    SolOptname::RCVBUF => {
                        let (recv_buf_ca, _) = buffer_capacity();
                        write_sockopt_out(optval, optlen, &(recv_buf_ca as u32).to_ne_bytes())
                    }
                    SolOptname::REUSEADDR => write_sockopt_out(optval, optlen, &1u32.to_ne_bytes()),
                    SolOptname::ERROR => {
                        let err = file_like.clone().as_socket()?.take_so_error();
                        write_sockopt_out(optval, optlen, &(err as u32).to_ne_bytes())
                    }
                    // struct linger { int l_onoff; int l_linger; } — zero-linger.
                    SolOptname::LINGER => write_sockopt_out(optval, optlen, &[0u8; 8]),
                }
            }
            Level::IPPROTO_TCP => {
                let optname = match TcpOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        warn!("getsockopt: unsupported IPPROTO_TCP optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };
                match optname {
                    TcpOptname::CONGESTION => Ok(0),
                }
            }
            Level::IPPROTO_IP => {
                let optname = match IpOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        warn!("getsockopt: unsupported IPPROTO_IP optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };
                match optname {
                    IpOptname::HDRINCL => write_sockopt_out(optval, optlen, &0u32.to_ne_bytes()),
                }
            }
        }
    }

    /// transmit a message to another socket
    pub fn sys_sendto(
        &mut self,
        sockfd: usize,
        buf: UserInPtr<u8>,
        len: usize,
        flags: usize,
        dest_addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_sendto: sockfd:{:?}, buffer:{:?}, length:{:?}, flags:{:?} , optlen:{:?}, addrlen:{:?}",
            sockfd, buf, len, flags, dest_addr, addrlen
        );
        let endpoint = if dest_addr.is_null() {
            None
        } else {
            let endpoint =
                sockaddr_to_endpoint(read_sockaddr(dest_addr.as_addr(), addrlen)?, addrlen)?;
            info!("sys_sendto: sockfd:{:?}, endpoint:{:?}", sockfd, endpoint);
            Some(endpoint)
        };
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        // Return the socket's ACTUAL queued byte count, not the full requested
        // `len`. TCP `write()` can perform a short write (queues min(len, free TX
        // space)); reporting `len` regardless makes the caller believe bytes it
        // never sent were delivered, silently truncating the stream.
        let written = file_like
            .clone()
            .as_socket()?
            .write(buf.as_slice(len)?, endpoint)?;
        // Do not drain_net_poll here — busybox ping uses sendto; 32× poll_ifaces
        // blocks for a long time (smoltcp + SOCKETS lock). Sockets drive RX in read/poll.
        Ok(written)
    }

    /// receive messages from a socket
    pub async fn sys_recvfrom(
        &mut self,
        sockfd: usize,
        mut buf: UserOutPtr<u8>,
        len: usize,
        flags: usize,
        src_addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        let _ = self.maybe_handle_tty_intr()?;
        linux_object::process::check_signals()?;
        info!(
            "sys_recvfrom: sockfd:{}, buffer:{:?}, length:{}, flags:{} , src_addr:{:?}, addrlen:{:?}",
            sockfd, buf, len, flags, src_addr, addrlen
        );
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let old_flags = file_like.flags();
        let force_nonblock =
            (flags & MSG_DONTWAIT) != 0 && !old_flags.contains(OpenFlags::NON_BLOCK);
        if force_nonblock {
            file_like.set_flags(old_flags | OpenFlags::NON_BLOCK)?;
        }
        debug!("FileLike {} flags: {:?}", sockfd, file_like.flags());
        let cap_len = len.min(super::SYSCALL_IO_MAX);
        let mut data = vec![0u8; cap_len];
        let socket = file_like.as_socket()?;
        let (result, endpoint) = if (flags & MSG_PEEK) != 0 {
            socket.peek(&mut data).await
        } else {
            socket.read(&mut data).await
        };
        if force_nonblock {
            let _ = file_like.set_flags(old_flags);
        }
        if let Ok(received) = result {
            if !src_addr.is_null() {
                let sockaddr_in = SockAddr::from(endpoint);
                sockaddr_in.write_to(src_addr, addrlen)?;
            }
            buf.write_array(&data[..received])?;
            Ok(received)
        } else {
            result
        }
    }

    /// Parse `SCM_RIGHTS` ancillary data and resolve the carried fd numbers to
    /// the sender's open files, ready to be queued on the peer.
    ///
    /// The byte walk is [`parse_scm_rights_fds`]; this half only turns numbers
    /// into files. An fd the sender does not have is `EBADF` for the **whole**
    /// `sendmsg`, as `scm_fp_copy()` does. Skipping it silently, which is what
    /// this used to do, is the worse answer by far: the receiver gets a short
    /// fd array with no indication, so a protocol that pairs the Nth fd with
    /// the Nth request — every one of them: DRI3, `wl_shm`, `wl_buffer` —
    /// silently pairs each fd with somebody else's request from there on.
    fn collect_scm_rights_fds(&self, ctrl: &[u8]) -> Result<Vec<Arc<dyn FileLike>>, LxError> {
        let raw_fds = parse_scm_rights_fds(ctrl)?;
        let proc = self.linux_process();
        let mut fds = Vec::with_capacity(raw_fds.len());
        for raw in raw_fds {
            if raw < 0 {
                return Err(LxError::EBADF);
            }
            fds.push(proc.get_file_like(FileDesc::from(raw as usize))?);
        }
        Ok(fds)
    }

    /// transmit a message to another socket
    #[allow(unsafe_code)]
    pub fn sys_sendmsg(
        &mut self,
        sockfd: usize,
        msg: UserInPtr<MsgHdr>,
        _flags: usize,
    ) -> SysResult {
        info!(
            "sys_sendmsg: sockfd:{:?}, msg:{:?}, flags:{}",
            sockfd, msg, _flags
        );
        let hdr = msg.read()?;
        self.sendmsg_hdr(sockfd, &hdr)
    }

    /// Core of `sendmsg` for an already-read header (shared with `sendmmsg`).
    fn sendmsg_hdr(&mut self, sockfd: usize, hdr: &MsgHdr) -> SysResult {
        let iov_ptr: UserInPtr<IoVecIn> = hdr.msg_iov.as_addr().into();
        let iovlen = hdr.msg_iovlen;
        let iovs = iov_ptr.read_iovecs(iovlen)?;
        let total = iovs.total_len();

        // Resolve the fd before gathering, so the socket's type can decide what
        // an oversized message means (and so EBADF/ENOTSOCK wins over a size
        // complaint, as it does on Linux).
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let sock_type = file_like.as_socket()?.socket_type();

        // A message longer than the bounded kernel buffer is NOT an error on a
        // stream socket. `sendmsg` may return a short count exactly like
        // `write`, and rejecting the whole call with `EINVAL` is what the
        // `sys_writev` doc block above describes killing every GLX client: xcb
        // saw errno 22, marked the connection dead and exited with "XIO: fatal
        // IO error 22 (Invalid argument)" right after its window appeared.
        // Firefox dies the same way, one layer up — its IPC channel treats any
        // `sendmsg` error as fatal and tears the channel down, so the browser
        // window opens and then immediately goes away.
        //
        // So gather only the first `SYSCALL_IO_MAX` bytes and report that count;
        // the caller resumes from it (Mozilla IPC, libwayland and stdio all
        // track a partial-write offset).
        //
        // Only for a socket we KNOW is a stream, though. Anything
        // message-oriented gets `EMSGSIZE`: a message boundary means a short
        // write would corrupt the message rather than short-change the writer,
        // and `EMSGSIZE` is what Linux returns for a message too large to send
        // atomically. `None` counts as message-oriented on purpose — it means
        // the socket does not report a type (netlink is one), and guessing
        // "stream" there would silently split, say, a netlink dump. Erring
        // toward a clean error beats erring toward silent corruption.
        let data = if total > super::SYSCALL_IO_MAX {
            if !matches!(sock_type, Some(SocketType::SOCK_STREAM)) {
                return Err(LxError::EMSGSIZE);
            }
            let mut buf = alloc::vec![0u8; super::SYSCALL_IO_MAX];
            let n = iovs.read_bytes_at(0, &mut buf)?;
            buf.truncate(n);
            buf
        } else {
            iovs.read_to_vec()?
        };

        // SCM_RIGHTS: resolve any attached fds before queueing the bytes.
        // `msg_controllen` is fully user-controlled; bound it before `read_array`
        // so a huge value cannot request a multi-GiB allocation (alloc abort) or
        // walk off the mapped control buffer. Linux bounds this by optmem_max.
        const CONTROL_MAX: usize = 64 * 1024;
        let passed_fds = if !hdr.msg_control.is_null() && hdr.msg_controllen >= 16 {
            if hdr.msg_controllen > CONTROL_MAX {
                return Err(LxError::EINVAL);
            }
            let ctrl = hdr.msg_control.read_array(hdr.msg_controllen)?;
            self.collect_scm_rights_fds(&ctrl)?
        } else {
            Vec::new()
        };

        let endpoint = if !hdr.msg_name.is_null() {
            let namelen = hdr.msg_namelen as usize;
            let endpoint =
                sockaddr_to_endpoint(read_sockaddr(hdr.msg_name.as_addr(), namelen)?, namelen)?;
            Some(endpoint)
        } else {
            None
        };

        let socket_fl = file_like.clone();
        let socket = socket_fl.as_socket()?;
        // Return the actual queued byte count (a TCP short write can queue less
        // than `data.len()`); reporting the full length silently drops the tail.
        // Queue the SCM_RIGHTS fds BEFORE the bytes so they are tagged with the
        // byte offset of the FIRST byte of this message. Linux delivers a passed
        // fd together with the recvmsg that returns the first accompanying data
        // byte; queueing after the write tagged the fds at the message's END, so
        // a peer that reads a header first and the body second (seatd/libseat)
        // got "Bad file descriptor" — the fd was withheld until the whole
        // message had been read.
        if !passed_fds.is_empty() {
            let _ = socket.send_fds(passed_fds);
        }
        let written = socket.write(&data, endpoint)?;
        Ok(written)
    }

    /// receive messages from a socket
    pub async fn sys_recvmsg(
        &mut self,
        sockfd: usize,
        msg: UserInOutPtr<MsgHdr>,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_recvmsg: sockfd:{}, msg:{:?}, flags:{}",
            sockfd, msg, flags
        );
        let mut hdr = msg.read()?;
        // Capture the user address before `msg` may be moved by `write_to_msg`.
        let msg_addr = msg.as_addr();

        let iov_ptr = hdr.msg_iov;
        let iovlen = hdr.msg_iovlen;
        let mut iovs = iov_ptr.read_iovecs(iovlen)?;
        let total_len = iovs.total_len().min(super::SYSCALL_IO_MAX);
        let mut data = vec![0u8; total_len];

        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let old_flags = file_like.flags();
        let force_nonblock =
            (flags & MSG_DONTWAIT) != 0 && !old_flags.contains(OpenFlags::NON_BLOCK);
        if force_nonblock {
            file_like.set_flags(old_flags | OpenFlags::NON_BLOCK)?;
        }
        let socket = file_like.as_socket()?;
        let (result, endpoint) = if (flags & MSG_PEEK) != 0 {
            socket.peek(&mut data).await
        } else {
            socket.read(&mut data).await
        };
        if force_nonblock {
            let _ = file_like.set_flags(old_flags);
        }

        let addr = hdr.msg_name;
        if let Ok(len) = result {
            iovs.write_from_buf(&data[..len])?;
            if !addr.is_null() {
                let sockaddr_in = SockAddr::from(endpoint);
                sockaddr_in.write_to_msg(msg)?;
            }
            // SCM_RIGHTS: install any fds the peer attached and emit a cmsg.
            let mut ctrl_written = 0usize;
            if !hdr.msg_control.is_null() && hdr.msg_controllen >= 16 {
                let max_fds = (hdr.msg_controllen - 16) / 4;
                let fds = socket.recv_fds(max_fds);
                if !fds.is_empty() {
                    let proc = self.linux_process();
                    let mut installed: Vec<i32> = Vec::with_capacity(fds.len());
                    for fl in fds {
                        installed.push(proc.add_file(fl)?.into());
                    }
                    let cbuf = build_scm_rights_cmsg(&installed);
                    ctrl_written = cbuf.len().min(hdr.msg_controllen);
                    hdr.msg_control.write_array(&cbuf[..ctrl_written])?;
                }
            }
            // Linux ALWAYS reports how much ancillary data it wrote through
            // msg_controllen — 0 when there is none. Leaving the caller's IN
            // value (the buffer CAPACITY) there handed strict cmsg parsers a
            // buffer's worth of stale user memory to walk as if it were
            // kernel-written control headers: rustix (wayland-rs / lunarbg)
            // read a garbage cmsg_len there and panicked with
            // "range start index 18446744069414584321 out of range for slice
            // of length 136" — 136 being exactly the untouched capacity.
            // libwayland's C macros merely skidded over the same garbage.
            if !hdr.msg_control.is_null() {
                let controllen_addr = msg_addr + core::mem::offset_of!(MsgHdr, msg_controllen);
                let mut p = UserOutPtr::<usize>::from(controllen_addr);
                p.write(ctrl_written)?;
            }
            // Report truncation (and any other recv flags) via msg_flags.
            let msg_flags = socket.take_msg_flags();
            {
                let flags_addr = msg_addr + core::mem::offset_of!(MsgHdr, msg_flags);
                let mut p = UserOutPtr::<i32>::from(flags_addr);
                p.write(msg_flags)?;
            }
        }

        result
    }

    /// assigns the address specified by addr to the socket referred to by the file descriptor sockfd
    pub fn sys_bind(
        &mut self,
        sockfd: usize,
        addr: UserInPtr<SockAddr>,
        addrlen: usize,
    ) -> SysResult {
        info!(
            "sys_bind: sockfd:{:?}, addr:{:?}, addrlen:{}",
            sockfd, addr, addrlen
        );
        let endpoint = sockaddr_to_endpoint(read_sockaddr(addr.as_addr(), addrlen)?, addrlen)?;
        debug!("sys_bind: fd:{} bind to {:?}", sockfd, endpoint);

        let proc = self.linux_process();
        if let Endpoint::Unix(path) = &endpoint {
            if !path.is_empty() {
                // Abstract-namespace sockets (leading NUL, e.g. X11's
                // `\0/tmp/.X11-unix/X0`) live only in the in-kernel registry and
                // have no filesystem node; only pathname sockets get a node.
                let is_abstract = path.starts_with('\0');
                if !is_abstract {
                    let (dir_path, file_name) = split_path(path);
                    match proc.lookup_inode_at(FileDesc::CWD, dir_path, true) {
                        Ok(dir_inode) => {
                            if dir_inode.find(file_name).is_err() {
                                if let Err(err) = dir_inode.create(
                                    file_name,
                                    linux_object::fs::vfs::FileType::Socket,
                                    0o666,
                                ) {
                                    warn!(
                                        "sys_bind: unable to create unix socket node {:?}: {:?}; continuing with in-kernel registration only",
                                        file_name, err
                                    );
                                } else {
                                    linux_object::fs::dcache_invalidate();
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                "sys_bind: unable to lookup unix socket directory {:?}: {:?}; continuing with in-kernel registration only",
                                dir_path, err
                            );
                        }
                    }
                }

                let file_like = proc.get_file_like(sockfd.into())?;
                if let Ok(unix) = file_like.clone().downcast_arc::<UnixSocketState>() {
                    UnixSocketState::register(path.clone(), unix)?;
                }
            }
        }

        let file_like = proc.get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.bind(endpoint)
    }

    /// marks the socket referred to by sockfd as a passive socket,
    /// that is, as a socket that will be used to accept incoming connection
    pub fn sys_listen(&mut self, sockfd: usize, backlog: usize) -> SysResult {
        info!("sys_listen: fd:{}, backlog:{}", sockfd, backlog);
        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.listen()
    }

    /// shutdown a socket
    pub fn sys_shutdown(&mut self, sockfd: usize, howto: usize) -> SysResult {
        info!("sys_shutdown: sockfd:{}, howto:{}", sockfd, howto);
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.shutdown(howto)
    }

    /// accept() is used with connection-based socket types (SOCK_STREAM, SOCK_SEQPACKET).
    /// It extracts the first connection request on the queue of pending connections
    /// for the listening socket, sockfd, creates a new connected socket, and returns
    /// a new file descriptor referring to that socket.
    /// The newly created socket is not in the listening state.
    /// The original socket sockfd is unaffected by this call.
    pub async fn sys_accept(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        self.sys_accept4(sockfd, addr, addrlen, 0).await
    }

    /// Like [`Self::sys_accept`], but takes an extra `flags` argument that may
    /// carry `SOCK_NONBLOCK` (`0x800`) and/or `SOCK_CLOEXEC` (`0x80000`),
    /// applied to the newly accepted socket. These share the bit values of
    /// `O_NONBLOCK`/`O_CLOEXEC`, so they map directly onto [`OpenFlags`].
    pub async fn sys_accept4(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_accept4: sockfd:{}, addr:{:?}, addrlen={:?}, flags={:#x}",
            sockfd, addr, addrlen, flags
        );
        // Validate flags BEFORE accept() consumes a connection from the queue.
        // SOCK_NONBLOCK / SOCK_CLOEXEC requested for the accepted socket; any
        // other bit is invalid (GLib's GDBus path only ever passes these two).
        // Previously this ran after accept(), so a bad flag accepted then
        // dropped an established client connection.
        const SOCK_NONBLOCK: usize = 0o4000;
        const SOCK_CLOEXEC: usize = 0o2000000;
        if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
            return Err(LxError::EINVAL);
        }

        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let (new_socket, remote_endpoint) = file_like.clone().as_socket()?.accept().await?;
        debug!(
            "FileLike{} flags: {:?}, New flags: {:?}",
            sockfd,
            file_like.flags(),
            new_socket.flags()
        );

        if flags != 0 {
            let new_flags = OpenFlags::from_bits_truncate(flags);
            new_socket.set_flags(new_flags)?;
        }

        // Copy the peer address out BEFORE installing the fd, so a bad addr/
        // addrlen pointer fails the syscall (EFAULT) without leaking the
        // accepted fd into the process table (Linux copies the address out
        // before fd_install).
        if !addr.is_null() {
            let sockaddr_in = SockAddr::from(remote_endpoint);
            sockaddr_in.write_to(addr, addrlen)?;
        }
        let new_fd = self.linux_process().add_socket(new_socket)?;
        Ok(new_fd.into())
    }

    /// returns the current address to which the socket sockfd is bound,
    /// in the buffer pointed to by addr.
    pub fn sys_getsockname(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getsockname: sockfd:{}, addr:{:?}, addrlen:{:?}",
            sockfd, addr, addrlen
        );
        if addr.is_null() {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let endpoint = file_like
            .clone()
            .as_socket()?
            .endpoint()
            .ok_or(LxError::EINVAL)?;
        SockAddr::from(endpoint).write_to(addr, addrlen)?;
        Ok(0)
    }

    /// returns the address of the peer connected to the socket sockfd,
    /// in the buffer pointed to by addr.
    pub fn sys_getpeername(
        &mut self,
        sockfd: usize,
        addr: UserOutPtr<SockAddr>,
        addrlen: UserInOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getpeername: sockfd:{}, addr:{:?}, addrlen:{:?}",
            sockfd, addr, addrlen
        );
        // smoltcp tcp sockets do not support backlog
        // open multiple sockets for each connection
        if addr.is_null() {
            return Err(LxError::EINVAL);
        }
        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        let remote_endpoint = file_like
            .clone()
            .as_socket()?
            .remote_endpoint()
            .ok_or(LxError::EINVAL)?;
        SockAddr::from(remote_endpoint).write_to(addr, addrlen)?;
        Ok(0)
    }

    /// creates a pair of connected sockets in the specified domain, of the specified type,
    /// and using the optionally specified protocol.
    pub fn sys_socketpair(
        &mut self,
        domain: usize,
        _type: usize,
        protocol: usize,
        mut sv: UserOutPtr<i32>,
    ) -> SysResult {
        info!(
            "sys_socketpair: domain:{}, type:{}, protocol:{}",
            domain, _type, protocol
        );
        if domain != Domain::AF_UNIX as usize {
            return Err(LxError::EAFNOSUPPORT);
        }
        let proc = self.linux_process();
        let socket1 = Arc::new(UnixSocketState::default());
        let socket2 = Arc::new(UnixSocketState::default());
        UnixSocketState::connect_pair(&socket1, &socket2);
        // Same as `sys_socket`: keep the requested type so `sendmsg` can tell a
        // datagram/seqpacket pair from a stream one. `SOCKET_TYPE_MASK` strips
        // the SOCK_NONBLOCK / SOCK_CLOEXEC bits handled just below; an
        // unrecognized type leaves the SOCK_STREAM default, which is what this
        // transport actually is.
        if let Ok(t) = SocketType::try_from(_type & SOCKET_TYPE_MASK) {
            socket1.set_socket_type(t);
            socket2.set_socket_type(t);
        }
        // The type argument packs SOCK_NONBLOCK / SOCK_CLOEXEC alongside the
        // socket type (same bit values as O_NONBLOCK / O_CLOEXEC, like
        // accept4). These were silently dropped, handing out BLOCKING sockets
        // to callers whose event loops assume nonblocking semantics —
        // Firefox's WaylandProxy (socketpair(AF_UNIX, SOCK_STREAM |
        // SOCK_NONBLOCK | SOCK_CLOEXEC)) drains with read-until-EAGAIN, so a
        // blocking pair wedged its forwarding thread and Wayland startup died
        // with "ProxiedConnection: broken source socket".
        const SOCK_NONBLOCK: usize = 0o4000;
        const SOCK_CLOEXEC: usize = 0o2000000;
        let flag_bits = _type & (SOCK_NONBLOCK | SOCK_CLOEXEC);
        if flag_bits != 0 {
            let new_flags = OpenFlags::from_bits_truncate(flag_bits);
            socket1.set_flags(new_flags)?;
            socket2.set_flags(new_flags)?;
        }
        let fd1 = proc.add_socket(socket1)?;
        // Taken back if the caller never gets the numbers, like `pipe2`.
        hand_out_pair(
            fd1,
            || proc.add_socket(socket2),
            |fd1, fd2| {
                sv.write_array(&[fd1.into(), fd2.into()])?;
                Ok(())
            },
            |fd| {
                let _ = proc.close_file(fd);
            },
        )?;
        Ok(0)
    }

    /// `sendmmsg`: send an array of messages in one call. glibc's resolver
    /// fires its parallel A+AAAA DNS queries through this; without it every
    /// Firefox name lookup spammed `unknown syscall: SENDMMSG` and fell back.
    /// Sequential delegation to the sendmsg core; each entry's `msg_len` is
    /// written back. On error: fail if nothing was sent, else report the count.
    pub fn sys_sendmmsg(
        &mut self,
        sockfd: usize,
        msgvec: UserInOutPtr<u8>,
        vlen: usize,
        _flags: usize,
    ) -> SysResult {
        info!(
            "sys_sendmmsg: sockfd:{}, msgvec:{:?}, vlen:{}",
            sockfd, msgvec, vlen
        );
        const MMSG_MAX: usize = 64;
        let base = msgvec.as_addr();
        let stride = core::mem::size_of::<MsgHdr>() + 8; // + u32 msg_len + pad
        let mut sent = 0usize;
        for i in 0..vlen.min(MMSG_MAX) {
            let hdr_ptr: UserInPtr<MsgHdr> = (base + i * stride).into();
            let hdr = hdr_ptr.read()?;
            match self.sendmsg_hdr(sockfd, &hdr) {
                Ok(n) => {
                    let mut len_ptr: UserOutPtr<u32> =
                        (base + i * stride + core::mem::size_of::<MsgHdr>()).into();
                    len_ptr.write(n as u32)?;
                    sent += 1;
                }
                Err(e) => {
                    if sent == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        Ok(sent)
    }

    /// `recvmmsg`: receive up to `vlen` messages. The first receive honors the
    /// caller's blocking mode; subsequent ones are forced non-blocking so the
    /// call returns as soon as the queue drains (Linux semantics without the
    /// timeout extra, which the resolver does not rely on).
    pub async fn sys_recvmmsg(
        &mut self,
        sockfd: usize,
        msgvec: UserInOutPtr<u8>,
        vlen: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "sys_recvmmsg: sockfd:{}, msgvec:{:?}, vlen:{}",
            sockfd, msgvec, vlen
        );
        const MMSG_MAX: usize = 64;
        let base = msgvec.as_addr();
        let stride = core::mem::size_of::<MsgHdr>() + 8;
        let mut received = 0usize;
        for i in 0..vlen.min(MMSG_MAX) {
            let hdr_ptr: UserInOutPtr<MsgHdr> = (base + i * stride).into();
            let per_flags = if i == 0 { flags } else { flags | MSG_DONTWAIT };
            match self.sys_recvmsg(sockfd, hdr_ptr, per_flags).await {
                Ok(n) => {
                    let mut len_ptr: UserOutPtr<u32> =
                        (base + i * stride + core::mem::size_of::<MsgHdr>()).into();
                    len_ptr.write(n as u32)?;
                    received += 1;
                }
                Err(LxError::EAGAIN) if received > 0 => break,
                Err(e) => {
                    if received == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        Ok(received)
    }

    /// Eclipse-specific DNS/hosts lookup for userland shims (`libeclipse_dns.so`).
    pub fn sys_eclipse_dns_query(
        &self,
        name: UserInPtr<u8>,
        name_len: usize,
        family: usize,
        out: UserOutPtr<linux_object::net::dns::DnsResultEntry>,
        out_max: usize,
    ) -> SysResult {
        use linux_object::fs::dns_vfs_root;
        use linux_object::net::dns::{self, DnsFamily};

        if out_max == 0 {
            return Ok(0);
        }
        if name_len == 0 || name_len > 253 {
            return Err(LxError::EINVAL);
        }
        let hostname = name.as_str(name_len).map_err(|_| LxError::EINVAL)?;
        let root_inode = dns_vfs_root().ok_or(LxError::ENOENT)?;
        let addrs = dns::resolve(&root_inode, hostname, DnsFamily::from_usize(family))?;
        let n = addrs.len().min(out_max);
        for (i, ip) in addrs.iter().take(n).enumerate() {
            out.add(i)
                .write(linux_object::net::dns::DnsResultEntry::from_ip(*ip))?;
        }
        Ok(n as _)
    }
}

/// The unix-socket control path: passing a file descriptor from one process to
/// another, which had no tests at all.
///
/// This is how a graphical desktop hands buffers around — DRI3 passes a GPU
/// buffer's fd over the X11 socket, `wl_shm` passes a memfd over the Wayland
/// one — so everything here runs thousands of times a second in a session and
/// never once in CI, which has no display.
///
/// Every field of a control buffer comes from userspace, `cmsg_len` included,
/// so the walk is written to survive whatever is in it. The round-trip test at
/// the end is the one that matters most: the 16-byte header is written in two
/// places, once by the parser and once by the builder, and nothing but that
/// test keeps them agreeing.
#[cfg(test)]
mod scm_rights_tests {
    use super::*;

    /// A control buffer holding one cmsg with `level`, `typ` and `payload`,
    /// padded to `CMSG_ALIGN` so another can follow.
    fn cmsg(level: i32, typ: i32, payload: &[u8]) -> Vec<u8> {
        let cmsg_len = CMSG_HDR_LEN + payload.len();
        let mut b = Vec::new();
        b.extend_from_slice(&(cmsg_len as u64).to_ne_bytes());
        b.extend_from_slice(&level.to_ne_bytes());
        b.extend_from_slice(&typ.to_ne_bytes());
        b.extend_from_slice(payload);
        while b.len() % 8 != 0 {
            b.push(0);
        }
        b
    }

    /// The bytes of `fds` as a C `int` array.
    fn fd_bytes(fds: &[i32]) -> Vec<u8> {
        fds.iter().flat_map(|f| f.to_ne_bytes()).collect()
    }

    /// A cmsg with a hand-chosen `cmsg_len`, for the malformed cases.
    fn cmsg_raw(cmsg_len: u64, level: i32, typ: i32, payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&cmsg_len.to_ne_bytes());
        b.extend_from_slice(&level.to_ne_bytes());
        b.extend_from_slice(&typ.to_ne_bytes());
        b.extend_from_slice(payload);
        b
    }

    #[test]
    fn one_message_yields_the_descriptors_it_carries() {
        let ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[7, 9, 11]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![7, 9, 11]));
    }

    #[test]
    fn a_message_of_another_level_or_type_carries_no_descriptors() {
        // SCM_CREDENTIALS (type 2) and IPPROTO_IP (level 0) both hold plain
        // data; reading it as descriptor numbers would install whatever the
        // numbers happened to be.
        let creds = cmsg(SOL_SOCKET_LEVEL, 2, &fd_bytes(&[3, 4, 5]));
        assert_eq!(parse_scm_rights_fds(&creds), Ok(vec![]));
        let ip = cmsg(0, SCM_RIGHTS, &fd_bytes(&[3, 4, 5]));
        assert_eq!(parse_scm_rights_fds(&ip), Ok(vec![]));
    }

    #[test]
    fn several_messages_are_walked_in_order_across_the_padding() {
        // The first payload is 4 bytes, so its cmsg is 20 and the next one
        // starts at 24. Getting the alignment wrong reads the second header
        // out of the middle of the first message.
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3]));
        assert_eq!(ctrl.len(), 24);
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, 2, &fd_bytes(&[99])));
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[5, 6])));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3, 5, 6]));
    }

    #[test]
    fn a_trailing_partial_descriptor_is_not_one() {
        // `cmsg_len` covering 6 bytes of payload is one `int` and a half; the
        // kernel divides and drops the remainder rather than reading past it.
        let ctrl = cmsg_raw(
            (CMSG_HDR_LEN + 6) as u64,
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &[1, 0, 0, 0, 2, 0],
        );
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![1]));
    }

    #[test]
    fn a_length_past_the_end_of_the_buffer_stops_the_walk() {
        let ctrl = cmsg_raw(4096, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]));
    }

    #[test]
    fn a_length_that_wraps_the_address_space_stops_the_walk() {
        // `off + cmsg_len` overflowing would land back under `ctrl.len()` and
        // then slice with start > end, which is a kernel panic.
        for len in [u64::MAX, u64::MAX - 7, (usize::MAX as u64) - 16] {
            let ctrl = cmsg_raw(len, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "cmsg_len {}", len);
        }
    }

    #[test]
    fn a_length_that_wraps_past_a_message_already_walked_stops_the_walk() {
        // The wrap that actually bites. At offset 0 a huge `cmsg_len` lands
        // past the end of the buffer and is refused on size alone; it takes a
        // first, well-formed message to move the walk forward before
        // `off + cmsg_len` can come back around to a SMALL number that passes
        // the bounds check. The slice is then `ctrl[off+16..that]`, with the
        // start past the end — a kernel panic inside `sendmsg`, from a control
        // buffer any process can hand over.
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3]));
        assert_eq!(ctrl.len(), 24);
        ctrl.extend(cmsg_raw(u64::MAX - 20, SOL_SOCKET_LEVEL, SCM_RIGHTS, &[]));
        assert_eq!(ctrl.len(), 40);
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3]));
    }

    #[test]
    fn a_length_shorter_than_its_own_header_stops_the_walk() {
        for len in [0u64, 1, 8, 15] {
            let ctrl = cmsg_raw(len, SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, 4]));
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "cmsg_len {}", len);
        }
    }

    #[test]
    fn an_empty_message_is_stepped_over_rather_than_looped_on() {
        // `cmsg_len` 16 is a well-formed SCM_RIGHTS carrying nothing. The walk
        // has to step past it and find the next one.
        //
        // The `s > 0` guard on the step is a second lock on the same door: a
        // step can only be 0 if `cmsg_len` is, and a `cmsg_len` under 16 has
        // already ended the walk. It is kept because losing the length check
        // would otherwise turn into an unkillable loop inside a syscall rather
        // than a wrong answer, and no test can tell the two locks apart.
        let mut ctrl = cmsg_raw(16, SOL_SOCKET_LEVEL, SCM_RIGHTS, &[]);
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[8])));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![8]));
    }

    #[test]
    fn a_buffer_too_short_for_a_header_carries_nothing() {
        for n in 0..CMSG_HDR_LEN {
            let ctrl = vec![0xffu8; n];
            assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![]), "{} bytes", n);
        }
    }

    #[test]
    fn the_descriptor_cap_is_the_number_linux_uses() {
        // Every other test here builds its fd array *from* the constant, so
        // they all move with it and none of them would notice it changing.
        // This is an ABI number (`SCM_MAX_FD`, include/net/scm.h), not a knob.
        assert_eq!(SCM_MAX_FD, 253);
    }

    #[test]
    fn more_descriptors_than_linux_allows_is_einval() {
        // Without the cap, one sendmsg with the 64 KiB control buffer the
        // caller is allowed asks the receiver to install some 16000 fds.
        let ok = cmsg(
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &fd_bytes(&(0..SCM_MAX_FD as i32).collect::<Vec<_>>()),
        );
        assert_eq!(parse_scm_rights_fds(&ok).map(|v| v.len()), Ok(SCM_MAX_FD));

        let too_many = cmsg(
            SOL_SOCKET_LEVEL,
            SCM_RIGHTS,
            &fd_bytes(&(0..SCM_MAX_FD as i32 + 1).collect::<Vec<_>>()),
        );
        assert_eq!(parse_scm_rights_fds(&too_many), Err(LxError::EINVAL));
    }

    #[test]
    fn the_cap_counts_across_messages_and_not_within_one() {
        // Two messages of 200 fds each is 400, which Linux refuses just the
        // same as one message of 400.
        let half = || fd_bytes(&(0..200i32).collect::<Vec<_>>());
        let mut ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &half());
        ctrl.extend(cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &half()));
        assert_eq!(parse_scm_rights_fds(&ctrl), Err(LxError::EINVAL));
    }

    #[test]
    fn a_negative_descriptor_number_comes_back_as_it_was_written() {
        // The walk reports what is on the wire; `collect_scm_rights_fds` is
        // the half that turns a number into a file and answers EBADF.
        let ctrl = cmsg(SOL_SOCKET_LEVEL, SCM_RIGHTS, &fd_bytes(&[3, -1, 4]));
        assert_eq!(parse_scm_rights_fds(&ctrl), Ok(vec![3, -1, 4]));
    }

    #[test]
    fn what_recvmsg_writes_is_what_sendmsg_reads() {
        // The one that keeps the two halves of the header honest.
        for fds in [
            vec![],
            vec![0],
            vec![3, 4, 5],
            vec![i32::MAX, 1, 2, 3, 4, 5, 6, 7],
        ] {
            let built = build_scm_rights_cmsg(&fds);
            assert_eq!(parse_scm_rights_fds(&built), Ok(fds.clone()), "{:?}", fds);
        }
    }

    #[test]
    fn the_built_message_is_the_length_it_declares() {
        // A caller walks the buffer with `cmsg_len`, so a header that lies
        // about its own size sends it into the next message or off the end.
        let built = build_scm_rights_cmsg(&[3, 4, 5]);
        let declared = u64::from_ne_bytes(built[..8].try_into().unwrap()) as usize;
        assert_eq!(declared, built.len());
        assert_eq!(built.len(), CMSG_HDR_LEN + 3 * 4);
        assert_eq!(
            i32::from_ne_bytes(built[8..12].try_into().unwrap()),
            SOL_SOCKET_LEVEL
        );
        assert_eq!(
            i32::from_ne_bytes(built[12..16].try_into().unwrap()),
            SCM_RIGHTS
        );
    }

    #[test]
    fn getsockopt_reports_what_it_wrote_and_not_what_it_had() {
        // A `struct ucred` is 12 bytes; a caller asking SO_PEERCRED into an
        // `int` gets 4 and must be told 4, or it reads 8 bytes of its own
        // stack as if the kernel had filled them.
        assert_eq!(sockopt_out_len(4, 12), 4);
        assert_eq!(sockopt_out_len(12, 12), 12);
        assert_eq!(sockopt_out_len(64, 12), 12);
        assert_eq!(sockopt_out_len(0, 12), 0);
    }
}
