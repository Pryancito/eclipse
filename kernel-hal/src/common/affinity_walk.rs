//! Which cores aarch64 bring-up probes, and what PSCI's answer means.
//!
//! aarch64 has no table of the cores a machine has. riscv reads `/cpus` out of
//! the device tree and starts exactly the harts it names; x86_64 reads the ACPI
//! MADT. aarch64 has neither here, so the only way to find a core is to call
//! PSCI `CPU_ON` at an MPIDR affinity and read what comes back — which makes
//! *which affinities to try* a decision, and the one this file holds.
//!
//! The walk it replaces was a `for aff in 1..=63`, with the first
//! `INVALID_PARAMETERS` taken to mean "no more cores" and ending the loop. Its
//! own comment said what it assumed: "QEMU `virt` (single cluster) numbers
//! cores by Aff0 = 0,1,2,…". Both halves of that cost cores on anything else:
//!
//!  * **An affinity is not a core number.** It is `Aff3<<24 | Aff2<<16 |
//!    Aff1<<8 | Aff0`, and `Aff1` is the cluster. Every big.LITTLE SoC and
//!    every aarch64 server part numbers its cores `0x0,0x1,0x2,0x3,
//!    0x100,0x101,…`, so a walk that only ever varies `Aff0` **cannot address
//!    a core outside cluster 0 at all**. On a 4+4 machine it started three
//!    secondaries out of seven and the kernel reported a four-core box.
//!  * **A gap is not the end.** On that same machine `Aff0 = 4` does not
//!    exist, so the walk stopped there — before it would have reached the
//!    second cluster even if it could address it. One hole in the numbering
//!    cost every core behind it, silently: nothing in the boot log says a core
//!    was skipped, only a smaller count than the machine has.
//!
//! So the walk goes cluster by cluster, tolerates a bounded run of holes
//! inside a cluster, and stops after a bounded run of clusters that hold
//! nothing. It also never probes the boot CPU's own affinity — which the old
//! walk did whenever the firmware had not booted on `Aff0 = 0`, and PSCI
//! answered `ALREADY_ON` to the kernel asking to start the CPU it was asking
//! from.
//!
//! None of this can be tested on the machine: `bare/arch/aarch64/smp.rs` is
//! `target_os = "none"`, the emulator boots a single cluster, and the file's
//! own header says the path has never been boot-tested. The decision lives
//! here so at least the decision is tested.

/// Highest `Aff1` (cluster) the walk will look at.
///
/// Sixteen clusters is past anything that ships — the largest aarch64 parts
/// group cores in twos and fours — and the cost of the ceiling being generous
/// is only the empty-cluster budget below, never a probe per unused cluster.
pub const MAX_CLUSTER: u32 = 15;

/// Highest `Aff0` (core within a cluster) the walk will look at.
///
/// As many as the per-CPU tables could give an id to if they were all in one
/// cluster, and not one more: a core numbered past that is a core the kernel
/// has nowhere to put. It also keeps the reach the old `for aff in 1..=63` had
/// on a machine that numbers every core in `Aff0` with no cluster field at
/// all — adding clusters must not cost the flat case a single core.
pub const MAX_CORE_IN_CLUSTER: u32 = (crate::config::MAX_CORE_NUM - 1) as u32;

/// Consecutive absent affinities inside one cluster before the walk decides
/// the cluster holds nothing more.
///
/// It has to be more than one, because one hole is what the old walk died on.
/// Four covers a cluster whose firmware disabled a core or two in the middle
/// of the numbering (a binned part, or a core fused off) without walking all
/// sixteen slots of every cluster a machine does not have.
pub const CLUSTER_MISS_BUDGET: u32 = 4;

/// Consecutive clusters holding nothing before the walk stops.
///
/// Two, not one: a machine may number its clusters `0` and `2` (the encoding
/// leaves that open, and firmware that maps clusters onto sockets does it), and
/// stopping at the first empty one would lose the rest exactly as stopping at
/// the first empty *core* did.
pub const EMPTY_CLUSTER_BUDGET: u32 = 2;

/// Pack a cluster and a core into the MPIDR affinity PSCI wants.
///
/// Only `Aff1` and `Aff0` are used. `Aff2`/`Aff3` name a chip and a
/// multi-chip system, and a kernel that cannot address a second cluster has no
/// business guessing at a second socket; a machine that needs them will need a
/// real table, not a wider probe.
pub const fn affinity(cluster: u32, core: u32) -> u32 {
    (cluster << 8) | core
}

/// What PSCI's return code means for the walk.
///
/// The codes are from the PSCI specification, and the walk cares about three
/// groups rather than nine values. The old code knew two of them by name and
/// funnelled the other seven into one branch that warned and carried on —
/// which turned "this core does not exist", a thing every probing walk hits on
/// purpose, into a warning line per absent core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The core accepted the entry point and is on its way.
    Started,
    /// The core is already running. It is not ours to count as started, but it
    /// does say the cluster is populated, so the walk keeps going through it.
    AlreadyOn,
    /// There is no such core, or firmware will not let us have it. Expected:
    /// this is how a probing walk finds the edge of a cluster.
    Absent,
    /// Anything else. The core may exist; we could not start it, and that is
    /// worth saying out loud.
    Failed(i64),
}

/// PSCI return codes. Named because the walk's behaviour turns on which of
/// them mean "no such core" — the distinction the old two-constant version
/// could not make.
pub mod psci {
    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMETERS: i64 = -2;
    pub const DENIED: i64 = -3;
    pub const ALREADY_ON: i64 = -4;
    pub const ON_PENDING: i64 = -5;
    pub const INTERNAL_FAILURE: i64 = -6;
    pub const NOT_PRESENT: i64 = -7;
    pub const DISABLED: i64 = -8;
    pub const INVALID_ADDRESS: i64 = -9;
}

/// Read a PSCI `CPU_ON` return code.
pub fn outcome(ret: i64) -> Outcome {
    match ret {
        psci::SUCCESS => Outcome::Started,
        // `ON_PENDING` is a core someone else is already starting, so it is
        // not absent and not ours.
        psci::ALREADY_ON | psci::ON_PENDING => Outcome::AlreadyOn,
        // The three ways firmware says "not a core you can have". `DENIED` is
        // in here rather than in `Failed` because a core the secure world
        // keeps for itself is, to this kernel, a core that does not exist —
        // and one per boot is not news.
        psci::INVALID_PARAMETERS | psci::NOT_PRESENT | psci::DISABLED | psci::DENIED => {
            Outcome::Absent
        }
        other => Outcome::Failed(other),
    }
}

/// The next thing for the bring-up to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Call `CPU_ON` at this packed affinity, then report the answer back
    /// through [`AffinityWalk::saw`].
    Probe(u32),
    /// Nothing left worth probing.
    Done,
}

/// The cluster-by-cluster probe walk.
///
/// Drive it as
/// `while let Step::Probe(aff) = walk.next_candidate() { … walk.saw(o) }`.
/// Every probe is answered exactly once; a caller that cannot probe (no stack
/// for the core, say) stops the loop rather than reporting an answer it does
/// not have, because a walk told nothing about a candidate cannot tell a hole
/// from a core.
///
/// Deliberately not an `Iterator`: what comes next depends on the answer to
/// what came before, so a caller able to `collect()` the candidates without
/// answering any of them would get the one thing this type exists to prevent
/// — a list of affinities produced without asking whether the cores are
/// there. Hence `next_candidate` rather than `next`.
pub struct AffinityWalk {
    /// The boot CPU's own affinity, never probed.
    boot: u32,
    /// How many secondaries the per-CPU tables still have room for.
    room: usize,
    cluster: u32,
    core: u32,
    started: usize,
    misses_here: u32,
    found_here: u32,
    empty_clusters: u32,
    done: bool,
}

impl AffinityWalk {
    /// `boot` is the packed affinity of the CPU doing the bring-up, and `room`
    /// how many more CPUs the kernel's per-CPU tables hold (`MAX_CORE_NUM - 1`
    /// on a machine whose boot CPU took logical 0).
    pub const fn new(boot: u32, room: usize) -> Self {
        Self {
            boot,
            room,
            cluster: 0,
            core: 0,
            started: 0,
            misses_here: 0,
            found_here: 0,
            empty_clusters: 0,
            done: false,
        }
    }

    /// How many cores this walk started.
    pub fn started(&self) -> usize {
        self.started
    }

    /// The affinity to probe next.
    pub fn next_candidate(&mut self) -> Step {
        loop {
            if self.done || self.started >= self.room {
                return Step::Done;
            }
            if self.core > MAX_CORE_IN_CLUSTER || self.misses_here >= CLUSTER_MISS_BUDGET {
                if self.found_here == 0 {
                    self.empty_clusters += 1;
                    if self.empty_clusters >= EMPTY_CLUSTER_BUDGET {
                        self.done = true;
                        return Step::Done;
                    }
                } else {
                    self.empty_clusters = 0;
                }
                if self.cluster >= MAX_CLUSTER {
                    self.done = true;
                    return Step::Done;
                }
                self.cluster += 1;
                self.core = 0;
                self.misses_here = 0;
                self.found_here = 0;
                continue;
            }
            let aff = affinity(self.cluster, self.core);
            self.core += 1;
            if aff == self.boot {
                // Us. Not a hole in the numbering and not a core to start:
                // asking PSCI to start the CPU making the call got
                // `ALREADY_ON` and a warning that read like a firmware fault.
                self.found_here += 1;
                self.misses_here = 0;
                continue;
            }
            return Step::Probe(aff);
        }
    }

    /// Report what PSCI answered for the affinity the last
    /// [`next_candidate`](Self::next_candidate) handed out.
    pub fn saw(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Started => {
                self.started += 1;
                self.found_here += 1;
                self.misses_here = 0;
            }
            Outcome::AlreadyOn => {
                self.found_here += 1;
                self.misses_here = 0;
            }
            Outcome::Absent => self.misses_here += 1,
            // A core that exists and would not start is not a hole in the
            // numbering: counting it as one would end the cluster early, and
            // the cluster is where the cores behind it are.
            Outcome::Failed(_) => {
                self.found_here += 1;
                self.misses_here = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A machine, as the walk can see one: the affinities that exist, and what
    /// PSCI answers for each. Everything else is absent.
    struct Machine {
        boot: u32,
        present: Vec<u32>,
        /// Affinities that exist but refuse to start, with the code they give.
        refuse: Vec<(u32, i64)>,
    }

    impl Machine {
        fn new(boot: u32, present: &[u32]) -> Self {
            Self {
                boot,
                present: present.to_vec(),
                refuse: Vec::new(),
            }
        }

        fn refusing(mut self, aff: u32, code: i64) -> Self {
            self.refuse.push((aff, code));
            self
        }

        fn cpu_on(&self, aff: u32) -> i64 {
            if let Some((_, code)) = self.refuse.iter().find(|(a, _)| *a == aff) {
                return *code;
            }
            if aff == self.boot {
                return psci::ALREADY_ON;
            }
            if self.present.contains(&aff) {
                psci::SUCCESS
            } else {
                psci::NOT_PRESENT
            }
        }
    }

    /// Run a whole bring-up against a machine. Returns the affinities started,
    /// in order, and how many probes it took.
    fn walk(m: &Machine, room: usize) -> (Vec<u32>, usize) {
        let mut w = AffinityWalk::new(m.boot, room);
        let mut started = Vec::new();
        let mut probes = 0usize;
        // A hard ceiling on iterations, so a walk that does not terminate
        // fails by name instead of hanging the suite: sixteen clusters of
        // sixteen cores is every affinity this walk can name.
        let ceiling = ((MAX_CLUSTER + 1) * (MAX_CORE_IN_CLUSTER + 1)) as usize + 1;
        while let Step::Probe(aff) = w.next_candidate() {
            probes += 1;
            assert!(
                probes <= ceiling,
                "the walk probed {} times with only {} affinities to name: it does not end",
                probes,
                ceiling
            );
            let o = outcome(m.cpu_on(aff));
            if o == Outcome::Started {
                started.push(aff);
            }
            w.saw(o);
        }
        assert_eq!(
            started.len(),
            w.started(),
            "the walk miscounted what it started"
        );
        (started, probes)
    }

    #[test]
    fn the_affinity_packs_the_cluster_above_the_core() {
        // `Aff1 << 8 | Aff0`. Getting this wrong aims every probe at the wrong
        // core, and on a single-cluster machine it would still look right.
        assert_eq!(affinity(0, 0), 0x000);
        assert_eq!(affinity(0, 3), 0x003);
        assert_eq!(affinity(1, 0), 0x100);
        assert_eq!(affinity(1, 2), 0x102);
        assert_eq!(affinity(2, 15), 0x20f);
    }

    #[test]
    fn a_single_cluster_machine_starts_every_core_it_has() {
        let m = Machine::new(0, &[0x0, 0x1, 0x2, 0x3]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x1, 0x2, 0x3]);
    }

    #[test]
    fn a_two_cluster_machine_starts_the_cluster_the_old_walk_could_not_address() {
        // The 4+4 case: `for aff in 1..=63` can only ever name `Aff0`, so
        // every core of cluster 1 was unreachable and the kernel reported a
        // four-core machine.
        let m = Machine::new(0, &[0x0, 0x1, 0x2, 0x3, 0x100, 0x101, 0x102, 0x103]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x1, 0x2, 0x3, 0x100, 0x101, 0x102, 0x103]);
    }

    #[test]
    fn one_hole_in_the_numbering_does_not_cost_the_cores_behind_it() {
        // The old walk ended on the first `INVALID_PARAMETERS`, so a core
        // fused off in the middle of the numbering took every core after it
        // with it — and said nothing, only reported a smaller count.
        let m = Machine::new(0, &[0x0, 0x1, 0x3, 0x4]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x1, 0x3, 0x4]);
    }

    #[test]
    fn clusters_numbered_with_a_gap_are_still_found() {
        // Firmware that maps clusters onto sockets leaves `Aff1 = 1` empty.
        // Stopping at the first empty cluster would lose the rest exactly as
        // stopping at the first empty core did.
        let m = Machine::new(0, &[0x0, 0x1, 0x200, 0x201]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x1, 0x200, 0x201]);
    }

    #[test]
    fn a_machine_whose_firmware_did_not_boot_on_core_zero_is_not_asked_to_start_itself() {
        // `for aff in 1..=63` assumed the boot CPU is affinity 0, so on a
        // machine that booted on another core the kernel called `CPU_ON` at
        // the CPU making the call and warned about the `ALREADY_ON` it got
        // back, as though firmware had done something wrong.
        let m = Machine::new(0x2, &[0x0, 0x1, 0x2, 0x3]);
        let mut w = AffinityWalk::new(m.boot, 63);
        let mut probed = Vec::new();
        while let Step::Probe(aff) = w.next_candidate() {
            probed.push(aff);
            w.saw(outcome(m.cpu_on(aff)));
        }
        assert!(
            !probed.contains(&0x2),
            "the walk asked PSCI to start the CPU it was asking from"
        );
        assert_eq!(w.started(), 3, "the other three cores are still started");
    }

    #[test]
    fn the_boot_cpu_does_not_read_as_a_hole_in_its_cluster() {
        // Skipping our own affinity must not spend a miss. The boot CPU here
        // sits right after a run of holes one short of the budget, so an
        // accounting that charged us a miss would end the cluster on the CPU
        // doing the bring-up and lose every core numbered after it.
        let holes = CLUSTER_MISS_BUDGET - 1;
        let boot = affinity(0, 1 + holes);
        let mut present = vec![0x0, boot];
        present.extend((1..=3).map(|k| boot + k));
        let m = Machine::new(boot, &present);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x0, boot + 1, boot + 2, boot + 3]);
    }

    #[test]
    fn a_core_that_exists_but_will_not_start_does_not_end_its_cluster() {
        // An `INTERNAL_FAILURE` is a core that is there, so it says the
        // cluster is populated. Counting it as a hole would end the cluster
        // early, and the cluster is where the cores behind it are.
        // A whole budget's worth of them in a row, so that counting one as a
        // hole would end the cluster and lose the cores behind it.
        let m = Machine::new(0, &[0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6])
            .refusing(0x1, psci::INTERNAL_FAILURE)
            .refusing(0x2, psci::INVALID_ADDRESS)
            .refusing(0x3, psci::NOT_SUPPORTED)
            .refusing(0x4, psci::INTERNAL_FAILURE);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x5, 0x6]);
    }

    #[test]
    fn a_core_already_running_is_not_counted_as_one_we_started() {
        let m = Machine::new(0, &[0x0, 0x1, 0x2]).refusing(0x1, psci::ALREADY_ON);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x2]);
        // But it is not a hole either, so 0x2 behind it is still reached —
        // which the assertion above already proves.
    }

    #[test]
    fn firmware_saying_there_is_no_such_core_is_expected_and_not_a_failure() {
        // A probing walk hits these on purpose, once per cluster edge. The old
        // code knew two return codes by name and warned about the other seven,
        // so a machine with three clusters printed a line per absent core.
        assert_eq!(outcome(psci::INVALID_PARAMETERS), Outcome::Absent);
        assert_eq!(outcome(psci::NOT_PRESENT), Outcome::Absent);
        assert_eq!(outcome(psci::DISABLED), Outcome::Absent);
        // A core the secure world keeps for itself is, to this kernel, a core
        // that does not exist.
        assert_eq!(outcome(psci::DENIED), Outcome::Absent);
    }

    #[test]
    fn a_core_on_its_way_up_is_not_absent_and_not_ours() {
        assert_eq!(outcome(psci::ALREADY_ON), Outcome::AlreadyOn);
        assert_eq!(outcome(psci::ON_PENDING), Outcome::AlreadyOn);
        assert_eq!(outcome(psci::SUCCESS), Outcome::Started);
    }

    #[test]
    fn a_return_code_nobody_planned_for_is_reported_rather_than_swallowed() {
        assert_eq!(outcome(psci::INTERNAL_FAILURE), Outcome::Failed(-6));
        assert_eq!(outcome(psci::NOT_SUPPORTED), Outcome::Failed(-1));
        assert_eq!(outcome(psci::INVALID_ADDRESS), Outcome::Failed(-9));
        assert_eq!(outcome(-42), Outcome::Failed(-42));
        // A positive code is not success either: `CPU_ON` returns 0 or an
        // error, so anything else is firmware the kernel does not understand.
        assert_eq!(outcome(7), Outcome::Failed(7));
    }

    #[test]
    fn the_walk_stops_once_the_per_cpu_tables_are_full() {
        // The tables are what the dense logical ids index. A core started past
        // them gets no id, and `register_logical_id` has to refuse it after it
        // is already running.
        let m = Machine::new(0, &[0x0, 0x1, 0x2, 0x3, 0x4, 0x5]);
        let (started, _) = walk(&m, 2);
        assert_eq!(started, [0x1, 0x2]);
    }

    #[test]
    fn a_walk_with_no_room_left_probes_nothing() {
        let m = Machine::new(0, &[0x0, 0x1, 0x2]);
        let (started, probes) = walk(&m, 0);
        assert!(started.is_empty());
        assert_eq!(
            probes, 0,
            "a probe with nowhere to put the core is a core started for nothing"
        );
    }

    #[test]
    fn a_single_core_machine_gives_up_after_a_bounded_number_of_probes() {
        // Every probe is an HVC trap into firmware, so the walk that finds
        // nothing must be cheap, not merely finite.
        let m = Machine::new(0, &[0x0]);
        let (started, probes) = walk(&m, 63);
        assert!(started.is_empty());
        assert!(
            probes <= (CLUSTER_MISS_BUDGET * (EMPTY_CLUSTER_BUDGET + 1)) as usize,
            "a uniprocessor boot paid {} PSCI calls to find out it is one",
            probes
        );
    }

    #[test]
    fn a_flat_numbered_machine_keeps_every_core_the_old_walk_reached() {
        // No cluster field at all, every core in `Aff0`. This is the case the
        // walk being replaced *did* handle, so it is the one a cluster-aware
        // walk must not quietly shrink: the last core of the run has to be
        // reached, and an off-by-one here loses one core per cluster, which on
        // a four-cluster machine is four cores nobody would miss by reading a
        // boot log.
        let full: Vec<u32> = (0..=MAX_CORE_IN_CLUSTER).map(|k| affinity(0, k)).collect();
        let m = Machine::new(0, &full);
        let (started, _) = walk(&m, MAX_CORE_IN_CLUSTER as usize);
        assert_eq!(started.len(), MAX_CORE_IN_CLUSTER as usize);
        assert_eq!(*started.last().unwrap(), affinity(0, MAX_CORE_IN_CLUSTER));
    }

    #[test]
    fn a_machine_whose_only_cluster_is_not_cluster_zero_is_still_found() {
        // Nothing says firmware has to number its one cluster `0`, and a walk
        // that gave up on the first empty cluster would report a uniprocessor
        // machine here.
        let m = Machine::new(0x100, &[0x100, 0x101, 0x102, 0x103]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x101, 0x102, 0x103]);
    }

    #[test]
    fn a_populated_cluster_clears_the_empty_run_behind_it() {
        // The empty-cluster run counts *consecutive* empties. A machine whose
        // clusters alternate — populated, empty, populated — must not have the
        // first empty one count towards the second: that would make two
        // clusters spread across the machine look like two in a row and end
        // the walk in the middle of it.
        let m = Machine::new(0x100, &[0x100, 0x101, 0x300, 0x301]);
        let (started, _) = walk(&m, 63);
        assert_eq!(started, [0x101, 0x300, 0x301]);
    }

    #[test]
    fn every_cluster_the_walk_can_name_is_actually_looked_at() {
        // `MAX_CLUSTER` is the hard ceiling that makes the walk terminate
        // whatever the budgets say, so it has to be an inclusive bound: off by
        // one there is a whole cluster of cores nobody ever probes. Needs a
        // machine populated everywhere and room for all of it, because the
        // empty-cluster budget stops a realistic machine long before here.
        let all: Vec<u32> = (0..=MAX_CLUSTER)
            .flat_map(|c| (0..=MAX_CORE_IN_CLUSTER).map(move |k| affinity(c, k)))
            .collect();
        let m = Machine::new(0, &all);
        let (started, _) = walk(&m, all.len());
        assert_eq!(
            started.len(),
            all.len() - 1,
            "every core but the boot CPU should have been started"
        );
        assert_eq!(
            *started.last().unwrap(),
            affinity(MAX_CLUSTER, MAX_CORE_IN_CLUSTER),
            "the last cluster the walk can name was never looked at"
        );
    }

    #[test]
    fn two_empty_clusters_in_a_row_is_where_the_walk_gives_up() {
        // The budget's honest limit, named here so it is a known boundary and
        // not a surprise: cores behind two consecutive empty clusters are not
        // found. A machine like that needs a real table (a device tree, as
        // riscv has), not a wider probe — probing every affinity is 4096 calls
        // into firmware on every boot.
        let reachable = Machine::new(0, &[0x0, 0x1, 0x200, 0x201]);
        assert_eq!(walk(&reachable, 63).0, [0x1, 0x200, 0x201]);

        let beyond = Machine::new(0, &[0x0, 0x1, 0x300, 0x301]);
        assert_eq!(
            walk(&beyond, 63).0,
            [0x1],
            "the limit moved: say so here and in EMPTY_CLUSTER_BUDGET's doc"
        );
    }

    #[test]
    fn a_machine_that_fills_the_tables_stops_there_rather_than_walking_every_cluster() {
        // Every affinity this walk can name exists. It must stop at `room`,
        // not at the end of the numbering.
        let all: Vec<u32> = (0..=MAX_CLUSTER)
            .flat_map(|c| (0..=MAX_CORE_IN_CLUSTER).map(move |k| affinity(c, k)))
            .collect();
        let m = Machine::new(0, &all);
        let (started, _) = walk(&m, 63);
        assert_eq!(started.len(), 63);
        assert_eq!(started[0], 0x1);
    }
}
