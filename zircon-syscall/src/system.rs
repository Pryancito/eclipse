use super::*;
use zircon_object::dev::{Resource, SystemResource};
use zircon_object::{signal::Event, task::Job};

impl Syscall<'_> {
    /// Retrieve a handle to a system event.
    ///
    /// `root_job: HandleValue`, must be a handle to the root job of the system.
    /// `kind: u32`, must be one of the following:
    /// ```rust
    ///     const EVENT_OUT_OF_MEMORY: u32 = 1;
    ///     const EVENT_MEMORY_PRESSURE_CRITICAL: u32 = 2;
    ///     const EVENT_MEMORY_PRESSURE_WARNING: u32 = 3;
    ///     const EVENT_MEMORY_PRESSURE_NORMAL: u32 = 4;
    /// ```
    pub fn sys_system_get_event(
        &self,
        root_job: HandleValue,
        kind: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "system.get_event: root_job={:#x}, kind={:#x}, out_ptr={:#x?}",
            root_job, kind, out
        );
        if !matches!(
            kind,
            EVENT_OUT_OF_MEMORY
                | EVENT_MEMORY_PRESSURE_CRITICAL
                | EVENT_MEMORY_PRESSURE_WARNING
                | EVENT_MEMORY_PRESSURE_NORMAL
                | EVENT_IMMINENT_OUT_OF_MEMORY
        ) {
            return Err(ZxError::INVALID_ARGS);
        }

        let proc = self.thread.proc();
        proc.get_object_with_rights::<Job>(root_job, Rights::MANAGE_PROCESS)?
            .check_root_job()?;
        // Expose the normal-pressure event as the single initially signaled
        // event. Memory-pressure transitions are not modeled yet.
        let event = Event::new();
        if kind == EVENT_MEMORY_PRESSURE_NORMAL {
            event.signal_set(Signal::SIGNALED);
        }
        install_handle(
            proc,
            Handle::new(event, Rights::DEFAULT_SYSTEM_EVENT_LOW_MEMORY),
            &mut out,
        )
    }
}

const EVENT_OUT_OF_MEMORY: u32 = 1;
const EVENT_MEMORY_PRESSURE_CRITICAL: u32 = 2;
const EVENT_MEMORY_PRESSURE_WARNING: u32 = 3;
const EVENT_MEMORY_PRESSURE_NORMAL: u32 = 4;
const EVENT_IMMINENT_OUT_OF_MEMORY: u32 = 5;

impl Syscall<'_> {
    /// `zx_system_powerctl`: put the machine into a different power state.
    ///
    /// Unimplemented until now, and that was not a quiet gap. `userboot-test`
    /// asks for a shutdown when the suite it ran has finished, and when the
    /// call fails it prints `Still here after poweroff` and **never
    /// returns** — so a core-tests run whose test process did not exit by
    /// itself hung for ever instead of ending. Measured in a libos build in
    /// this container, after `FutexTest.WaitExitRace` was killed by an
    /// exception:
    ///
    /// ```text
    /// *** Exit status -1028 ***
    /// Process exited, executing poweroff
    /// ERROR zircon_syscall] syscall unimplemented: SYSTEM_POWERCTL
    /// Still here after poweroff
    /// ```
    ///
    /// The handle is the **system** resource's `POWER` subrange, not the root
    /// resource: validating against `ResourceKind::ROOT` answered
    /// `WRONG_TYPE` here, because userboot carves a child of the system
    /// resource for this and passes that. See `SystemResource::Power`.
    ///
    /// Only `SHUTDOWN` is honoured. It is the one command this tree has
    /// actually seen asked for — the same run logged `a1=0x8` — and the only
    /// one whose number is therefore measured rather than remembered. Every
    /// other command keeps answering `NOT_SUPPORTED`, which is what they all
    /// did a moment ago: a constant guessed wrong here does not return an
    /// error, it resets the machine.
    pub fn sys_system_powerctl(&self, resource: HandleValue, cmd: u32, _arg: usize) -> ZxResult {
        /// `ZX_SYSTEM_POWERCTL_SHUTDOWN`. Measured, not remembered: see above.
        const SHUTDOWN: u32 = 8;

        info!("system.powerctl: resource={:#x}, cmd={:#x}", resource, cmd);
        let proc = self.thread.proc();
        proc.get_object::<Resource>(resource)?
            .validate_system(SystemResource::Power)?;
        if cmd != SHUTDOWN {
            return Err(ZxError::NOT_SUPPORTED);
        }
        // Does not come back, here or on hardware, which is why there is no
        // `Ok(())` after it: `reset` is `-> !`.
        kernel_hal::cpu::reset()
    }
}
