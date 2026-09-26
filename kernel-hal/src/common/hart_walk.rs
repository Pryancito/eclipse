//! Which secondary harts a riscv64 machine starts, and what it does when the
//! firmware says no.
//!
//! This is the third place in the kernel that asks the same question — "the
//! firmware describes a CPU; do we start it, and what if it refuses?" — after
//! the MADT scan on x86_64 ([`crate::common::cpu_topology`]) and the affinity
//! walk on aarch64 ([`crate::common::affinity_walk`]). The other two answer it
//! by skipping the CPU and booting with the ones that came up. riscv64 answered
//! it with `panic!`, so one secondary hart that the SEE will not hand over took
//! the whole machine down before userspace existed.
//!
//! That it bites in the real world is written into the tree already:
//! `entry64.rs` carries a `#[cfg(feature = "board-fu740")] if id == 0
//! { continue; }`, a per-board hard-coded skip whose only purpose is to keep
//! the boot hart list away from a core the SEE will not start. A board nobody
//! wrote a `cfg` for has no such luck.
//!
//! Lives in `common/` rather than in `zCore/src/platform/riscv/entry.rs`
//! because the decisions here — how to read a `cpu@…` node name, what an SBI
//! error code means, and which harts the walk is allowed to hand out — are
//! arithmetic over values, not register pokes. The `ecall` stays in the boot
//! code; the policy is here, where the host test suite can reach it.

/// SBI error codes, as the specification numbers them. The `ecall` hands them
/// back in `a0` as a `usize` holding a two's-complement negative, which is why
/// [`outcome`] takes a `usize` and casts: the boot code must not have to know
/// how to squint at the raw word.
pub mod sbi {
    pub const SUCCESS: isize = 0;
    pub const FAILED: isize = -1;
    pub const NOT_SUPPORTED: isize = -2;
    pub const INVALID_PARAM: isize = -3;
    pub const DENIED: isize = -4;
    pub const INVALID_ADDRESS: isize = -5;
    pub const ALREADY_AVAILABLE: isize = -6;
    pub const ALREADY_STARTED: isize = -7;
    pub const ALREADY_STOPPED: isize = -8;
}

/// What `HART_START` returning `ret` means for the machine we are booting.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    /// The hart took the entry address and is on its way up.
    Started,
    /// The hart was already running. Expected when a previous stage started it
    /// or when the device tree lists one core twice; not a failure, but not a
    /// hart this boot brought up either.
    AlreadyOn,
    /// The SEE does not own this hart, or will not give it to us. The device
    /// tree said it exists; the firmware disagrees, and the firmware wins.
    /// Expected on boards with a monitor or management core, so it is not
    /// worth a warning, let alone a panic.
    Absent,
    /// Something we did was wrong, or the SEE failed internally. Carries the
    /// code so the boot log names it instead of "failed".
    Failed(isize),
}

/// Classify a raw `HART_START` return word.
///
/// [`sbi::INVALID_ADDRESS`] is deliberately **not** [`Outcome::Absent`]: it is
/// a complaint about *our* entry address, not about the hart, so it will come
/// back identically for every hart in the list. Filing it with the absent ones
/// would turn "the kernel passed a physical address the SEE cannot jump to"
/// into a silent single-core boot.
pub fn outcome(ret: usize) -> Outcome {
    match ret as isize {
        sbi::SUCCESS => Outcome::Started,
        sbi::ALREADY_AVAILABLE | sbi::ALREADY_STARTED => Outcome::AlreadyOn,
        sbi::INVALID_PARAM | sbi::DENIED | sbi::NOT_SUPPORTED => Outcome::Absent,
        other => Outcome::Failed(other),
    }
}

/// Read the hart id out of a `/cpus/cpu@<unit-address>` node name.
///
/// The walk this replaces did
///
/// ```text
/// usize::from_str_radix(core::str::from_utf8_unchecked(&name[4..]), 16).unwrap()
/// ```
///
/// on a byte string that came from the device tree the firmware handed us, so
/// a `cpu@` node whose unit address was not plain hex panicked the kernel
/// before the console was fully up. A unit address is allowed to carry more
/// than one cell, separated by commas (`cpu@0,0`), and a truncated or
/// mis-generated blob can leave anything at all there. None of that is worth
/// the machine.
///
/// Returns `None` for a name this function will not vouch for; the caller says
/// so in the boot log and walks on to the next node.
pub fn parse_hart_id(node_name: &str) -> Option<usize> {
    let addr = node_name.strip_prefix("cpu@")?;
    // Only the first cell of the unit address names the hart; `cpu@0,0` is one
    // hart, not a parse error.
    let first = match addr.find(',') {
        Some(comma) => &addr[..comma],
        None => addr,
    };
    // `from_str_radix` would take a sign, and `cpu@+1` is not a hart id -- it
    // is a blob this kernel should not be guessing about. An empty address
    // needs no check of its own: it has no digits, so the parse refuses it.
    if !first.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    usize::from_str_radix(first, 16).ok()
}

/// The device-tree walk over `/cpus`, as a state machine.
///
/// The shape is dictated by `dtb-walker`, which streams the tree through one
/// callback and never says "the walk is over": a `cpu@…` node's `status`
/// property arrives *after* the node itself, so the id has to be held pending
/// until either the next node begins or the walk ends. The code this replaces
/// held that pending id too, and flushed it only when it saw the next
/// **root-level** node after `/cpus`. A device tree whose `/cpus` is the last
/// child of the root — nothing forbids it, the order is the blob author's
/// choice — therefore lost its last hart, silently, every boot.
///
/// [`finish`](Self::finish) is that missing end of the walk.
pub struct HartWalk {
    boot: usize,
    /// Highest hart id + 1 that the boot stack array has a slot for. A hart
    /// past it is parked by the entry assembly the instant it arrives, with no
    /// stack and a message nobody is reading yet, so starting it buys a scary
    /// line and nothing else.
    stack_slots: usize,
    /// How many secondaries the per-CPU tables still have room for.
    room: usize,
    pending: Option<usize>,
    issued: usize,
    started: usize,
    already: usize,
    absent: usize,
    failures: usize,
}

/// Why a hart the device tree listed was not handed out.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Skip {
    /// It is the hart running this code.
    BootHart,
    /// Its id is past the boot stack array.
    NoStack,
    /// The per-CPU tables are full.
    NoRoom,
}

/// What the walk has for the caller after one device-tree event.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Next {
    /// Nothing was pending; carry on reading the tree.
    Nothing,
    /// Start this hart, then report the answer with [`HartWalk::saw`].
    Start(usize),
    /// This hart was listed but will not be started, for this reason. Worth a
    /// line in the boot log and nothing else.
    Skip(usize, Skip),
}

impl HartWalk {
    pub const fn new(boot: usize, stack_slots: usize, room: usize) -> Self {
        Self {
            boot,
            stack_slots,
            room,
            pending: None,
            issued: 0,
            started: 0,
            already: 0,
            absent: 0,
            failures: 0,
        }
    }

    /// A `/cpus/cpu@...` node has begun. `id` is [`parse_hart_id`]'s answer for
    /// its name, or `None` when the name did not name a hart -- which the
    /// caller has already said in the boot log, because only the caller has
    /// the name.
    ///
    /// Returns what to do about the *previous* node, now that its `status` has
    /// been seen and cannot arrive any more.
    pub fn see_cpu_node(&mut self, id: Option<usize>) -> Next {
        let ready = self.flush();
        self.pending = id;
        ready
    }

    /// The pending node's `status` property. Anything but `"okay"` drops it:
    /// the device tree is telling us this core is fused off, reserved, or
    /// failed self-test. Returns the hart id it dropped, for the boot log.
    pub fn see_status(&mut self, status: &str) -> Option<usize> {
        if status == "okay" {
            return None;
        }
        self.pending.take()
    }

    /// The walk is over. Returns the last pending hart, which is the one the
    /// old walk dropped when `/cpus` had no root-level sibling after it.
    pub fn finish(&mut self) -> Next {
        self.flush()
    }

    fn flush(&mut self) -> Next {
        let Some(hart) = self.pending.take() else {
            return Next::Nothing;
        };
        if hart == self.boot {
            Next::Skip(hart, Skip::BootHart)
        } else if hart >= self.stack_slots {
            Next::Skip(hart, Skip::NoStack)
        } else if self.issued >= self.room {
            Next::Skip(hart, Skip::NoRoom)
        } else {
            self.issued += 1;
            Next::Start(hart)
        }
    }

    /// Record what the firmware answered for the hart just handed out. Called
    /// once per [`see_cpu_node`](Self::see_cpu_node) or
    /// [`finish`](Self::finish) that answered [`Next::Start`].
    pub fn saw(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Started => self.started += 1,
            Outcome::AlreadyOn => self.already += 1,
            Outcome::Absent => self.absent += 1,
            Outcome::Failed(_) => self.failures += 1,
        }
    }

    /// Harts this boot actually brought up.
    pub fn started(&self) -> usize {
        self.started
    }

    /// Harts that were already running when we asked.
    pub fn already_on(&self) -> usize {
        self.already
    }

    /// Harts the device tree listed that the firmware does not own.
    pub fn absent(&self) -> usize {
        self.absent
    }

    /// Harts that exist and would not start. The one number worth a warning at
    /// the end of the boot: an absent hart is a board's business, a refusal is
    /// a fault.
    pub fn failures(&self) -> usize {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// One `/cpus/cpu@…` node as the device tree presents it: a name, and the
    /// `status` property if it has one (a node without it is `okay` by the
    /// device-tree specification, which is why `None` is not the same as
    /// `Some("okay")` here — both are started, but by different code paths).
    struct Node {
        name: &'static str,
        status: Option<&'static str>,
    }

    const fn node(name: &'static str) -> Node {
        Node { name, status: None }
    }

    const fn node_with(name: &'static str, status: &'static str) -> Node {
        Node {
            name,
            status: Some(status),
        }
    }

    /// A synthetic SEE: `owns` is the set of harts it will start, and
    /// `refuses` the ones that exist but answer with a failure.
    struct See {
        owns: &'static [usize],
        running: &'static [usize],
        refuses: &'static [usize],
    }

    impl See {
        const fn new(owns: &'static [usize]) -> Self {
            Self {
                owns,
                running: &[],
                refuses: &[],
            }
        }

        fn hart_start(&self, hart: usize) -> usize {
            if self.refuses.contains(&hart) {
                return sbi::FAILED as usize;
            }
            if self.running.contains(&hart) {
                return sbi::ALREADY_AVAILABLE as usize;
            }
            if self.owns.contains(&hart) {
                return sbi::SUCCESS as usize;
            }
            sbi::INVALID_PARAM as usize
        }
    }

    /// Drive a whole device tree through the walk, exactly as
    /// `boot_secondary_harts` does: every node, then the end of the walk.
    /// Returns the harts handed out, the ones refused with a reason, and the
    /// walk itself for its tallies.
    fn walk(
        boot: usize,
        stack_slots: usize,
        room: usize,
        nodes: &[Node],
        see: &See,
    ) -> (Vec<usize>, Vec<(usize, Skip)>, HartWalk) {
        let mut w = HartWalk::new(boot, stack_slots, room);
        let mut handed = Vec::new();
        let mut skipped = Vec::new();
        let take = |w: &mut HartWalk,
                    next: Next,
                    handed: &mut Vec<usize>,
                    skipped: &mut Vec<(usize, Skip)>| match next {
            Next::Nothing => {}
            Next::Skip(hart, why) => skipped.push((hart, why)),
            Next::Start(hart) => {
                handed.push(hart);
                w.saw(outcome(see.hart_start(hart)));
            }
        };
        for n in nodes {
            let next = w.see_cpu_node(parse_hart_id(n.name));
            take(&mut w, next, &mut handed, &mut skipped);
            if let Some(status) = n.status {
                w.see_status(status);
            }
        }
        let next = w.finish();
        take(&mut w, next, &mut handed, &mut skipped);
        (handed, skipped, w)
    }

    /// The default shape of a QEMU-like four-hart board.
    const FOUR: [Node; 4] = [node("cpu@0"), node("cpu@1"), node("cpu@2"), node("cpu@3")];

    #[test]
    fn the_last_cpu_node_is_started_even_when_nothing_follows_the_cpus_node() {
        // The walk this replaces flushed its pending hart only when it saw the
        // next root-level node after `/cpus`. A blob whose `/cpus` is the last
        // child of the root reached the end of the tree with one hart still
        // pending and started three of four cores, every boot, saying nothing.
        let see = See::new(&[1, 2, 3]);
        let (handed, _, w) = walk(0, 8, 63, &FOUR, &see);
        assert_eq!(handed, [1, 2, 3]);
        assert_eq!(w.started(), 3);
    }

    #[test]
    fn a_two_hart_board_starts_its_one_secondary() {
        // The smallest machine where the missing end of the walk costs
        // everything: one secondary, pending when the tree runs out.
        let see = See::new(&[1]);
        let (handed, _, w) = walk(0, 8, 63, &[node("cpu@0"), node("cpu@1")], &see);
        assert_eq!(handed, [1]);
        assert_eq!(w.started(), 1);
    }

    #[test]
    fn a_cpu_node_the_device_tree_marks_disabled_is_not_started() {
        let see = See::new(&[1, 2, 3]);
        let nodes = [node("cpu@0"), node_with("cpu@1", "disabled"), node("cpu@2")];
        let (handed, _, _) = walk(0, 8, 63, &nodes, &see);
        assert_eq!(handed, [2]);
    }

    #[test]
    fn a_status_property_only_speaks_for_the_node_it_is_inside() {
        // `status` arrives after the node begins, so a walk that applied it to
        // whatever was pending at the time would kill the node before it.
        let see = See::new(&[1, 2]);
        let nodes = [
            node("cpu@0"),
            node("cpu@1"),
            node_with("cpu@2", "fail"),
            node("cpu@3"),
        ];
        let (handed, _, _) = walk(0, 8, 63, &nodes, &see);
        assert!(handed.contains(&1), "hart 1 lost to hart 2's status");
        assert!(!handed.contains(&2));
    }

    #[test]
    fn a_cpu_node_that_says_it_is_okay_is_started_like_one_that_says_nothing() {
        let see = See::new(&[1]);
        let (with, _, _) = walk(0, 8, 63, &[node_with("cpu@1", "okay")], &see);
        let (without, _, _) = walk(0, 8, 63, &[node("cpu@1")], &see);
        assert_eq!(with, [1]);
        assert_eq!(with, without);
    }

    #[test]
    fn the_boot_hart_is_not_asked_to_start_itself() {
        let see = See::new(&[0, 1, 2, 3]);
        let (handed, skipped, _) = walk(0, 8, 63, &FOUR, &see);
        assert_eq!(handed, [1, 2, 3]);
        assert_eq!(skipped, [(0, Skip::BootHart)]);
    }

    #[test]
    fn a_board_whose_firmware_booted_on_a_later_hart_starts_hart_zero() {
        // The fu740's boot hart is not hart 0, and neither is the C910's on
        // every configuration. A walk that assumed hart 0 was always the one
        // running this code would both skip a real core and send a start to
        // itself.
        let see = See::new(&[0, 2, 3]);
        let (handed, skipped, _) = walk(1, 8, 63, &FOUR, &see);
        assert_eq!(handed, [0, 2, 3]);
        assert_eq!(skipped, [(1, Skip::BootHart)]);
    }

    #[test]
    fn firmware_refusing_one_hart_does_not_stop_the_ones_behind_it() {
        // The whole point of the tanda: `boot_secondary_harts` answered a
        // refusal with `panic!`, so one core the SEE would not hand over took
        // down a machine that had three more waiting.
        let see = See {
            owns: &[1, 2, 3],
            running: &[],
            refuses: &[1],
        };
        let (handed, _, w) = walk(0, 8, 63, &FOUR, &see);
        assert_eq!(handed, [1, 2, 3]);
        assert_eq!(w.failures(), 1);
        assert_eq!(w.started(), 2);
    }

    #[test]
    fn a_hart_the_firmware_does_not_own_is_expected_and_not_a_failure() {
        // A management or monitor core listed in `/cpus` that the SEE keeps
        // for itself. `entry64.rs` carries a `board-fu740` `cfg` that skips
        // hart 0 by hand for exactly this; a board nobody wrote a `cfg` for
        // must still boot.
        let see = See::new(&[2, 3]);
        let (handed, _, w) = walk(0, 8, 63, &FOUR, &see);
        assert_eq!(handed, [1, 2, 3]);
        assert_eq!(w.absent(), 1);
        assert_eq!(w.failures(), 0, "an absent hart is not a fault");
        assert_eq!(w.started(), 2);
    }

    #[test]
    fn a_hart_already_running_is_not_counted_as_one_we_started() {
        let see = See {
            owns: &[2, 3],
            running: &[1],
            refuses: &[],
        };
        let (_, _, w) = walk(0, 8, 63, &FOUR, &see);
        assert_eq!(w.already_on(), 1);
        assert_eq!(w.started(), 2);
        assert_eq!(w.failures(), 0);
    }

    #[test]
    fn a_core_the_device_tree_lists_twice_does_not_count_as_two() {
        // A duplicated `cpu@` node is a blob bug, not ours, and the second
        // `HART_START` comes back ALREADY_AVAILABLE. Counting it as started
        // would report more CPUs than the machine has.
        let see = See {
            owns: &[1],
            running: &[],
            refuses: &[],
        };
        let nodes = [node("cpu@0"), node("cpu@1"), node("cpu@1")];
        let (handed, _, w) = walk(0, 8, 63, &nodes, &see);
        assert_eq!(handed, [1, 1]);
        // The synthetic SEE answers SUCCESS twice because it has no state; the
        // tally that matters is that a real one's ALREADY_AVAILABLE lands in
        // `already_on`, which `a_hart_already_running_...` pins down.
        assert_eq!(w.started() + w.already_on(), 2);
    }

    #[test]
    fn a_hart_past_the_boot_stack_array_is_not_started() {
        // The entry assembly parks a hart whose *raw* id has no slot in
        // `BOOT_STACK`, with no stack and a message written before the console
        // is up. Starting it costs a core nothing and the log a scare.
        let see = See::new(&[1, 2, 8, 9]);
        let nodes = [node("cpu@0"), node("cpu@1"), node("cpu@8"), node("cpu@9")];
        let (handed, skipped, _) = walk(0, 8, 63, &nodes, &see);
        assert_eq!(handed, [1]);
        assert_eq!(
            skipped,
            [(0, Skip::BootHart), (8, Skip::NoStack), (9, Skip::NoStack)]
        );
    }

    #[test]
    fn the_highest_hart_the_boot_stack_holds_is_a_usable_one() {
        // The bound is a count of slots, so the last usable id is one below
        // it. An off-by-one here silently costs the top core of every board
        // that fills its stack array.
        let see = See::new(&[7]);
        let (handed, _, _) = walk(0, 8, 63, &[node("cpu@7")], &see);
        assert_eq!(handed, [7]);
    }

    #[test]
    fn the_walk_stops_handing_out_harts_once_the_per_cpu_tables_are_full() {
        let see = See::new(&[1, 2, 3]);
        let (handed, skipped, _) = walk(0, 8, 2, &FOUR, &see);
        assert_eq!(handed, [1, 2]);
        assert_eq!(skipped, [(0, Skip::BootHart), (3, Skip::NoRoom)]);
    }

    #[test]
    fn a_walk_with_no_room_left_starts_nothing() {
        let see = See::new(&[1, 2, 3]);
        let (handed, _, w) = walk(0, 8, 0, &FOUR, &see);
        assert!(handed.is_empty());
        assert_eq!(w.started(), 0);
    }

    #[test]
    fn a_hart_the_walk_refused_does_not_spend_room_a_real_one_needs() {
        // The boot hart and the stackless ones must not count against the
        // budget: they were never handed out, so charging them for it would
        // cost the tail of the list a core apiece.
        let see = See::new(&[1, 2, 3]);
        let nodes = [node("cpu@0"), node("cpu@9"), node("cpu@1"), node("cpu@2")];
        let (handed, _, _) = walk(0, 8, 2, &nodes, &see);
        assert_eq!(handed, [1, 2]);
    }

    #[test]
    fn a_node_name_that_does_not_name_a_hart_does_not_stop_the_boot() {
        // `usize::from_str_radix(&name[4..], 16).unwrap()` on a string the
        // firmware wrote. A blob that is truncated, mis-generated, or simply
        // uses a form this kernel did not expect panicked the machine here.
        let see = See::new(&[1, 2]);
        let nodes = [node("cpu@0"), node("cpu@zz"), node("cpu@2")];
        let (handed, _, _) = walk(0, 8, 63, &nodes, &see);
        assert_eq!(handed, [2]);
    }

    #[test]
    fn a_unit_address_with_more_than_one_cell_names_one_hart() {
        // A unit address may carry several comma-separated cells. `cpu@1,0` is
        // hart 1, not a reason to abort the boot.
        assert_eq!(parse_hart_id("cpu@1,0"), Some(1));
        assert_eq!(parse_hart_id("cpu@0,0"), Some(0));
    }

    #[test]
    fn hart_ids_are_read_as_hexadecimal_and_not_as_decimal() {
        // Device-tree unit addresses are hex, with no `0x`. Reading `cpu@10`
        // as ten would point the SBI call at the wrong core and leave hart 16
        // asleep.
        assert_eq!(parse_hart_id("cpu@10"), Some(0x10));
        assert_eq!(parse_hart_id("cpu@ff"), Some(0xff));
    }

    #[test]
    fn a_unit_address_is_read_the_same_in_upper_and_lower_case() {
        assert_eq!(parse_hart_id("cpu@AB"), parse_hart_id("cpu@ab"));
        assert_eq!(parse_hart_id("cpu@AB"), Some(0xab));
    }

    #[test]
    fn an_empty_unit_address_is_not_a_hart_id() {
        assert_eq!(parse_hart_id("cpu@"), None);
        assert_eq!(parse_hart_id("cpu@,0"), None);
    }

    #[test]
    fn a_unit_address_that_is_not_hex_is_not_a_hart_id() {
        assert_eq!(parse_hart_id("cpu@0x1"), None);
        assert_eq!(parse_hart_id("cpu@ -1"), None);
        assert_eq!(parse_hart_id("cpu@1g"), None);
        // `from_str_radix` takes a sign; a device-tree unit address does not.
        assert_eq!(parse_hart_id("cpu@+1"), None);
        assert_eq!(parse_hart_id("cpu@-1"), None);
    }

    #[test]
    fn a_unit_address_too_big_for_a_usize_is_refused_rather_than_wrapped() {
        // Wrapping would name some other hart, and that hart is real.
        let too_big = "cpu@ffffffffffffffffff";
        assert_eq!(parse_hart_id(too_big), None);
    }

    #[test]
    fn a_node_that_is_not_a_cpu_node_has_no_hart_id() {
        // `/cpus` also holds `cpu-map` and, on some boards, a plain `cpu` node
        // with no unit address at all.
        assert_eq!(parse_hart_id("cpu-map"), None);
        assert_eq!(parse_hart_id("cpu"), None);
        assert_eq!(parse_hart_id("cpus"), None);
        assert_eq!(parse_hart_id("memory@80000000"), None);
    }

    #[test]
    fn a_node_whose_name_happens_to_read_as_hex_is_not_a_hart() {
        // This takes a node *name*, not a unit address, so the `cpu@` prefix
        // is part of what makes the name a hart. Without it every node whose
        // name is spellable in hex — and a device tree has plenty of names
        // nobody chose for their digits — would be handed to `HART_START` as
        // a core, with the boot stack and a per-CPU slot spent on it.
        assert_eq!(parse_hart_id("ace"), None);
        assert_eq!(parse_hart_id("0"), None);
        assert_eq!(parse_hart_id("8"), None);
        assert_eq!(parse_hart_id("1,0"), None);
    }

    #[test]
    fn a_bad_entry_address_is_a_failure_and_not_an_absent_hart() {
        // INVALID_ADDRESS complains about the address *we* passed, so it comes
        // back for every hart in the list. Filing it with the absent ones
        // would turn "the kernel handed the SEE an entry point it cannot jump
        // to" into a silent single-core boot with nothing in the log.
        assert_eq!(
            outcome(sbi::INVALID_ADDRESS as usize),
            Outcome::Failed(sbi::INVALID_ADDRESS)
        );
    }

    #[test]
    fn firmware_saying_there_is_no_such_hart_is_expected_and_not_a_failure() {
        assert_eq!(outcome(sbi::INVALID_PARAM as usize), Outcome::Absent);
        assert_eq!(outcome(sbi::DENIED as usize), Outcome::Absent);
        assert_eq!(outcome(sbi::NOT_SUPPORTED as usize), Outcome::Absent);
    }

    #[test]
    fn a_hart_that_was_already_up_reads_as_running_and_not_as_started() {
        assert_eq!(outcome(sbi::ALREADY_AVAILABLE as usize), Outcome::AlreadyOn);
        assert_eq!(outcome(sbi::ALREADY_STARTED as usize), Outcome::AlreadyOn);
    }

    #[test]
    fn an_sbi_code_nobody_planned_for_is_reported_rather_than_swallowed() {
        // A later SBI version, or a SEE with a code of its own. Carrying the
        // number is what lets the boot log name it instead of "failed".
        assert_eq!(outcome(sbi::FAILED as usize), Outcome::Failed(sbi::FAILED));
        assert_eq!(outcome(-42isize as usize), Outcome::Failed(-42));
        assert_eq!(
            outcome(sbi::ALREADY_STOPPED as usize),
            Outcome::Failed(sbi::ALREADY_STOPPED)
        );
    }

    #[test]
    fn a_zero_return_is_the_only_one_that_means_the_hart_is_coming_up() {
        assert_eq!(outcome(sbi::SUCCESS as usize), Outcome::Started);
        for code in 1..=9isize {
            assert_ne!(
                outcome(-code as usize),
                Outcome::Started,
                "SBI error {} read as a successful start",
                -code
            );
        }
    }
}
