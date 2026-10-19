//! A syscall made in a host test the way the loader makes it: from inside a
//! started thread's own future, with that thread as `Syscall::thread`.
//!
//! `Syscall` borrows a `CurrentThread`, and the only way to get one is
//! `Thread::start_with`, which hands it to a `ThreadFn`. That is a plain
//! `fn`, so the call to make reaches it through a table keyed by the
//! thread's id.

extern crate std;

use super::*;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use linux_object::thread::ThreadExt;
use std::sync::{mpsc, Mutex};
use zircon_object::task::Thread;

type Call = Box<
    dyn for<'a> FnOnce(&'a Syscall<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> + Send,
>;

static CALLS: Mutex<Vec<(KoID, Call)>> = Mutex::new(Vec::new());

fn run_call(current: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        // A thread the call itself started -- a `fork`'s child, a `clone`'s
        // thread -- has nothing to run here, and ends as if it had exited.
        let call = {
            let mut calls = CALLS.lock().unwrap();
            let Some(at) = calls.iter().position(|(id, _)| *id == current.id()) else {
                return;
            };
            calls.swap_remove(at).1
        };
        let syscall = Syscall {
            thread: &current,
            thread_fn: run_call,
            syscall_entry: 0,
        };
        call(&syscall).await;
    })
}

/// Make `call` from a new thread of `proc` and return what it returns.
///
/// The thread exits when the call returns, and with it the process if it
/// was its only one: a test that needs the process afterwards makes every
/// call it needs inside the one `call`.
pub(crate) fn on_a_thread<R: Send + 'static>(
    proc: &Arc<Process>,
    call: impl for<'a> FnOnce(&'a Syscall<'a>) -> Pin<Box<dyn Future<Output = R> + Send + 'a>>
        + Send
        + 'static,
) -> R {
    let thread = Thread::create_linux(proc).unwrap();
    let (tx, rx) = mpsc::channel();
    let call: Call = Box::new(move |syscall| {
        Box::pin(async move {
            let _ = tx.send(call(syscall).await);
        })
    });
    CALLS.lock().unwrap().push((thread.id(), call));
    thread.start_with(|_| {}, run_call).unwrap();
    rx.recv_timeout(Duration::from_secs(20))
        .expect("the call never came back")
}
