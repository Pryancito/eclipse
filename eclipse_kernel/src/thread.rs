//! Thread management for Eclipse OS.
//!
//! A Thread represents an execution context within a Process.

use alloc::sync::Arc;
use spin::Mutex;
use crate::process::{ProcessId, Context, ProcessState, RTParams, Sigaltstack, NO_CPU, Proc};

pub type ThreadId = u32;

pub struct Thread {
    pub id: ThreadId,
    pub proc: Arc<Proc>,               // Parent process
    pub state: ProcessState,
    pub context: Context,
    pub stack_base: u64,
    pub stack_size: usize,
    pub kernel_stack_top: u64,
    pub kernel_stack: Option<alloc::vec::Vec<u8>>,
    
    // Scheduling parameters
    pub priority: u8,
    pub vruntime: u64,
    pub weight: u64,
    pub time_slice: u32,
    pub rt_params: Option<RTParams>,
    
    // Execution state
    pub current_cpu: u32,
    pub last_cpu: u32,
    pub cpu_affinity: Option<u32>,
    pub wake_tick: u64,
    
    // Signal state (per-thread)
    pub pending_signals: u64,
    pub signal_mask: u64,
    pub sigaltstack: Sigaltstack,
    
    // Thread-local storage
    pub fs_base: u64,
    pub gs_base: u64,
    
    // Linux/POSIX specific
    pub clear_child_tid: u64,
    pub set_child_tid: u64,
    
    // Statistics
    pub cpu_ticks: u64,
}

impl Thread {
    pub fn new(id: ThreadId, proc: Arc<Proc>) -> Self {
        Self {
            id,
            proc,
            state: ProcessState::Blocked,
            context: Context::new(),
            stack_base: 0,
            stack_size: 0,
            kernel_stack_top: 0,
            kernel_stack: None,
            priority: 5,
            vruntime: 0,
            weight: 1024,
            time_slice: 10,
            rt_params: None,
            current_cpu: NO_CPU,
            last_cpu: NO_CPU,
            cpu_affinity: None,
            wake_tick: 0,
            pending_signals: 0,
            signal_mask: 0,
            sigaltstack: Sigaltstack::new(),
            fs_base: 0,
            gs_base: 0,
            clear_child_tid: 0,
            set_child_tid: 0,
            cpu_ticks: 0,
        }
    }
}
