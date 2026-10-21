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
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use linux_object::thread::ThreadExt;
use std::collections::HashMap;
use std::sync::{mpsc, Mutex, Once};
use std::thread::ThreadId;
use zircon_object::task::Thread;

type Call = Box<
    dyn for<'a> FnOnce(&'a Syscall<'a>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> + Send,
>;

static CALLS: Mutex<Vec<(KoID, Call)>> = Mutex::new(Vec::new());

/// What the last panic on each host thread said.
///
/// A panic inside the call used to reach the test as `the call never came
/// back: Disconnected`, which names neither the failing assertion nor its
/// file: `async_std::task::spawn` drops the `JoinHandle`, so the executor
/// catches the unwind and throws the payload away, and libtest's output
/// capture is inherited by the executor's worker threads from whichever test
/// first spawned one -- so the printed message lands in that test's captured
/// output and is discarded when it passes. The hook below keeps the message
/// where the unwinding task can pick it up and send it on.
static PANICS: Mutex<Option<HashMap<ThreadId, String>>> = Mutex::new(None);

/// Keep what every panic says, on top of whatever the harness already does
/// with it.
fn remember_panics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut panics) = PANICS.lock() {
                panics
                    .get_or_insert_with(HashMap::new)
                    .insert(std::thread::current().id(), info.to_string());
            }
            previous(info);
        }));
    });
}

/// What the panic that is unwinding this thread said, taken out of the map.
fn panic_of(id: ThreadId) -> Option<String> {
    PANICS.lock().ok()?.as_mut()?.remove(&id)
}

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
    remember_panics();
    let thread = Thread::create_linux(proc).unwrap();
    let (tx, rx) = mpsc::channel();
    let call: Call = Box::new(move |syscall| {
        Box::pin(async move {
            // A panic inside the call unwinds this future and drops the
            // sender with it, leaving the receiver a closed channel and
            // nothing else. Send what the panic said on the way out.
            struct Unwound<R> {
                tx: mpsc::Sender<Result<R, String>>,
                answered: bool,
            }
            impl<R> Drop for Unwound<R> {
                fn drop(&mut self) {
                    if !self.answered {
                        let said = panic_of(std::thread::current().id())
                            .unwrap_or_else(|| String::from("the task was dropped"));
                        let _ = self.tx.send(Err(said));
                    }
                }
            }
            let mut unwound = Unwound {
                tx: tx.clone(),
                answered: false,
            };
            let got = call(syscall).await;
            unwound.answered = true;
            let _ = tx.send(Ok(got));
        })
    });
    CALLS.lock().unwrap().push((thread.id(), call));
    thread.start_with(|_| {}, run_call).unwrap();
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(got)) => got,
        // Where the panic the executor swallowed reaches the test.
        Ok(Err(said)) => panic!("the call panicked: {}", said),
        Err(e) => panic!("the call never came back: {:?}", e),
    }
}

/// The machine-wide `utsname` names, held for the duration of one test.
///
/// `sethostname` and `setdomainname` write a global that every test in this
/// binary shares, and `cargo test` runs tests in parallel: two tests that
/// each set a name and then read it back can read each other's. Every test
/// that writes either name takes this first, and puts the names back before
/// it lets go.
///
/// Poisoning is ignored on purpose. A test that panicked holding this has
/// already failed and said why; the tests that follow should go on to say
/// what THEY found, so each of them sets the name it is about rather than
/// trusting the one it inherited.
pub(crate) fn the_machine_names() -> std::sync::MutexGuard<'static, ()> {
    static NAMES: Mutex<()> = Mutex::new(());
    NAMES.lock().unwrap_or_else(|held| held.into_inner())
}
