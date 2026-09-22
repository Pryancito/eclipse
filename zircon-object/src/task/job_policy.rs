use crate::error::*;
use crate::signal::Slack;

/// Security and resource policies of a job.
#[derive(Default, Copy, Clone)]
pub struct JobPolicy {
    // TODO: use bitset
    action: [Option<PolicyAction>; 15],
}

/// Every condition `PolicyCondition::NewAny` stands for.
///
/// Zircon documents `ZX_POL_NEW_ANY` as "a special condition that stands for
/// all of the above ZX_NEW conditions", and expands it when the policy is
/// applied. Storing it in its own slot instead meant a job that denied
/// `NEW_ANY` denied nothing at all: every `check_policy` asks about the
/// specific condition, and no `NEW_*` slot had been written.
const NEW_ANY_EXPANSION: &[PolicyCondition] = &[
    PolicyCondition::NewVMO,
    PolicyCondition::NewChannel,
    PolicyCondition::NewEvent,
    PolicyCondition::NewEventPair,
    PolicyCondition::NewPort,
    PolicyCondition::NewSocket,
    PolicyCondition::NewFIFO,
    PolicyCondition::NewTimer,
    PolicyCondition::NewProcess,
    PolicyCondition::NewProfile,
];

impl JobPolicy {
    /// Get the action of a policy `condition`.
    pub fn get_action(&self, condition: PolicyCondition) -> Option<PolicyAction> {
        self.action[condition as usize]
    }

    /// Apply a basic policy.
    pub fn apply(&mut self, condition: PolicyCondition, action: PolicyAction) {
        self.action[condition as usize] = Some(action);
        if let PolicyCondition::NewAny = condition {
            for &condition in NEW_ANY_EXPANSION {
                self.action[condition as usize] = Some(action);
            }
        }
    }

    /// Merge the policy with `parent`'s.
    pub fn merge(&self, parent: &Self) -> Self {
        let mut new = *self;
        for i in 0..new.action.len() {
            if parent.action[i].is_some() {
                new.action[i] = parent.action[i];
            }
        }
        new
    }
}

/// Control the effect in the case of conflict between
/// the existing policies and the new policies when setting new policies.
#[derive(Debug, Copy, Clone)]
pub enum SetPolicyOptions {
    /// Policy is applied for all conditions in policy or the call fails.
    Absolute,
    /// Policy is applied for the conditions not specifically overridden by the parent policy.
    Relative,
}

/// One `zx_policy_basic_t`, exactly as it is laid out in the caller's memory.
///
/// Both fields are raw `u32` and not the enums they name, because this struct
/// is read straight out of userspace: `sys_job_set_policy` hands the caller's
/// buffer to `UserInPtr::as_slice`, so whatever the process wrote is what
/// arrives here. Reading an arbitrary `u32` as a `#[repr(u32)]` Rust enum is
/// undefined behaviour, and the concrete consequence was an unprivileged
/// process choosing `condition` freely and indexing a fifteen-slot array with
/// it. Use [`BasicPolicy::parse`], which rejects what the enums do not name.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BasicPolicy {
    /// Condition when the policy is applied, a [`PolicyCondition`] value.
    pub condition: u32,
    /// Action taken when the condition happens, a [`PolicyAction`] value.
    pub action: u32,
}

impl BasicPolicy {
    /// The condition and action this policy names.
    pub fn parse(&self) -> ZxResult<(PolicyCondition, PolicyAction)> {
        Ok((
            PolicyCondition::from_raw(self.condition)?,
            PolicyAction::from_raw(self.action)?,
        ))
    }
}

impl PolicyCondition {
    /// The condition `raw` names, or `INVALID_ARGS` when it names none.
    ///
    /// Written out rather than derived so that adding a variant without
    /// growing `JobPolicy::action` cannot compile: the array is indexed by
    /// these values.
    pub fn from_raw(raw: u32) -> ZxResult<Self> {
        use PolicyCondition::*;
        Ok(match raw {
            0 => BadHandle,
            1 => WrongObject,
            2 => VmarWx,
            3 => NewAny,
            4 => NewVMO,
            5 => NewChannel,
            6 => NewEvent,
            7 => NewEventPair,
            8 => NewPort,
            9 => NewSocket,
            10 => NewFIFO,
            11 => NewTimer,
            12 => NewProcess,
            13 => NewProfile,
            14 => AmbientMarkVMOExec,
            _ => return Err(ZxError::INVALID_ARGS),
        })
    }
}

impl PolicyAction {
    /// The action `raw` names, or `INVALID_ARGS` when it names none.
    pub fn from_raw(raw: u32) -> ZxResult<Self> {
        use PolicyAction::*;
        Ok(match raw {
            0 => Allow,
            1 => Deny,
            2 => AllowException,
            3 => DenyException,
            4 => Kill,
            _ => return Err(ZxError::INVALID_ARGS),
        })
    }
}

/// The condition when a policy is applied.
#[repr(u32)]
#[derive(Debug, Copy, Clone)]
pub enum PolicyCondition {
    /// A process under this job is attempting to issue a syscall with an invalid handle.
    /// In this case, `PolicyAction::Allow` and `PolicyAction::Deny` are equivalent:
    /// if the syscall returns, it will always return the error ZX_ERR_BAD_HANDLE.
    BadHandle = 0,
    /// A process under this job is attempting to issue a syscall with a handle that does not support such operation.
    WrongObject = 1,
    /// A process under this job is attempting to map an address region with write-execute access.
    VmarWx = 2,
    /// A special condition that stands for all of the above ZX_NEW conditions
    /// such as NEW_VMO, NEW_CHANNEL, NEW_EVENT, NEW_EVENTPAIR, NEW_PORT, NEW_SOCKET, NEW_FIFO,
    /// And any future ZX_NEW policy.
    /// This will include any new kernel objects which do not require a parent object for creation.
    NewAny = 3,
    /// A process under this job is attempting to create a new vm object.
    NewVMO = 4,
    /// A process under this job is attempting to create a new channel.
    NewChannel = 5,
    /// A process under this job is attempting to create a new event.
    NewEvent = 6,
    /// A process under this job is attempting to create a new event pair.
    NewEventPair = 7,
    /// A process under this job is attempting to create a new port.
    NewPort = 8,
    /// A process under this job is attempting to create a new socket.
    NewSocket = 9,
    /// A process under this job is attempting to create a new fifo.
    NewFIFO = 10,
    /// A process under this job is attempting to create a new timer.
    NewTimer = 11,
    /// A process under this job is attempting to create a new process.
    NewProcess = 12,
    /// A process under this job is attempting to create a new profile.
    NewProfile = 13,
    /// A process under this job is attempting to use zx_vmo_replace_as_executable()
    /// with a ZX_HANDLE_INVALID as the second argument rather than a valid ZX_RSRC_KIND_VMEX.
    AmbientMarkVMOExec = 14,
}

/// The action taken when the condition happens specified by a policy.
#[repr(u32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PolicyAction {
    /// Allow condition.
    Allow = 0,
    /// Prevent condition.
    Deny = 1,
    /// Generate an exception via the debug port. An exception generated this
    /// way acts as a breakpoint. The thread may be resumed after the exception.
    AllowException = 2,
    /// Just like `AllowException`, but after resuming condition is denied.
    DenyException = 3,
    /// Terminate the process.
    Kill = 4,
}

/// Timer slack policy.
///
/// See [timer slack](../../signal/timer/enum.Slack.html) for more information.
/// The wire form of `zx_policy_timer_slack_t`, as `sys_job_set_policy` reads it
/// out of the caller's memory. `default_mode` is a raw `u32` for the same
/// reason [`BasicPolicy`]'s fields are: see [`TimerSlackPolicy::parse`].
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TimerSlackPolicy {
    min_slack: i64,
    default_mode: u32,
}

impl TimerSlackPolicy {
    /// Check the policy the caller wrote, and return what it says.
    ///
    /// Zircon rejects a negative `min_slack` and a `default_mode` above
    /// `ZX_TIMER_SLACK_LATE`. Only the first was checked here, and the second
    /// was a `Slack` read straight out of the caller's memory, which is
    /// undefined behaviour for any value but 0, 1 and 2. `zx_timer_create`,
    /// the other way into the same enum, has always checked its mode.
    pub fn parse(&self) -> ZxResult<(i64, Slack)> {
        if self.min_slack.is_negative() {
            return Err(ZxError::INVALID_ARGS);
        }
        Ok((self.min_slack, Slack::from_raw(self.default_mode)?))
    }

    /// Build one as a caller would have written it.
    ///
    /// Test-only: the kernel only ever gets these out of user memory.
    #[cfg(test)]
    pub(crate) fn from_raw_parts(min_slack: i64, default_mode: u32) -> Self {
        Self {
            min_slack,
            default_mode,
        }
    }
}

#[repr(C)]
pub(super) struct TimerSlack {
    amount: i64,
    mode: Slack,
}

impl TimerSlack {
    pub(super) fn generate_new(&self, min_slack: i64, mode: Slack) -> TimerSlack {
        TimerSlack {
            amount: self.amount.max(min_slack),
            mode,
        }
    }

    /// What the job's timer slack currently is.
    #[cfg(test)]
    pub(super) fn parts(&self) -> (i64, Slack) {
        (self.amount, self.mode)
    }
}

impl Default for TimerSlack {
    fn default() -> Self {
        TimerSlack {
            amount: 0,
            mode: Slack::Center,
        }
    }
}

#[cfg(test)]
mod job_policy_tests {
    use super::*;

    /// `sys_job_set_policy` reads a `zx_policy_timer_slack_t` straight out of
    /// the caller's memory, so `default_mode` is whatever the process wrote.
    /// Only `min_slack` was checked, and the mode was materialised as a `Slack`
    /// with three variants. Zircon checks both.
    #[test]
    fn a_timer_slack_mode_the_caller_invented_is_rejected() {
        for (raw, expected) in [(0, Slack::Center), (1, Slack::Early), (2, Slack::Late)] {
            assert_eq!(Slack::from_raw(raw), Ok(expected));
            assert_eq!(expected as u32, raw);
            assert_eq!(
                TimerSlackPolicy::from_raw_parts(7, raw).parse(),
                Ok((7, expected))
            );
        }
        for raw in [3, 4, 0x8000, u32::MAX] {
            assert_eq!(Slack::from_raw(raw), Err(ZxError::INVALID_ARGS));
            assert_eq!(
                TimerSlackPolicy::from_raw_parts(7, raw).parse(),
                Err(ZxError::INVALID_ARGS)
            );
        }
    }

    /// The mode is checked even when `min_slack` is the thing that is wrong,
    /// and a negative one is still refused.
    #[test]
    fn a_negative_minimum_slack_is_rejected() {
        for min_slack in [-1i64, -1000, i64::MIN] {
            assert_eq!(
                TimerSlackPolicy::from_raw_parts(min_slack, 0).parse(),
                Err(ZxError::INVALID_ARGS)
            );
        }
        assert_eq!(
            TimerSlackPolicy::from_raw_parts(0, 0).parse(),
            Ok((0, Slack::Center))
        );
        assert_eq!(
            TimerSlackPolicy::from_raw_parts(i64::MAX, 2).parse(),
            Ok((i64::MAX, Slack::Late))
        );
    }

    /// The amount only ever grows: a job cannot ask for less slack than it
    /// already has, but the mode is replaced.
    #[test]
    fn the_amount_of_slack_only_grows_and_the_mode_is_replaced() {
        let slack = TimerSlack::default();
        assert_eq!(slack.parts(), (0, Slack::Center));
        let slack = slack.generate_new(500, Slack::Late);
        assert_eq!(slack.parts(), (500, Slack::Late));
        let slack = slack.generate_new(100, Slack::Early);
        assert_eq!(slack.parts(), (500, Slack::Early));
        let slack = slack.generate_new(900, Slack::Center);
        assert_eq!(slack.parts(), (900, Slack::Center));
    }

    /// `zx_policy_timer_slack_t` is sixteen bytes on the wire, and the struct
    /// the syscall reads has to stay that size.
    #[test]
    fn the_timer_slack_policy_keeps_its_wire_size() {
        assert_eq!(core::mem::size_of::<TimerSlackPolicy>(), 16);
    }

    /// One past the last condition and the last action there are.
    const CONDITION_COUNT: u32 = 15;
    const ACTION_COUNT: u32 = 5;

    /// `sys_job_set_policy` hands the caller's own buffer to
    /// `UserInPtr::as_slice`, so both fields of a `zx_policy_basic_t` are
    /// whatever the process wrote. Reading an arbitrary `u32` as a
    /// `#[repr(u32)]` enum is undefined behaviour, and the condition then
    /// indexes a fifteen-slot array.
    #[test]
    fn a_condition_or_action_outside_the_enum_is_rejected() {
        for raw in 0..CONDITION_COUNT {
            assert_eq!(PolicyCondition::from_raw(raw).unwrap() as u32, raw);
        }
        for raw in [CONDITION_COUNT, CONDITION_COUNT + 1, 0x8000, u32::MAX] {
            assert_eq!(
                PolicyCondition::from_raw(raw).err(),
                Some(ZxError::INVALID_ARGS)
            );
        }

        for raw in 0..ACTION_COUNT {
            assert_eq!(PolicyAction::from_raw(raw).unwrap() as u32, raw);
        }
        for raw in [ACTION_COUNT, ACTION_COUNT + 1, u32::MAX] {
            assert_eq!(
                PolicyAction::from_raw(raw).err(),
                Some(ZxError::INVALID_ARGS)
            );
        }

        // And the two together, which is how one arrives.
        let good = BasicPolicy {
            condition: PolicyCondition::NewPort as u32,
            action: PolicyAction::Kill as u32,
        };
        assert!(matches!(
            good.parse(),
            Ok((PolicyCondition::NewPort, PolicyAction::Kill))
        ));
        let bad_condition = BasicPolicy {
            condition: CONDITION_COUNT,
            action: PolicyAction::Allow as u32,
        };
        assert_eq!(bad_condition.parse().err(), Some(ZxError::INVALID_ARGS));
        let bad_action = BasicPolicy {
            condition: PolicyCondition::NewPort as u32,
            action: ACTION_COUNT,
        };
        assert_eq!(bad_action.parse().err(), Some(ZxError::INVALID_ARGS));
    }

    /// `ZX_POL_NEW_ANY` is documented as standing for every `ZX_NEW_*`
    /// condition, and Zircon expands it when the policy is applied. Keeping it
    /// in its own slot meant a job that denied `NEW_ANY` denied nothing:
    /// `check_policy` always asks about the specific condition.
    #[test]
    fn new_any_denies_every_kind_of_new_object() {
        let mut policy = JobPolicy::default();
        policy.apply(PolicyCondition::NewAny, PolicyAction::Deny);

        // Written out rather than read from `NEW_ANY_EXPANSION`: a test that
        // loops over the list it is checking shrinks along with it.
        for condition in [
            PolicyCondition::NewVMO,
            PolicyCondition::NewChannel,
            PolicyCondition::NewEvent,
            PolicyCondition::NewEventPair,
            PolicyCondition::NewPort,
            PolicyCondition::NewSocket,
            PolicyCondition::NewFIFO,
            PolicyCondition::NewTimer,
            PolicyCondition::NewProcess,
            PolicyCondition::NewProfile,
        ] {
            assert_eq!(
                policy.get_action(condition),
                Some(PolicyAction::Deny),
                "{:?} was not covered by NEW_ANY",
                condition
            );
        }
        assert_eq!(
            policy.get_action(PolicyCondition::NewAny),
            Some(PolicyAction::Deny)
        );

        // And nothing else is touched: NEW_ANY is about creating objects.
        for condition in [
            PolicyCondition::BadHandle,
            PolicyCondition::WrongObject,
            PolicyCondition::VmarWx,
            PolicyCondition::AmbientMarkVMOExec,
        ] {
            assert_eq!(policy.get_action(condition), None, "{:?}", condition);
        }
    }

    /// A more specific policy set after `NEW_ANY` wins, which is the point of
    /// having both.
    #[test]
    fn a_specific_new_policy_overrides_the_blanket_one() {
        let mut policy = JobPolicy::default();
        policy.apply(PolicyCondition::NewAny, PolicyAction::Deny);
        policy.apply(PolicyCondition::NewChannel, PolicyAction::Allow);

        assert_eq!(
            policy.get_action(PolicyCondition::NewChannel),
            Some(PolicyAction::Allow)
        );
        assert_eq!(
            policy.get_action(PolicyCondition::NewVMO),
            Some(PolicyAction::Deny)
        );
    }

    /// `merge` walked a hard-coded fifteen slots next to an array whose length
    /// is the thing that actually has to be walked.
    #[test]
    fn merging_covers_every_slot_a_policy_has() {
        let mut parent = JobPolicy::default();
        let mut child = JobPolicy::default();
        // The last condition there is, which a count written by hand is
        // exactly what would miss.
        parent.apply(PolicyCondition::AmbientMarkVMOExec, PolicyAction::Deny);
        child.apply(PolicyCondition::BadHandle, PolicyAction::Allow);

        let merged = child.merge(&parent);
        assert_eq!(
            merged.get_action(PolicyCondition::AmbientMarkVMOExec),
            Some(PolicyAction::Deny)
        );
        assert_eq!(
            merged.get_action(PolicyCondition::BadHandle),
            Some(PolicyAction::Allow)
        );

        // The parent wins where both have spoken.
        let mut both = JobPolicy::default();
        both.apply(PolicyCondition::BadHandle, PolicyAction::Deny);
        assert_eq!(
            both.merge(&child).get_action(PolicyCondition::BadHandle),
            Some(PolicyAction::Allow)
        );
    }
}
