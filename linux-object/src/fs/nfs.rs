//! The NFS client: mounting a remote export over NFSv3.
//!
//! This is the first batch of it, and it is the whole protocol stack below
//! the first byte of socket I/O:
//!
//! - [`xdr`]: the XDR encoding (RFC 4506) every ONC RPC protocol uses.
//! - [`rpc`]: the RPC v2 call and reply envelope (RFC 5531) and the TCP
//!   record marking that frames it (RFC 1831 section 10).
//! - [`portmap`]: `GETPORT` against port 111, which is how a client finds
//!   `mountd`.
//! - [`mount3`]: the MOUNT v3 `MNT` call, which turns an export path into the
//!   root filehandle every NFS call starts from.
//! - [`nfs3`]: the NFSv3 types (RFC 1813) and the procedures of the read
//!   path.
//! - [`nfs3_write`]: the NFSv3 procedures that change something on the
//!   server.
//! - [`nfs3_dir`]: `READDIR` and `READDIRPLUS`, which read a directory.
//! - [`client`]: one call out, one reply back, with transaction ids,
//!   timeouts and retries, over a four-method transport trait.
//! - [`source`]: the `host:/export` string `mount(8)` passes as the device.
//!
//! What is deliberately *not* here yet is the socket and the VFS types. The split is not arbitrary: everything in this batch is
//! a pure function from bytes to bytes, so all of it is tested on the host,
//! by tests that build a reply byte by byte and assert what the decoder makes
//! of it. Nothing below needs a running kernel, a network or a server, and
//! the layer above — which needs all three — is better written against a
//! codec that is already known to be right.
//!
//! # Only a client, and only NFSv3
//!
//! No server: Eclipse does not export its own directories, by decision. The
//! protocol is symmetric enough that most of this module would serve one,
//! which is why the codec is not shaped around the client's needs only, but
//! nothing here listens.
//!
//! NFSv3 and not v4. They are not versions of one protocol so much as two
//! protocols: v4 folds the MOUNT protocol in, adds compound operations,
//! state, leases, delegations and a mandatory security framework, and its
//! client is several times the size of a v3 one. v3 is what gets a remote
//! export mounted, it is still what most NAS boxes serve by default, and it
//! is stateless — which matters more here than anywhere, because a stateless
//! protocol has nothing to recover after the kind of reboot this kernel is
//! still having.
//!
//! # The server is not trusted
//!
//! Every byte parsed here came from another machine, and the parsing happens
//! in kernel context before anything has checked who sent it. Two rules run
//! through the whole module, and both are enforced in the decoder rather than
//! asked of each caller: no length from the wire is ever used to allocate,
//! and no arithmetic on one may wrap. [`xdr`] documents the reasoning and the
//! tests hold it at the boundary.

pub mod client;
pub mod mount3;
pub mod nfs3;
pub mod nfs3_dir;
pub mod nfs3_write;
pub mod portmap;
pub mod rpc;
pub mod source;
pub mod transport;
pub mod xdr;
