//! Reference benchmarks for the Zircon IPC objects: what a channel message,
//! a call, a port packet and a handle-table entry cost in *software*, with
//! no syscall entry, no scheduler and no guest.
//!
//! The microkernel plan moves the Linux personality, the drivers and the file
//! systems into userspace servers, and every request to one of them becomes
//! a channel call. `zx-ipc-bench` (tools/) measures that call end to end on
//! the real kernel, under QEMU, against a pipe; these rows say how much of it
//! is this crate. If a row moves, the end-to-end number moves with it.
//!
//! The shape is a ladder, read as DIFFERENCES between rows rather than as
//! absolutes:
//!
//! 1. `channel_write_read_32b` -- one message through the queue and out:
//!    the floor, and the control row for the rest.
//! 2. `channel_write_read_4kib` -- the same with a page of payload: what the
//!    copy adds over the queue.
//! 3. `channel_write_read_one_handle` -- a 32-byte message carrying an
//!    event: what moving a handle adds, which is how a server hands out its
//!    channels.
//! 4. `channel_call_reply_32b` -- a whole call: the request out, the reply
//!    routed back to the caller by its txid through the reply table. Twice
//!    row 1 plus what the routing costs.
//! 5. `channel_wait_park_and_wake` -- what a server that waits for its next
//!    request pays: a `wait_signal` future that parks, the write that fires
//!    it, the poll that returns, the read.
//! 6. `port_queue_and_wait` -- one user packet through a port, the other
//!    way a server learns that something happened.
//! 7. `handle_add_remove` / `handle_lookup` -- the handle table every
//!    syscall above goes through to find its object.
//!
//! What the host cannot show: the kernel heap. Each message allocates (its
//! `Vec`s, the reply slot), and here that is the host's allocator, not the
//! kernel's buddy with its per-CPU front cache. A change that adds an
//! allocation per message shows up here as a few nanoseconds and on the
//! machine as much more.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the
//! reasons given in `benches/vm.rs`, and every row passes its INPUTS through
//! `test::black_box` as well as its result.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p zircon-object --bench ipc --features libos,aspace-separate
//! ```

#![feature(test)]
extern crate test;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use test::Bencher;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::{Handle, KernelObject, Rights, Signal};
use zircon_object::signal::{Event, PayloadRepr, Port, PortPacketRepr};
use zircon_object::task::{Job, Process};

fn message(len: usize) -> MessagePacket {
    MessagePacket {
        data: vec![0u8; len],
        handles: Vec::new(),
    }
}

/// The reply to `request`: its txid (the first four bytes) and a payload.
fn reply_to(request: &MessagePacket) -> MessagePacket {
    let mut reply = message(32);
    reply.set_txid(request.get_txid());
    reply
}

fn write_read(b: &mut Bencher, len: usize) {
    let (a, z) = Channel::create();
    b.iter(|| {
        test::black_box(&a)
            .write(message(test::black_box(len)))
            .unwrap();
        test::black_box(z.read().unwrap())
    });
}

#[bench]
fn channel_write_read_32b(b: &mut Bencher) {
    write_read(b, 32);
}

#[bench]
fn channel_write_read_4kib(b: &mut Bencher) {
    write_read(b, 4096);
}

#[bench]
fn channel_write_read_one_handle(b: &mut Bencher) {
    let (a, z) = Channel::create();
    let event: Arc<dyn KernelObject> = Event::new();
    b.iter(|| {
        let mut msg = message(32);
        msg.handles.push(Handle::new(
            test::black_box(&event).clone(),
            Rights::DEFAULT_EVENT,
        ));
        a.write(msg).unwrap();
        test::black_box(z.read().unwrap())
    });
}

#[bench]
fn channel_call_reply_32b(b: &mut Bencher) {
    let (client, server) = Channel::create();
    let mut cx = Context::from_waker(Waker::noop());
    b.iter(|| {
        let mut call = pin!(test::black_box(&client).call(message(32)));
        assert!(call.as_mut().poll(&mut cx).is_pending());
        let request = server.read().unwrap();
        server.write(reply_to(&request)).unwrap();
        match call.as_mut().poll(&mut cx) {
            Poll::Ready(reply) => test::black_box(reply.unwrap()),
            Poll::Pending => panic!("the reply was not routed to its call"),
        }
    });
}

#[bench]
fn channel_wait_park_and_wake(b: &mut Bencher) {
    let (client, server) = Channel::create();
    let object: Arc<dyn KernelObject> = server.clone();
    let mut cx = Context::from_waker(Waker::noop());
    b.iter(|| {
        let mut wait = pin!(test::black_box(&object).wait_signal(Signal::READABLE));
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        client.write(message(32)).unwrap();
        match wait.as_mut().poll(&mut cx) {
            Poll::Ready(signal) => assert!(signal.contains(Signal::READABLE)),
            Poll::Pending => panic!("the write did not wake the wait"),
        }
        test::black_box(server.read().unwrap())
    });
}

#[bench]
fn port_queue_and_wait(b: &mut Bencher) {
    let port = Port::new(0).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    b.iter(|| {
        test::black_box(&port)
            .push_user(PortPacketRepr {
                key: 1,
                status: 0,
                data: PayloadRepr::User([0; 32]),
            })
            .unwrap();
        match pin!(port.wait()).poll(&mut cx) {
            Poll::Ready(packet) => test::black_box(packet),
            Poll::Pending => panic!("a queued packet was not there to wait for"),
        }
    });
}

#[bench]
fn handle_add_remove(b: &mut Bencher) {
    let proc = Process::create(&Job::root(), "bench").unwrap();
    let event: Arc<dyn KernelObject> = Event::new();
    b.iter(|| {
        let value = test::black_box(&proc).add_handle(Handle::new(
            test::black_box(&event).clone(),
            Rights::DEFAULT_EVENT,
        ));
        test::black_box(proc.remove_handle(value).unwrap())
    });
}

#[bench]
fn handle_lookup(b: &mut Bencher) {
    let proc = Process::create(&Job::root(), "bench").unwrap();
    let value = proc.add_handle(Handle::new(Event::new(), Rights::DEFAULT_EVENT));
    b.iter(|| {
        test::black_box(&proc)
            .get_dyn_object_with_rights(test::black_box(value), Rights::WAIT)
            .unwrap()
    });
}
