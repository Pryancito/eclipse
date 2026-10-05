## 修改日志

#### executor 修改（3.8-3.9 ZYR）
* 去除 executor 中的 trapframe，直接使用 context 完成新 executor 的创建
    * 不需要修改 sstatus, tp / cs, ss, rflags 等, 通过中断返回和通过 switch 返回能力是等价的。
* 去掉 executor::new() 的 cpuid 参数
    * 事实上具体使用哪个 cpu 并不是靠 new 的时候控制的，需要其他机制

#### Merge `vendor/preemptive-scheduler` → this tree (2026-10)
* Compared the lowercase pristine/upstream-shaped copy with this Eclipse fork.
* This tree already supersedes it (affinity, steal, stack guards, null-exec
  bounce, `tlbi vmalle1`, `lock::current_cpu_id`, diag, switch_contract, …).
* Taken from the lowercase tree (safe hygiene only):
  * `.gitignore` (`/target`, `Cargo.lock`)
  * `rust-toolchain` pin → `nightly-2026-09-01` (matches workspace)
  * `unsafe extern "C"` / `#[unsafe(no_mangle)]` for the 2026 nightly
* **Not** taken (would regress): `raw-cpuid`, `push 0; jmp run_executor`,
  aarch64 `tlbi vaae1is`, IRQ-on-before-wfi, 128 KiB stacks, 8-core fixed
  runtime, simplified `take_notified`.
* Removed `vendor/preemptive-scheduler`; CI/scripts test this path only.

#### Scheduler evolve in place (2026-10)
* Steal ranks victims by `ready_num_for(thief)` (affinity-aware), not raw `ready_num`.
* Affinity kick prefers least-loaded allowed CPU (sleeping still first).
* Weak reclaim on every strong downgrade (stacks return to pool sooner); soft cap 16/CPU + stats.
* New counters: `sched_steal_stats`, `sched_weak_stats`, `stack_pool_stats` → `/proc/perf/kernel`.
* Rebalance pull: every 32 polls, a CPU with exactly one runnable task may steal
  from a peer that has at least `1 + REBALANCE_MARGIN` (3) tasks it can run.
  Idle steal unchanged. No waker-page migration.
* `has_ready` only inspects priority 4 (the only queue inserts use).
* Spawn no longer `force`s a runtime for a CPU that has not entered
  `run_until_idle`; the task parks on a live CPU and is stolen later.
* Steal tie-break by distance from thief (equal loads no longer all hit CPU 0).
* Generator coalesces affinity kicks: one `kick_for_affinity` per distinct mask per pass.
* `stack_high_water()` sampled at `sched_yield` → `sched stack:` line in `/proc/perf/kernel`.
* Test: halt protocol end-to-end against a remote wake in each of its three windows.
* `affinity_changed(mask)`: `Thread::set_affinity` kicks an allowed CPU when the
  mask narrows/moves, via `kernel_hal::thread::affinity_changed`. Widening is a no-op.
* Weak-idle reviewed, left as is: each `sched_yield` with a weak outstanding
  resumes a preempted poll; weaks die after their poll. Not a spin.
* Stress tests (`src/stress_tests.rs`, host threads, seeded xorshift): waker page
  (no lost wake, no borrowed slot handed out), waker ref from many threads,
  task collection owner + thieves + reaper (each task out at most once at a time,
  pinned tasks never on the wrong CPU, `task_num` follows retirements), steal
  across 4 collections, `ResumeClaim` with 16 CPUs (one holder max), resched
  burst (one IPI per CPU), picker fuzz 200k iters (never names a CPU outside its
  inputs). Tests that count resched requests clear `NEED_RESCHED` first: a bit
  left pending by an earlier test coalesces the request and reads as 0.
