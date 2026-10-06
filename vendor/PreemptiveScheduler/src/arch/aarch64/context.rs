#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ContextData {
    // callee saved registers
    pub s: [usize; 11],
    // pc / sp
    pub lr: usize,
    pub sp: usize,
    // pg base register
    pub ttbr0: usize,
    // AAPCS64 preserves the low 64 bits of v8-v15.
    pub d: [u64; 8],
}

impl ContextData {
    pub fn new(lr: usize, sp: usize, ttbr0: usize) -> Self {
        Self {
            s: [0; 11],
            lr,
            sp,
            ttbr0,
            d: [0; 8],
        }
    }
}
