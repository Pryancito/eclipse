//! Sistema de interrupciones del microkernel

use spin::Mutex;

pub struct InterruptStats {
    pub exceptions: u64,
    pub irqs: u64,
    pub timer_ticks: u64,
}

static INTERRUPT_STATS: Mutex<InterruptStats> = Mutex::new(InterruptStats {
    exceptions: 0,
    irqs: 0,
    timer_ticks: 0,
});

pub fn init() {
    // Stub - interrupciones básicas
    // TODO: Implementar IDT completa
}

pub fn get_stats() -> InterruptStats {
    let stats = INTERRUPT_STATS.lock();
    InterruptStats {
        exceptions: stats.exceptions,
        irqs: stats.irqs,
        timer_ticks: stats.timer_ticks,
    }
}
