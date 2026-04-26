//! Futex (fast userspace mutex) — cola de espera en el kernel para `FUTEX_WAIT` / `FUTEX_WAKE`.
//! Basado en el comportamiento Linux x86-64 usado por musl/pthreads.

use alloc::vec::Vec;
use spin::Mutex;

use crate::process::{self, ProcessState};
use crate::scheduler::{add_sleep, enqueue_process, yield_cpu};

use super::{copy_from_user, is_user_pointer, linux_abi_error};

#[inline]
fn read_user_u32_volatile(addr: u64) -> Option<u32> {
    if !is_user_pointer(addr, 4) {
        return None;
    }
    // Recuperación ante #PF (misma filosofía que copy_from_user).
    if unsafe { !crate::interrupts::set_recovery_point() } {
        let v = unsafe { core::ptr::read_volatile(addr as *const u32) };
        unsafe { crate::interrupts::clear_recovery_point() };
        Some(v)
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        None
    }
}

#[inline]
fn write_user_u32_volatile(addr: u64, v: u32) -> bool {
    if !is_user_pointer(addr, 4) {
        return false;
    }
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) };
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

struct FutexWaiter {
    addr: u64,
    pid: process::ProcessId,
    bitset: u32,
}

static FUTEX_WAITERS: Mutex<Vec<FutexWaiter>> = Mutex::new(Vec::new());

/// Despierta todos los procesos en cola para `addr` (p. ej. `set_tid_address` / `CLONE_CHILD_CLEARTID`).
pub fn futex_wake_all_atomic(addr: u64) {
    let mut woken = 0u32;
    let mut i = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    while i < waiters.len() {
        if waiters[i].addr == addr {
            let wpid = waiters[i].pid;
            waiters.remove(i);
            enqueue_process(wpid);
            woken = woken.saturating_add(1);
        } else {
            i += 1;
        }
    }
}

/// `sys_futex` — op en bits bajos; `FUTEX_PRIVATE_FLAG` (128) y reloj se ignoran salvo wait bitset.
pub fn sys_futex(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u32) -> u64 {
    let pid = process::current_process_id().unwrap_or(0);
    let cmd = op & 0x7F;

    match cmd {
        0 | 9 => futex_wait(pid, uaddr, op, val, timeout_ptr, val3, cmd == 9),
        1 | 10 => futex_wake(uaddr, val, uaddr2, val3, cmd == 10),
        3 | 4 => futex_requeue(uaddr, val, timeout_ptr, uaddr2, val3, cmd == 4),
        5 => futex_wake_op(uaddr, val, timeout_ptr, uaddr2, val3),
        6..=8 | 11 | 12 => linux_abi_error(38), // PI / requeue-PI — ENOSYS
        _ => linux_abi_error(38),
    }
}

fn futex_wait(
    pid: process::ProcessId,
    uaddr: u64,
    _op: u64,
    val: u64,
    timeout_ptr: u64,
    val3: u32,
    is_bitset: bool,
) -> u64 {
    let bitset: u32 = if is_bitset { val3 } else { 0xFFFF_FFFF };

    if !is_user_pointer(uaddr, 4) { return linux_abi_error(14); }

    {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        waiters.push(FutexWaiter {
            addr: uaddr,
            pid,
            bitset,
        });
    }

    let Some(current) = read_user_u32_volatile(uaddr) else {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return linux_abi_error(14);
    };
    if current != val as u32 {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return linux_abi_error(11); // EAGAIN
    }

    let timeout_ms = if timeout_ptr != 0 && is_user_pointer(timeout_ptr, 16) {
        let mut b = [0u8; 16];
        if !copy_from_user(timeout_ptr, &mut b) {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return linux_abi_error(14);
        }
        let sec = i64::from_le_bytes(b[0..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
        if sec < 0 || nsec < 0 {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return linux_abi_error(22);
        }
        Some((sec as u64).saturating_mul(1000).saturating_add((nsec as u64) / 1_000_000))
    } else {
        None
    };

    let start_ticks = crate::interrupts::ticks();

    let cas_ok = process::compare_and_set_process_state(
        pid,
        ProcessState::Running,
        ProcessState::Blocked,
    )
    .ok()
    .unwrap_or(false);

    if !cas_ok {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return 0;
    }

    if let Some(ms) = timeout_ms {
        let wake = start_ticks.saturating_add(ms);
        add_sleep(pid, wake);
    }

    loop {
        if let Some(p) = process::get_process(pid) {
            if p.state != ProcessState::Blocked {
                let mut waiters = FUTEX_WAITERS.lock();
                waiters.retain(|w| w.pid != pid);
                return 0;
            }
        } else {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return 0;
        }

        if let Some(ms) = timeout_ms {
            if crate::interrupts::ticks().saturating_sub(start_ticks) >= ms {
                let mut waiters = FUTEX_WAITERS.lock();
                waiters.retain(|w| w.pid != pid);
                let _ = process::compare_and_set_process_state(
                    pid,
                    ProcessState::Blocked,
                    ProcessState::Running,
                );
                return linux_abi_error(110); // ETIMEDOUT
            }
        }

        yield_cpu();
    }
}

fn futex_wake(uaddr: u64, max: u64, _uaddr2: u64, val3: u32, is_bitset: bool) -> u64 {
    let bitset: u32 = if is_bitset { val3 } else { 0xFFFF_FFFF };
    let mut woken: u64 = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    let mut i = 0;
    while i < waiters.len() && woken < max {
        if waiters[i].addr == uaddr && (waiters[i].bitset & bitset) != 0 {
            let wpid = waiters[i].pid;
            waiters.remove(i);
            enqueue_process(wpid);
            woken += 1;
        } else {
            i += 1;
        }
    }
    woken
}

fn futex_requeue(uaddr: u64, wake_n: u64, max_requeue: u64, uaddr2: u64, val3: u32, is_cmp: bool) -> u64 {
    if is_cmp {
        if !is_user_pointer(uaddr, 4) {
            return linux_abi_error(14);
        }
        let Some(current) = read_user_u32_volatile(uaddr) else { return linux_abi_error(14); };
        if current != val3 {
            return linux_abi_error(11);
        }
    }
    let mut woken: u64 = 0;
    let mut requeued: u64 = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    let mut i = 0;
    while i < waiters.len() {
        if waiters[i].addr == uaddr {
            if woken < wake_n {
                let wpid = waiters[i].pid;
                waiters.remove(i);
                enqueue_process(wpid);
                woken += 1;
            } else if requeued < max_requeue {
                waiters[i].addr = uaddr2;
                requeued += 1;
                i += 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    woken + requeued
}

fn futex_wake_op(uaddr: u64, wake1: u64, val2: u64, uaddr2: u64, val3: u32) -> u64 {
    // val2 = número a despertar en uaddr2 si la comparación con *uaddr2 (tras op) cumple
    let old_val2 = if is_user_pointer(uaddr2, 4) {
        let op_num = (val3 >> 28) & 0xF;
        let cmp = (val3 >> 24) & 0xF;
        let oparg = ((val3 >> 12) & 0xFFF) as u32;
        let cmparg = (val3 & 0xFFF) as u32;
        let effective_oparg = if op_num & 8 != 0 {
            1u32 << (oparg & 31)
        } else {
            oparg
        };
        let Some(old) = read_user_u32_volatile(uaddr2) else { return linux_abi_error(14); };
        let new_val = match op_num & 7 {
            0 => effective_oparg,
            1 => old.wrapping_add(effective_oparg),
            2 => old | effective_oparg,
            3 => old & !effective_oparg,
            4 => old ^ effective_oparg,
            _ => old,
        };
        if !write_user_u32_volatile(uaddr2, new_val) { return linux_abi_error(14); }
        let cmp_ok = match cmp {
            0 => old == cmparg,
            1 => old != cmparg,
            2 => old < cmparg,
            3 => old <= cmparg,
            4 => old > cmparg,
            5 => old >= cmparg,
            _ => false,
        };
        Some(cmp_ok)
    } else {
        None
    };
    let do_u2 = old_val2.unwrap_or(false);
    let mut woken: u64 = 0;
    {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() && woken < wake1 {
            if waiters[i].addr == uaddr {
                let wpid = waiters[i].pid;
                waiters.remove(i);
                enqueue_process(wpid);
                woken += 1;
            } else {
                i += 1;
            }
        }
        if do_u2 {
            let mut w2: u64 = 0;
            i = 0;
            while i < waiters.len() && w2 < val2 {
                if waiters[i].addr == uaddr2 {
                    let wpid = waiters[i].pid;
                    waiters.remove(i);
                    enqueue_process(wpid);
                    w2 += 1;
                } else {
                    i += 1;
                }
            }
            woken += w2;
        }
    }
    woken
}
