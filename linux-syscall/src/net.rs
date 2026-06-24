use super::*;
use core::mem::size_of;
use kernel_hal::user::UserInOutPtr;
use linux_object::{
    fs::{split_path, FileLike, OpenFlags},
    net::*,
};

const MSG_DONTWAIT: usize = 0x40;
const MSG_PEEK: usize = 0x2;

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
/// Copy exactly `addrlen` bytes (capped at the union size) byte-wise (alignment
/// 1) into a zeroed buffer instead, matching Linux `move_addr_to_kernel`
/// semantics.
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
                return match UnixSocketState::lookup(path) {
                    None => Err(LxError::ECONNREFUSED),
                    Some(server) => {
                        if !server.is_listening() {
                            return Err(LxError::ECONNREFUSED);
                        }
                        // Establish the connection now: create the server's end,
                        // wire it to the client, and queue it for accept(). Wiring
                        // at connect time (rather than in accept) lets the client
                        // send its first bytes — e.g. the X11 handshake — before
                        // the server has accepted, instead of getting ENOTCONN.
                        let server_side = UnixSocketState::new();
                        server_side.set_path(server.bound_path());
                        UnixSocketState::connect_pair(&client, &server_side);
                        server.push_accept(server_side);
                        Ok(0)
                    }
                };
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
        mut optval: UserOutPtr<u32>,
        mut optlen: UserOutPtr<u32>,
    ) -> SysResult {
        info!(
            "sys_getsockopt: sockfd:{}, level:{}, optname:{}, optval:{:?} , optlen:{:?}",
            sockfd, level, optname, optval, optlen
        );
        let level = match Level::try_from(level) {
            Ok(level) => level,
            Err(_) => {
                // Unknown levels (e.g. SOL_PACKET=263) — return Ok(0) to be lenient.
                warn!("getsockopt: unsupported level: {}", level);
                optval.write(0)?;
                optlen.write(size_of::<u32>() as u32)?;
                return Ok(0);
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
                    optval.write_array(&[pid as u32, 0u32, 0u32])?;
                    optlen.write(12)?; // sizeof(struct ucred)
                    return Ok(0);
                }
                let optname = match SolOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        error!("invalid optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };

                let file_like = self.linux_process().get_file_like(sockfd.into())?;
                let (recv_buf_ca, send_buf_ca) = file_like
                    .clone()
                    .as_socket()?
                    .get_buffer_capacity()
                    .unwrap_or((64 * 1024, 64 * 1024));
                debug!("sys_getsockopt recv and send buffer capacity: {}, {}. optval: {:?}, optlen: {:?}", recv_buf_ca, send_buf_ca, optval.check(), optlen.check());

                match optname {
                    SolOptname::SNDBUF => {
                        optval.write(send_buf_ca as u32)?;
                        optlen.write(size_of::<u32>() as u32)?;
                        Ok(0)
                    }
                    SolOptname::RCVBUF => {
                        optval.write(recv_buf_ca as u32)?;
                        optlen.write(size_of::<u32>() as u32)?;
                        Ok(0)
                    }
                    SolOptname::REUSEADDR => {
                        optval.write(1)?;
                        optlen.write(size_of::<u32>() as u32)?;
                        Ok(0)
                    }
                    SolOptname::ERROR => {
                        optval.write(0)?;
                        optlen.write(size_of::<u32>() as u32)?;
                        Ok(0)
                    }
                    SolOptname::LINGER => {
                        // Return zero-linger: l_onoff=0, l_linger=0
                        optval.write(0)?;
                        optlen.write(8)?; // sizeof(struct linger)
                        Ok(0)
                    }
                }
            }
            Level::IPPROTO_TCP => {
                let optname = match TcpOptname::try_from(optname) {
                    Ok(optname) => optname,
                    Err(_) => {
                        error!("invalid optname: {}", optname);
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
                        error!("invalid optname: {}", optname);
                        return Err(LxError::ENOPROTOOPT);
                    }
                };
                match optname {
                    IpOptname::HDRINCL => {
                        optval.write(0)?;
                        optlen.write(size_of::<u32>() as u32)?;
                        Ok(0)
                    }
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
        file_like
            .clone()
            .as_socket()?
            .write(buf.as_slice(len)?, endpoint)?;
        // Do not drain_net_poll here — busybox ping uses sendto; 32× poll_ifaces
        // blocks for a long time (smoltcp + SOCKETS lock). Sockets drive RX in read/poll.
        Ok(len)
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
        if let Err(e) = linux_object::process::check_signals() {
            return Err(e);
        }
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
        let iov_ptr: UserInPtr<IoVecIn> = unsafe { core::mem::transmute(hdr.msg_iov) };
        let iovlen = hdr.msg_iovlen;
        let iovs = iov_ptr.read_iovecs(iovlen)?;
        if iovs.total_len() > super::SYSCALL_IO_MAX {
            return Err(LxError::EINVAL);
        }
        let data = iovs.read_to_vec()?;

        let endpoint = if !hdr.msg_name.is_null() {
            let namelen = hdr.msg_namelen as usize;
            let endpoint =
                sockaddr_to_endpoint(read_sockaddr(hdr.msg_name.as_addr(), namelen)?, namelen)?;
            Some(endpoint)
        } else {
            None
        };

        let file_like = self.linux_process().get_file_like(sockfd.into())?;
        file_like.clone().as_socket()?.write(&data, endpoint)?;
        Ok(data.len())
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
        let hdr = msg.read()?;

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
        info!(
            "sys_accept: sockfd:{}, addr:{:?}, addrlen={:?}",
            sockfd, addr, addrlen
        );
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

        let new_fd = self.linux_process().add_socket(new_socket)?;
        if !addr.is_null() {
            let sockaddr_in = SockAddr::from(remote_endpoint);
            sockaddr_in.write_to(addr, addrlen)?;
        }
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
        let fd1 = proc.add_socket(socket1)?;
        let fd2 = proc.add_socket(socket2)?;
        sv.write_array(&[fd1.into(), fd2.into()])?;
        Ok(0)
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
