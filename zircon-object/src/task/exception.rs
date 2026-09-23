use alloc::{sync::Arc, vec::Vec};
use core::mem::size_of;

use futures::channel::oneshot;
use kernel_hal::context::{TrapReason, UserContext};
use kernel_hal::sync::Mutex;

use super::{Job, Task, Thread};
use crate::ipc::{Channel, MessagePacket};
use crate::object::{Handle, KObjectBase, KernelObject, KoID, Rights, Signal};
use crate::{impl_kobject, ZxError, ZxResult};

/// Kernel-owned exception channel endpoint.
pub struct Exceptionate {
    type_: ExceptionChannelType,
    inner: Mutex<ExceptionateInner>,
}

enum ExceptionateInner {
    Init,
    Bind {
        channel: Arc<Channel>,
        rights: Rights,
    },
    Shutdown,
}

impl Exceptionate {
    /// Create an `Exceptionate`.
    pub(super) fn new(type_: ExceptionChannelType) -> Arc<Self> {
        Arc::new(Exceptionate {
            type_,
            inner: Mutex::new(ExceptionateInner::Init),
        })
    }

    /// Shutdown the exceptionate.
    pub(super) fn shutdown(&self) {
        *self.inner.lock() = ExceptionateInner::Shutdown;
    }

    /// Create an exception channel endpoint for user.
    pub fn create_channel(&self, rights: Rights) -> ZxResult<Arc<Channel>> {
        let mut inner = self.inner.lock();
        match &*inner {
            ExceptionateInner::Shutdown => return Err(ZxError::BAD_STATE),
            ExceptionateInner::Bind { channel, .. } if channel.peer().is_ok() => {
                // already has a valid channel
                return Err(ZxError::ALREADY_BOUND);
            }
            _ => {}
        }
        let (channel, user_channel) = Channel::create();
        *inner = ExceptionateInner::Bind { channel, rights };
        Ok(user_channel)
    }

    /// Whether the user-owned channel endpoint is alive.
    pub(super) fn has_channel(&self) -> bool {
        let inner = self.inner.lock();
        matches!(&*inner, ExceptionateInner::Bind { channel, .. } if channel.peer().is_ok())
    }

    /// Send exception to the user-owned endpoint.
    pub(super) fn send_exception(
        &self,
        exception: &Arc<Exception>,
    ) -> ZxResult<oneshot::Receiver<()>> {
        debug!(
            "Exception: {:?} ,try send to {:?}",
            exception.type_, self.type_
        );
        let mut inner = self.inner.lock();
        let (channel, rights) = match &*inner {
            ExceptionateInner::Bind { channel, rights } => (channel, *rights),
            _ => return Err(ZxError::NEXT),
        };
        let info = ExceptionInfo {
            pid: exception.thread.proc().id(),
            tid: exception.thread.id(),
            type_: exception.type_,
            padding: Default::default(),
        };
        let (object, closed) = ExceptionObject::create(exception.clone(), rights);
        let msg = MessagePacket {
            data: info.pack(),
            handles: alloc::vec![Handle::new(object, Rights::DEFAULT_EXCEPTION)],
        };
        channel.write(msg).map_err(|err| {
            if err == ZxError::PEER_CLOSED {
                *inner = ExceptionateInner::Init;
                return ZxError::NEXT;
            }
            err
        })?;
        Ok(closed)
    }
}

#[repr(C)]
#[derive(Debug)]
struct ExceptionInfo {
    pid: KoID,
    tid: KoID,
    type_: ExceptionType,
    padding: u32,
}

impl ExceptionInfo {
    #[allow(unsafe_code)]
    fn pack(&self) -> Vec<u8> {
        let buf: [u8; size_of::<ExceptionInfo>()] = unsafe { core::mem::transmute_copy(self) };
        Vec::from(buf)
    }
}

/// The common header of all exception reports.
#[repr(C)]
#[derive(Debug, Clone)]
struct ExceptionHeader {
    /// The actual size, in bytes, of the report (including this field).
    size: u32,
    /// The type of the exception.
    type_: ExceptionType,
}

/// Data associated with an exception (siginfo in linux parlance)
/// Things available from regsets (e.g., pc) are not included here.
/// For an example list of things one might add, see linux siginfo.
#[repr(C)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ExceptionContext {
    arch: ExceptionContextInner,
    synth_code: u32,
    synth_data: u32,
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        #[repr(C)]
        #[derive(Debug, Default, Clone, PartialEq, Eq)]
        struct ExceptionContextInner {
            vector: u64,
            err_code: u64,
            cr2: u64,
        }
    } else if #[cfg(target_arch = "aarch64")] {
        #[repr(C)]
        #[derive(Debug, Default, Clone, PartialEq, Eq)]
        struct ExceptionContextInner {
            esr: u32,
            _padding1: u32,
            far: u64,
            _padding2: u64,
        }
    } else if #[cfg(target_arch = "riscv64")] {
        #[repr(C)]
        #[derive(Debug, Default, Clone, PartialEq, Eq)]
        struct ExceptionContextInner {
            scause: u64,
            stval: u64,
            _padding: u64,
        }
    }
}

impl ExceptionContext {
    fn from_user_context(ctx: &UserContext) -> Self {
        let fault_vaddr = if let TrapReason::PageFault(vaddr, _) = ctx.trap_reason() {
            vaddr as u64
        } else {
            return Default::default();
        };
        #[cfg(target_arch = "x86_64")]
        let arch = ExceptionContextInner {
            vector: ctx.raw_trap_reason() as _,
            err_code: ctx.error_code() as _,
            cr2: fault_vaddr,
        };
        #[cfg(target_arch = "aarch64")]
        let arch = ExceptionContextInner {
            esr: ctx.raw_trap_reason() as _,
            far: fault_vaddr,
            ..Default::default()
        };
        #[cfg(target_arch = "riscv64")]
        let arch = ExceptionContextInner {
            scause: ctx.raw_trap_reason() as _,
            stval: fault_vaddr,
            ..Default::default()
        };
        Self {
            arch,
            synth_code: ZxError::ACCESS_DENIED as i32 as u32,
            ..Default::default()
        }
    }
}

/// Version 1 exception report, without synthetic exception data.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ExceptionReportV1 {
    header: ExceptionHeader,
    context: ExceptionContextInner,
}

/// Data reported to an exception handler for most exceptions.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct ExceptionReport {
    /// The common header of all exception reports.
    header: ExceptionHeader,
    /// Exception-specific data.
    context: ExceptionContext,
}

impl ExceptionReport {
    fn new(type_: ExceptionType, cx: Option<&UserContext>) -> Self {
        ExceptionReport {
            header: ExceptionHeader {
                type_,
                size: core::mem::size_of::<ExceptionReport>() as u32,
            },
            context: cx
                .map(ExceptionContext::from_user_context)
                .unwrap_or_default(),
        }
    }

    pub(super) fn as_v1(&self) -> ExceptionReportV1 {
        ExceptionReportV1 {
            header: ExceptionHeader {
                type_: self.header.type_,
                size: core::mem::size_of::<ExceptionReportV1>() as u32,
            },
            context: self.context.arch.clone(),
        }
    }
}

/// Type of exception
#[allow(missing_docs)]
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExceptionType {
    General = 0x008,
    FatalPageFault = 0x108,
    UndefinedInstruction = 0x208,
    SoftwareBreakpoint = 0x308,
    HardwareBreakpoint = 0x408,
    UnalignedAccess = 0x508,
    // exceptions generated by kernel instead of the hardware
    Synth = 0x8000,
    ThreadStarting = 0x8008,
    ThreadExiting = 0x8108,
    PolicyError = 0x8208,
    ProcessStarting = 0x8308,
}

impl ExceptionType {
    /// Is the exception type generated by kernel instead of the hardware.
    pub fn is_synth(self) -> bool {
        (self as u32) & (ExceptionType::Synth as u32) != 0
    }
}

/// Type of the exception channel
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum ExceptionChannelType {
    None = 0,
    Debugger = 1,
    Thread = 2,
    Process = 3,
    Job = 4,
    JobDebugger = 5,
}

/// The exception object received from the exception channel.
///
/// This will be transmitted to registered exception handlers in userspace
/// and provides them with exception state and control functionality.
/// We do not send exception directly since it's hard to figure out
/// when will the handle close.
pub struct ExceptionObject {
    base: KObjectBase,
    exception: Arc<Exception>,
    /// Task rights copied from `Exceptionate`.
    rights: Rights,
    close_signal: Option<oneshot::Sender<()>>,
}

impl_kobject!(ExceptionObject);

impl ExceptionObject {
    /// Create an kernel object of `Exception`.
    ///
    /// Return the object and a `Receiver` of the object dropped event.
    fn create(exception: Arc<Exception>, rights: Rights) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel();
        let object = Arc::new(ExceptionObject {
            base: KObjectBase::new(),
            exception,
            rights,
            close_signal: Some(sender),
        });
        (object, receiver)
    }

    /// Create a handle for the exception's thread.
    pub fn get_thread_handle(&self) -> Handle {
        Handle {
            object: self.exception.thread.clone(),
            rights: self.rights & Rights::DEFAULT_THREAD,
        }
    }

    /// Create a handle for the exception's process.
    pub fn get_process_handle(&self) -> ZxResult<Handle> {
        if self.exception.current_channel_type() == ExceptionChannelType::Thread {
            return Err(ZxError::ACCESS_DENIED);
        }
        Ok(Handle {
            object: self.exception.thread.proc().clone(),
            rights: self.rights & Rights::DEFAULT_PROCESS,
        })
    }

    /// Get whether closing the exception handle will
    /// finish exception processing and resume the underlying thread.
    pub fn state(&self) -> u32 {
        self.exception.inner.lock().handled as u32
    }

    /// Set whether closing the exception handle will
    /// finish exception processing and resume the underlying thread.
    pub fn set_state(&self, state: u32) -> ZxResult {
        if state > 1 {
            return Err(ZxError::INVALID_ARGS);
        }
        self.exception.inner.lock().handled = state == 1;
        Ok(())
    }

    /// Get whether the debugger gets a 'second chance' at handling the exception
    /// if the process-level handler fails to do so.
    pub fn strategy(&self) -> u32 {
        self.exception.inner.lock().second_chance as u32
    }

    /// Set whether the debugger gets a 'second chance' at handling the exception
    /// if the process-level handler fails to do so.
    pub fn set_strategy(&self, strategy: u32) -> ZxResult {
        if strategy > 1 {
            return Err(ZxError::INVALID_ARGS);
        }
        let mut inner = self.exception.inner.lock();
        match inner.current_channel_type {
            ExceptionChannelType::Debugger | ExceptionChannelType::JobDebugger => {
                inner.second_chance = strategy == 1;
                Ok(())
            }
            _ => Err(ZxError::BAD_STATE),
        }
    }
}

impl Drop for ExceptionObject {
    fn drop(&mut self) {
        self.close_signal.take().unwrap().send(()).ok();
    }
}

/// An Exception represents a single currently-active exception.
pub(super) struct Exception {
    thread: Arc<Thread>,
    type_: ExceptionType,
    report: ExceptionReport,
    inner: Mutex<ExceptionInner>,
}

struct ExceptionInner {
    current_channel_type: ExceptionChannelType,
    handled: bool,
    second_chance: bool,
}

impl Exception {
    /// Create an `Exception`.
    pub fn new(thread: &Arc<Thread>, type_: ExceptionType, cx: Option<&UserContext>) -> Arc<Self> {
        Arc::new(Exception {
            thread: thread.clone(),
            type_,
            report: ExceptionReport::new(type_, cx),
            inner: Mutex::new(ExceptionInner {
                current_channel_type: ExceptionChannelType::None,
                handled: false,
                second_chance: false,
            }),
        })
    }

    /// Handle the exception.
    ///
    /// Note that it's possible that this may returns before exception was send to any exception channel.
    /// This happens only when the thread is killed before we send the exception.
    pub async fn handle(self: &Arc<Self>) {
        let result = match self.type_ {
            ExceptionType::ProcessStarting => {
                self.handle_with(JobDebuggerIterator::new(self.thread.proc().job()), true)
                    .await
            }
            ExceptionType::ThreadStarting | ExceptionType::ThreadExiting => {
                self.handle_with(Some(self.thread.proc().debug_exceptionate()), false)
                    .await
            }
            _ => {
                self.handle_with(ExceptionateIterator::new(self), false)
                    .await
            }
        };
        if result == Err(ZxError::NEXT) && !self.type_.is_synth() {
            // Nobody handled the exception, kill myself
            self.thread.proc().exit(super::TASK_RETCODE_SYSCALL_KILL);
        }
    }

    /// Handle the exception with a customized iterator.
    ///
    /// If `first_only` is true, this will only send exception to the first one that received the exception
    /// even when the exception is not handled.
    async fn handle_with(
        self: &Arc<Self>,
        exceptionates: impl IntoIterator<Item = Arc<Exceptionate>>,
        first_only: bool,
    ) -> ZxResult {
        for exceptionate in exceptionates.into_iter() {
            let closed = match exceptionate.send_exception(self) {
                // This channel is not available now!
                Err(ZxError::NEXT) => continue,
                res => res?,
            };
            self.inner.lock().current_channel_type = exceptionate.type_;
            // If this error, the sender is dropped, and the handle should also be closed.
            closed.await.ok();
            let handled = {
                let mut inner = self.inner.lock();
                inner.current_channel_type = ExceptionChannelType::None;
                inner.handled
            };
            if handled | first_only {
                return Ok(());
            }
        }
        Err(ZxError::NEXT)
    }

    /// Get the exception's channel type.
    pub fn current_channel_type(&self) -> ExceptionChannelType {
        self.inner.lock().current_channel_type
    }

    /// Get a report of the exception.
    pub fn report(&self) -> ExceptionReport {
        self.report.clone()
    }
}

/// An iterator used to find Exceptionates used while handling the exception
/// This is only used to handle normal exceptions (Architectural & Policy)
/// We can use rust generator instead here but that is somehow not stable
/// Exception handlers are tried in the following order:
/// - process debugger
/// - thread
/// - process
/// - process debugger (in dealing with a second-chance exception)
/// - job (first owning job, then its parent job, and so on up to root job)
struct ExceptionateIterator<'a> {
    exception: &'a Exception,
    state: ExceptionateIteratorState,
}

/// The state used in ExceptionateIterator.
/// Name of options is what to consider next
enum ExceptionateIteratorState {
    Debug(bool),
    Thread,
    Process,
    Job(Arc<Job>),
    Finished,
}

impl<'a> ExceptionateIterator<'a> {
    fn new(exception: &'a Exception) -> Self {
        ExceptionateIterator {
            exception,
            state: ExceptionateIteratorState::Debug(false),
        }
    }
}

impl<'a> Iterator for ExceptionateIterator<'a> {
    type Item = Arc<Exceptionate>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &self.state {
                ExceptionateIteratorState::Debug(second_chance) => {
                    if *second_chance && !self.exception.inner.lock().second_chance {
                        self.state =
                            ExceptionateIteratorState::Job(self.exception.thread.proc().job());
                        continue;
                    }
                    let proc = self.exception.thread.proc();
                    self.state = if *second_chance {
                        ExceptionateIteratorState::Job(self.exception.thread.proc().job())
                    } else {
                        ExceptionateIteratorState::Thread
                    };
                    return Some(proc.debug_exceptionate());
                }
                ExceptionateIteratorState::Thread => {
                    self.state = ExceptionateIteratorState::Process;
                    return Some(self.exception.thread.exceptionate());
                }
                ExceptionateIteratorState::Process => {
                    let proc = self.exception.thread.proc();
                    self.state = ExceptionateIteratorState::Debug(true);
                    return Some(proc.exceptionate());
                }
                ExceptionateIteratorState::Job(job) => {
                    let parent = job.parent();
                    let result = job.exceptionate();
                    self.state = parent.map_or(
                        ExceptionateIteratorState::Finished,
                        ExceptionateIteratorState::Job,
                    );
                    return Some(result);
                }
                ExceptionateIteratorState::Finished => return None,
            }
        }
    }
}

/// This is only used by ProcessStarting exceptions
struct JobDebuggerIterator {
    job: Option<Arc<Job>>,
}

impl JobDebuggerIterator {
    /// Create a new JobDebuggerIterator
    fn new(job: Arc<Job>) -> Self {
        JobDebuggerIterator { job: Some(job) }
    }
}

impl Iterator for JobDebuggerIterator {
    type Item = Arc<Exceptionate>;
    fn next(&mut self) -> Option<Self::Item> {
        let result = self.job.as_ref().map(|job| job.debug_exceptionate());
        self.job = self.job.as_ref().and_then(|job| job.parent());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::*;
    use core::convert::TryInto;

    #[test]
    fn exceptionate_iterator() {
        let parent_job = Job::root();
        let job = parent_job.create_child().unwrap();
        let proc = Process::create(&job, "proc").unwrap();
        let thread = Thread::create(&proc, "thread").unwrap();

        let exception = Exception::new(&thread, ExceptionType::Synth, None);
        let actual: Vec<_> = ExceptionateIterator::new(&exception).collect();
        let expected = [
            proc.debug_exceptionate(),
            thread.exceptionate(),
            proc.exceptionate(),
            job.exceptionate(),
            parent_job.exceptionate(),
        ];
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            assert!(Arc::ptr_eq(&actual, expected));
        }
    }

    #[test]
    fn exceptionate_iterator_second_chance() {
        let parent_job = Job::root();
        let job = parent_job.create_child().unwrap();
        let proc = Process::create(&job, "proc").unwrap();
        let thread = Thread::create(&proc, "thread").unwrap();

        let exception = Exception::new(&thread, ExceptionType::Synth, None);
        exception.inner.lock().second_chance = true;
        let actual: Vec<_> = ExceptionateIterator::new(&exception).collect();
        let expected = [
            proc.debug_exceptionate(),
            thread.exceptionate(),
            proc.exceptionate(),
            proc.debug_exceptionate(),
            job.exceptionate(),
            parent_job.exceptionate(),
        ];
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            assert!(Arc::ptr_eq(&actual, expected));
        }
    }

    #[test]
    fn job_debugger_iterator() {
        let parent_job = Job::root();
        let job = parent_job.create_child().unwrap();
        let child_job = job.create_child().unwrap();
        let _grandson_job = child_job.create_child().unwrap();

        let actual: Vec<_> = JobDebuggerIterator::new(child_job.clone()).collect();
        let expected = [
            child_job.debug_exceptionate(),
            job.debug_exceptionate(),
            parent_job.debug_exceptionate(),
        ];
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            assert!(Arc::ptr_eq(&actual, expected));
        }
    }

    #[async_std::test]
    async fn exception_handling() {
        let parent_job = Job::root();
        let job = parent_job.create_child().unwrap();
        let proc = Process::create(&job, "proc").unwrap();
        let thread = Thread::create(&proc, "thread").unwrap();

        let exception = Exception::new(&thread, ExceptionType::Synth, None);

        // This is used to verify that exceptions are handled in a specific order
        let handled_order = Arc::new(Mutex::new(Vec::<usize>::new()));

        let create_handler = |exceptionate: &Arc<Exceptionate>,
                              should_receive: bool,
                              should_handle: bool,
                              order: usize| {
            let channel = exceptionate
                .create_channel(Rights::DEFAULT_THREAD | Rights::DEFAULT_PROCESS)
                .unwrap();
            let handled_order = handled_order.clone();

            async_std::task::spawn(async move {
                // wait for the channel is ready
                let channel_object: Arc<dyn KernelObject> = channel.clone();
                channel_object
                    .wait_signal(Signal::READABLE | Signal::PEER_CLOSED)
                    .await;

                if !should_receive {
                    // channel should be closed without message
                    assert_eq!(channel.read().err(), Some(ZxError::PEER_CLOSED));
                    return;
                }

                // we should get the exception here
                let data = channel.read().unwrap();
                assert_eq!(data.handles.len(), 1);
                let exception = data.handles[0]
                    .object
                    .clone()
                    .downcast_arc::<ExceptionObject>()
                    .unwrap();
                if should_handle {
                    exception.set_state(1).unwrap();
                }
                // record the order of the handler used
                handled_order.lock().push(order);
            })
        };

        // proc debug should get the exception first
        create_handler(&proc.debug_exceptionate(), true, false, 0);
        // thread should get the exception next
        create_handler(&thread.exceptionate(), true, false, 1);
        // here we omit proc to test that we can handle the case that there is none handler
        // job should get the exception and handle it next
        create_handler(&job.exceptionate(), true, true, 3);
        // since exception is handled we should not get it from parent job
        create_handler(&parent_job.exceptionate(), false, false, 4);

        exception.handle().await;

        // terminate handlers by shutdown the related exceptionates
        thread.exceptionate().shutdown();
        proc.debug_exceptionate().shutdown();
        job.exceptionate().shutdown();
        parent_job.exceptionate().shutdown();

        // test for the order: proc debug -> thread -> job
        assert_eq!(handled_order.lock().clone(), vec![0, 1, 3]);
    }

    /// A thread of its own job and process, so nothing here shares an
    /// exceptionate with anything else.
    fn a_thread() -> Arc<Thread> {
        let job = Job::root().create_child().unwrap();
        let proc = Process::create(&job, "proc").unwrap();
        Thread::create(&proc, "thread").unwrap()
    }

    fn page_fault(thread: &Arc<Thread>) -> Arc<Exception> {
        Exception::new(thread, ExceptionType::FatalPageFault, None)
    }

    #[test]
    /// `zx_task_create_exception_channel` gives out one endpoint at a time, and
    /// the seat opens again when the handler lets go of its end -- a debugger
    /// that crashed must not lock the task out of ever being debugged again.
    fn a_task_has_one_exception_channel_at_a_time() {
        let e = Exceptionate::new(ExceptionChannelType::Thread);
        let handler = e.create_channel(Rights::DEFAULT_THREAD).unwrap();
        assert!(e.has_channel());
        assert_eq!(
            e.create_channel(Rights::DEFAULT_THREAD).err(),
            Some(ZxError::ALREADY_BOUND),
        );

        drop(handler);
        assert!(!e.has_channel(), "the seat is free once the handler goes");
        let _second = e.create_channel(Rights::DEFAULT_THREAD).unwrap();
        assert!(e.has_channel());
    }

    #[test]
    /// Shutdown is how a dying task stops accepting handlers, and it is final:
    /// it is not the same state as "nobody has asked yet".
    fn a_shut_down_exceptionate_takes_no_more_handlers() {
        let e = Exceptionate::new(ExceptionChannelType::Thread);
        let _handler = e.create_channel(Rights::DEFAULT_THREAD).unwrap();
        e.shutdown();
        assert!(!e.has_channel());
        assert_eq!(
            e.create_channel(Rights::DEFAULT_THREAD).err(),
            Some(ZxError::BAD_STATE),
        );

        let thread = a_thread();
        assert_eq!(
            e.send_exception(&page_fault(&thread)).err(),
            Some(ZxError::NEXT),
            "a shut down handler is skipped, not an error the thread sees",
        );
    }

    #[test]
    /// `NEXT` is how one link in the handler chain says "not me": nobody
    /// listening, and a handler that has closed its end, both mean the same
    /// thing to `handle_with`, which is to try the next one.
    fn an_exceptionate_nobody_is_listening_to_says_next() {
        let thread = a_thread();
        let exception = page_fault(&thread);
        let e = Exceptionate::new(ExceptionChannelType::Thread);
        assert_eq!(e.send_exception(&exception).err(), Some(ZxError::NEXT));

        let handler = e.create_channel(Rights::DEFAULT_THREAD).unwrap();
        assert!(e.send_exception(&exception).is_ok());
        drop(handler);
        assert_eq!(e.send_exception(&exception).err(), Some(ZxError::NEXT));

        // And the seat is given up on the way out, so the next handler to ask
        // for it gets it rather than `ALREADY_BOUND` for a channel that is
        // never going to answer.
        assert!(!e.has_channel());
        assert!(e.create_channel(Rights::DEFAULT_THREAD).is_ok());
    }

    #[test]
    /// What the handler reads off the channel is `zx_exception_info_t`, laid
    /// out for a program that is not this kernel: two koids, the type, and the
    /// padding that makes it 24 bytes. Plus one handle, the exception object.
    fn the_packet_names_the_process_and_the_thread_that_faulted() {
        let thread = a_thread();
        let e = Exceptionate::new(ExceptionChannelType::Thread);
        let handler = e.create_channel(Rights::DEFAULT_THREAD).unwrap();
        // Kept, so the exception object in the packet stays unsignalled.
        let _closed = e.send_exception(&page_fault(&thread)).unwrap();

        let packet = handler.read().unwrap();
        assert_eq!(packet.data.len(), 24);
        let word = |at: usize| u64::from_ne_bytes(packet.data[at..at + 8].try_into().unwrap());
        let half = |at: usize| u32::from_ne_bytes(packet.data[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), thread.proc().id(), "pid");
        assert_eq!(word(8), thread.id(), "tid");
        assert_eq!(half(16), ExceptionType::FatalPageFault as u32, "type");
        assert_eq!(half(20), 0, "padding");

        assert_eq!(packet.handles.len(), 1);
        let object = packet.handles[0]
            .object
            .clone()
            .downcast_arc::<ExceptionObject>()
            .unwrap();
        assert_eq!(object.state(), 0, "not handled until the handler says so");
    }

    #[test]
    /// The report a debugger reads carries its own length, and the v1 form is
    /// the same report without the synthetic tail. Both numbers are the ABI,
    /// so they are worth pinning: a field added to `ExceptionContext` without
    /// a matching change on the other side reads as garbage.
    fn an_exception_report_is_the_size_it_says_it_is() {
        let report = ExceptionReport::new(ExceptionType::FatalPageFault, None);
        assert_eq!(report.header.size as usize, size_of::<ExceptionReport>());
        assert_eq!(report.header.type_, ExceptionType::FatalPageFault);

        let v1 = report.as_v1();
        assert_eq!(v1.header.size as usize, size_of::<ExceptionReportV1>());
        assert_eq!(v1.header.type_, report.header.type_);
        assert!(
            size_of::<ExceptionReportV1>() < size_of::<ExceptionReport>(),
            "v1 is the report without the synthetic fields",
        );
        assert_eq!(
            size_of::<ExceptionReportV1>(),
            size_of::<ExceptionHeader>() + size_of::<ExceptionContextInner>(),
        );
        assert_eq!(size_of::<ExceptionInfo>(), 24);
        assert_eq!(size_of::<ExceptionHeader>(), 8);
    }

    #[test]
    /// `zx_exception_set_state` and `zx_exception_set_strategy` are both
    /// booleans on the wire, so anything else is an error rather than a
    /// truncation to some bit of it.
    fn the_state_and_the_strategy_only_take_a_zero_or_a_one() {
        let thread = a_thread();
        let exception = page_fault(&thread);
        exception.inner.lock().current_channel_type = ExceptionChannelType::Debugger;
        let (object, _closed) = ExceptionObject::create(exception, Rights::DEFAULT_THREAD);

        assert_eq!(object.state(), 0);
        assert_eq!(object.set_state(2).err(), Some(ZxError::INVALID_ARGS));
        assert_eq!(object.state(), 0, "a refused set changes nothing");
        object.set_state(1).unwrap();
        assert_eq!(object.state(), 1);
        object.set_state(0).unwrap();
        assert_eq!(object.state(), 0);

        assert_eq!(object.strategy(), 0);
        assert_eq!(object.set_strategy(2).err(), Some(ZxError::INVALID_ARGS));
        assert_eq!(object.strategy(), 0);
        object.set_strategy(1).unwrap();
        assert_eq!(object.strategy(), 1);
    }

    #[test]
    /// Only a debugger can ask for a second chance, because the second chance
    /// *is* the debugger being tried again after the process handler passed.
    /// Everyone else gets `BAD_STATE`, and the flag stays where it was.
    fn only_a_debugger_gets_a_second_chance() {
        let thread = a_thread();
        let exception = page_fault(&thread);
        let (object, _closed) = ExceptionObject::create(exception.clone(), Rights::DEFAULT_THREAD);

        for kind in [
            ExceptionChannelType::None,
            ExceptionChannelType::Thread,
            ExceptionChannelType::Process,
            ExceptionChannelType::Job,
        ] {
            exception.inner.lock().current_channel_type = kind;
            assert_eq!(
                object.set_strategy(1).err(),
                Some(ZxError::BAD_STATE),
                "{:?} asked for a second chance",
                kind,
            );
            assert_eq!(object.strategy(), 0);
        }

        for kind in [
            ExceptionChannelType::Debugger,
            ExceptionChannelType::JobDebugger,
        ] {
            exception.inner.lock().current_channel_type = kind;
            object.set_strategy(1).unwrap();
            assert_eq!(object.strategy(), 1);
            object.set_strategy(0).unwrap();
        }
    }

    #[test]
    /// A thread-level handler is given the faulting thread and nothing above
    /// it; the process handle is the debugger's. And both handles carry the
    /// rights the exceptionate was opened with, narrowed to what the kind of
    /// object allows -- never more.
    fn a_thread_handler_does_not_get_a_handle_to_the_process() {
        let thread = a_thread();
        let exception = page_fault(&thread);
        // A job's exceptionate opens with a job's rights, which is what tells
        // the two masks below apart: `ENUMERATE` survives into a process
        // handle and not into a thread handle.
        let (object, _closed) = ExceptionObject::create(exception.clone(), Rights::DEFAULT_JOB);

        exception.inner.lock().current_channel_type = ExceptionChannelType::Thread;
        assert_eq!(
            object.get_process_handle().err(),
            Some(ZxError::ACCESS_DENIED),
        );

        exception.inner.lock().current_channel_type = ExceptionChannelType::Debugger;
        let process = object.get_process_handle().unwrap();
        assert!(Arc::ptr_eq(
            &process.object.clone().downcast_arc::<Process>().unwrap(),
            thread.proc(),
        ));
        assert_eq!(
            process.rights,
            Rights::DEFAULT_JOB & Rights::DEFAULT_PROCESS
        );
        assert!(process.rights.contains(Rights::ENUMERATE));
        assert!(!process.rights.contains(Rights::MANAGE_JOB), "and no more");

        let handle = object.get_thread_handle();
        assert!(Arc::ptr_eq(
            &handle.object.clone().downcast_arc::<Thread>().unwrap(),
            &thread,
        ));
        assert_eq!(handle.rights, Rights::DEFAULT_JOB & Rights::DEFAULT_THREAD);
        assert!(
            !handle.rights.contains(Rights::ENUMERATE),
            "a thread is not something you enumerate the children of",
        );

        // A handler opened with less than the default keeps less.
        let (thin, _closed) = ExceptionObject::create(exception, Rights::INSPECT);
        assert_eq!(thin.get_thread_handle().rights, Rights::INSPECT);
    }

    #[test]
    /// The thread that faulted is waiting on the exception object going away,
    /// so letting go of the last handle is what resumes it. That signal is the
    /// whole reason the object exists rather than the exception being sent.
    fn letting_go_of_the_exception_object_is_what_wakes_the_thread() {
        let thread = a_thread();
        let (object, mut closed) = ExceptionObject::create(page_fault(&thread), Rights::INSPECT);
        assert_eq!(closed.try_recv(), Ok(None), "still held, still waiting");

        let second = object.clone();
        drop(object);
        assert_eq!(
            closed.try_recv(),
            Ok(None),
            "one of two handles is not enough"
        );
        drop(second);
        assert_eq!(closed.try_recv(), Ok(Some(())), "the last one wakes it");
    }

    #[test]
    /// An exception that is not a fault has no fault to report, and the early
    /// return in `from_user_context` is the only thing standing between it and
    /// the architecture's fault-syndrome registers -- which, at this point in
    /// the exception's life, hold whatever the last real fault put there.
    /// (On aarch64 that read was `unimplemented!()` until this batch, so the
    /// guard was also the only thing keeping the kernel up.)
    fn an_exception_that_is_not_a_fault_reports_no_fault() {
        let quiet = UserContext::new();
        let with_context = ExceptionReport::new(ExceptionType::ThreadStarting, Some(&quiet));
        let without = ExceptionReport::new(ExceptionType::ThreadStarting, None);
        assert_eq!(with_context.context, without.context);
        assert_eq!(with_context.context, ExceptionContext::default());
    }

    #[test]
    /// The numbers are `zx_excp_type_t` and a debugger reads them off the
    /// wire, so they are fixed. `is_synth` is the bit that says whether the
    /// kernel made this one up, which is what decides that there is no
    /// register context to report and that nobody being home is not fatal.
    fn the_kernel_made_exceptions_are_the_ones_with_the_synth_bit() {
        let architectural = [
            (ExceptionType::General, 0x008),
            (ExceptionType::FatalPageFault, 0x108),
            (ExceptionType::UndefinedInstruction, 0x208),
            (ExceptionType::SoftwareBreakpoint, 0x308),
            (ExceptionType::HardwareBreakpoint, 0x408),
            (ExceptionType::UnalignedAccess, 0x508),
        ];
        let synthetic = [
            (ExceptionType::Synth, 0x8000),
            (ExceptionType::ThreadStarting, 0x8008),
            (ExceptionType::ThreadExiting, 0x8108),
            (ExceptionType::PolicyError, 0x8208),
            (ExceptionType::ProcessStarting, 0x8308),
        ];
        for (type_, number) in architectural {
            assert_eq!(type_ as u32, number, "{:?}", type_);
            assert!(!type_.is_synth(), "{:?}", type_);
        }
        for (type_, number) in synthetic {
            assert_eq!(type_ as u32, number, "{:?}", type_);
            assert!(type_.is_synth(), "{:?}", type_);
        }
    }
}
