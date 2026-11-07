//! The client against a real NFSv3 server, over a real TCP socket.
//!
//! Everything else in this module is measured against a fake: a scripted
//! connector, a transport that hands back bytes a test wrote. That is the
//! right way to measure *which* call was made and *what* was done with the
//! answer, and it is the only way to measure a reply no real server will
//! send. What it cannot measure is whether a real server accepts what this
//! client sends, because the fake accepts whatever it is given.
//!
//! So these tests talk to an actual `nfsd`. They are `#[ignore]`d: they need
//! a server listening, and a suite that fails on the machines that have none
//! is a suite people learn to ignore. To run them:
//!
//! ```text
//! apt-get install -y nfs-ganesha rpcbind
//! # /etc/ganesha/ganesha.conf: NFS_Port 2049, MNT_Port 20048,
//! # NFS_Protocols 3, one EXPORT of /srv/ganesha, Squash = No_Root_Squash
//! rpcbind -w && ganesha.nfsd -F -f /etc/ganesha/ganesha.conf &
//! cargo test -p linux-object --lib real_server -- --ignored --test-threads=1
//! ```
//!
//! # Why the transport here is `std` and not the kernel's
//!
//! [`TcpConnector`](super::connect::TcpConnector) dials through this
//! kernel's own smoltcp stack, which on the host has no device under it: its
//! handshakes time out, which is exactly what `connect_tests` measures. A
//! host test cannot reach a socket on the machine through it.
//!
//! That bounds what these tests cover, and the bound is worth stating
//! plainly: **everything from the RPC record marking upwards is measured
//! here against a real server, and the kernel's TCP path is not.** The
//! framing, the XDR, the credential, the port discovery, the MOUNT
//! conversation, the NFS procedures and the loops over them are the same
//! code in both cases; only the four methods of the transport differ, and
//! those have their own suite. Measuring the kernel's socket needs a running
//! kernel, which means QEMU, not `cargo test`.

// `linux-object` is `#![no_std]`, so `std` is not in the extern prelude
// even in a test build where it is linked (`async_std` pulls it in). This is
// the one module that needs it, and it needs it only because the host's
// sockets are the only ones that can reach a server on this machine.
extern crate std;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::error::{LxError, LxResult};

use super::client::{RpcTransport, TransportError};
use super::connect::Connector;
use super::mount::connect_export;
use super::nfs3::Ftype3;
use super::nfs3_write::{CreateHow, SetAttr, StableHow};
use super::source::{NfsOptions, NfsSource};

/// Where the server in the module docs listens.
const EXPORT: &str = "127.0.0.1:/srv/ganesha";

/// A transport over a host socket.
///
/// Non-blocking, because [`RpcTransport::recv`] must answer `Ok(0)` for "no
/// bytes yet" rather than block: the client's own wait loop is what applies
/// the timeout, and a blocking read would take the timeout away from it and
/// hand it to the kernel's socket instead. That is the same shape the
/// kernel transport has, which is what makes this a substitution of one
/// transport for another and not a different client.
struct StdTransport {
    sock: TcpStream,
    started: Instant,
}

impl RpcTransport for StdTransport {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        self.sock
            .set_nonblocking(false)
            .map_err(|_| TransportError::Closed)?;
        let r = self
            .sock
            .write_all(data)
            .map_err(|_| TransportError::Closed);
        self.sock
            .set_nonblocking(true)
            .map_err(|_| TransportError::Closed)?;
        r
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.sock.read(buf) {
            Ok(0) => Err(TransportError::Closed),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(_) => Err(TransportError::Closed),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn poll(&mut self) {
        // Nothing to drive: the host's stack runs itself. A short sleep so
        // the spin that waits for a reply does not burn a core while the
        // server is thinking.
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Drop for StdTransport {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

/// A connector over host sockets, counting the ports it dialled.
struct StdConnector {
    host: String,
    dialled: Vec<u16>,
}

impl StdConnector {
    fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            dialled: Vec::new(),
        }
    }
}

impl Connector for StdConnector {
    type Transport = StdTransport;

    fn connect(&mut self, port: u16, timeout_ms: u64) -> LxResult<Self::Transport> {
        self.dialled.push(port);
        let addr: SocketAddr = alloc::format!("{}:{}", self.host, port)
            .parse()
            .map_err(|_| LxError::EINVAL)?;
        let sock = TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms.max(1)))
            .map_err(|_| LxError::ECONNREFUSED)?;
        sock.set_nodelay(true).map_err(|_| LxError::EIO)?;
        sock.set_nonblocking(true).map_err(|_| LxError::EIO)?;
        Ok(StdTransport {
            sock,
            started: Instant::now(),
        })
    }
}

fn a_source() -> NfsSource {
    NfsSource::parse(EXPORT).expect("the export string")
}

/// Options for a test: short waits, since a server that is there answers in
/// milliseconds and one that is not should fail the test rather than hold it.
fn test_options() -> NfsOptions {
    NfsOptions {
        timeout_ms: 5_000,
        retrans: 1,
        ..NfsOptions::default()
    }
}

/// The whole mount against the real server, and the ports it took to get
/// there.
macro_rules! mounted {
    () => {{
        let mut c = StdConnector::new("127.0.0.1");
        let m = connect_export(&mut c, &a_source(), &test_options()).expect("the mount");
        (m, c.dialled)
    }};
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn the_ports_come_from_the_servers_own_rpcbind() {
    let (mounted, dialled) = mounted!();
    assert_eq!(
        dialled[0], 111,
        "the first thing asked is rpcbind: {:?}",
        dialled
    );
    assert_eq!(dialled.len(), 4, "portmap twice, mountd, then nfsd");
    assert_eq!(*dialled.last().unwrap(), mounted.nfs_port);
    // What the server in the module docs is configured with. Asserted rather
    // than assumed: if rpcbind answered with something else, the test that
    // reads a file would still pass and this is the one that says why.
    assert_eq!(mounted.nfs_port, 2049);
    assert_eq!(mounted.mount_port, 20048);
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn the_transfer_sizes_are_the_servers_and_not_the_ones_asked_for() {
    let (mounted, _) = mounted!();
    let session = mounted.fs.session().lock();
    let info = *session.fsinfo().expect("FSINFO was asked at mount time");
    assert!(info.rtmax > 0 && info.wtmax > 0, "{:?}", info);
    assert!(
        session.read_size() <= info.rtmax as usize,
        "the client kept {} over the server's {}",
        session.read_size(),
        info.rtmax
    );
    assert!(session.write_size() <= info.wtmax as usize);
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn a_file_the_server_has_is_looked_up_and_read() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    let found = session.lookup(&root, "fichero.txt").expect("the lookup");
    let mut buf = vec![0u8; 64];
    let n = session.read_into(&found.fh, 0, &mut buf).expect("the read");
    assert_eq!(&buf[..n], b"hola desde nfs");
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn a_directory_the_server_has_is_listed_with_every_name_in_it() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    for plus in [false, true] {
        let entries = session.readdir_all(&root, plus).expect("the listing");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for want in [".", "..", "fichero.txt", "enlace", "sub"] {
            assert!(names.contains(&want), "plus={}: {:?}", plus, names);
        }
    }
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn a_symbolic_link_the_server_has_reads_back_as_its_target() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    let found = session.lookup(&root, "enlace").expect("the lookup");
    assert_eq!(found.attr.map(|a| a.ftype), Some(Ftype3::Lnk));
    assert_eq!(
        session.readlink(&found.fh).expect("READLINK"),
        "fichero.txt"
    );
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn what_this_client_writes_is_what_the_server_reads_back() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    let name = "escrito-por-eclipse.txt";
    let _ = session.remove(&root, name);
    let made = session
        .create(
            &root,
            name,
            CreateHow::Unchecked(SetAttr {
                mode: Some(0o644),
                ..SetAttr::default()
            }),
        )
        .expect("CREATE");
    let fh = match made.fh {
        Some(fh) => fh,
        // Legal, and the reason `CreateReply::fh` is an `Option`.
        None => session.lookup(&root, name).expect("the lookup").fh,
    };
    // Longer than one WRITE, so the loop in `write_all` really turns.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    session
        .write_all(&fh, 0, &data, StableHow::FileSync)
        .expect("WRITE");
    let attr = session.getattr(&fh).expect("GETATTR");
    assert_eq!(attr.size, data.len() as u64);
    let mut back = vec![0u8; data.len()];
    let n = session.read_into(&fh, 0, &mut back).expect("the read");
    assert_eq!(n, data.len());
    assert_eq!(back, data, "the bytes came back changed");
    session.remove(&root, name).expect("REMOVE");
    assert_eq!(
        session.lookup(&root, name).err(),
        Some(LxError::ENOENT),
        "the file is gone from the server"
    );
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn a_directory_this_client_makes_is_a_directory_on_the_server() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    let name = "dir-de-eclipse";
    let _ = session.rmdir(&root, name);
    session
        .mkdir(
            &root,
            name,
            &SetAttr {
                mode: Some(0o755),
                ..SetAttr::default()
            },
        )
        .expect("MKDIR");
    let found = session.lookup(&root, name).expect("the lookup");
    assert_eq!(found.attr.map(|a| a.ftype), Some(Ftype3::Dir));
    // `.` and `..` and nothing else: a directory the server just made.
    let entries = session.readdir_all(&found.fh, false).expect("the listing");
    assert_eq!(entries.len(), 2, "{:?}", entries);
    session.rmdir(&root, name).expect("RMDIR");
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn a_name_the_server_does_not_have_is_enoent_and_not_eio() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    assert_eq!(
        session.lookup(&root, "no-existe-esto").err(),
        Some(LxError::ENOENT),
        "a real NFS3ERR_NOENT has to come out as ENOENT and not as the \
         catch-all"
    );
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn the_numbers_statfs_wants_come_from_the_server() {
    let (mounted, _) = mounted!();
    let mut session = mounted.fs.session().lock();
    let root = session.root().clone();
    let st = session.fsstat(&root).expect("FSSTAT");
    assert!(st.tbytes > 0, "{:?}", st);
    assert!(st.fbytes <= st.tbytes);
}

#[test]
#[ignore = "needs an NFSv3 server on 127.0.0.1; see the module docs"]
fn an_export_the_server_does_not_have_is_refused_at_mount_time() {
    let mut c = StdConnector::new("127.0.0.1");
    let source = NfsSource::parse("127.0.0.1:/srv/no-exportado").expect("the source");
    let err = connect_export(&mut c, &source, &test_options())
        .err()
        .expect("a mount of an export the server does not have");
    assert!(
        matches!(err, LxError::ENOENT | LxError::EACCES | LxError::EPERM),
        "a refused export came out as {:?}",
        err
    );
    assert!(
        !c.dialled.contains(&2049),
        "nfsd was dialled for an export mountd refused: {:?}",
        c.dialled
    );
}

/// The `String` import is used by the connector; this silences nothing and
/// is here so the unused-import deny does not fire on a build where the
/// tests above are all `#[ignore]`d but still compiled.
#[allow(dead_code)]
fn _uses(_: String) {}
