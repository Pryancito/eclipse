//! Classic BPF seccomp filter (Documentation/userspace-api/seccomp_filter.rst).
//!
//! Enough of the VM for libseccomp / bubblewrap: load `seccomp_data`, compare
//! syscall number/arch/args, return ALLOW / ERRNO / KILL. An unparseable
//! program is treated as allow-all so a sandbox still starts; `mode()` stays
//! Filter so `prctl(PR_GET_SECCOMP)` reports 2.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::error::{LxError, LxResult};

/// `SECCOMP_SET_MODE_STRICT`
pub const MODE_STRICT: u32 = 1;
/// `SECCOMP_SET_MODE_FILTER`
pub const MODE_FILTER: u32 = 2;

const SECCOMP_RET_ACTION: u32 = 0xffff_0000;
const SECCOMP_RET_KILL: u32 = 0x0000_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

const BPF_CLASS: u16 = 0x07;
const BPF_LD: u16 = 0x00;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;

const BPF_SIZE: u16 = 0x18;
const BPF_W: u16 = 0x00;

const BPF_MODE: u16 = 0xe0;
const BPF_ABS: u16 = 0x20;
#[allow(dead_code)]
const BPF_K: u16 = 0x00;

const BPF_OP: u16 = 0xf0;
const BPF_ADD: u16 = 0x00;
const BPF_SUB: u16 = 0x10;
const BPF_AND: u16 = 0x50;
const BPF_OR: u16 = 0x40;
const BPF_LSH: u16 = 0x60;
const BPF_RSH: u16 = 0x70;
const BPF_NEG: u16 = 0x80;
const BPF_XOR: u16 = 0xa0;

const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;

const BPF_SRC: u16 = 0x08;
const BPF_K_SRC: u16 = 0x00;

const BPF_RVAL: u16 = 0x18;
const BPF_K_RET: u16 = 0x00;
const BPF_A_RET: u16 = 0x10;

/// One classic `sock_filter` instruction.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// Installed filter.
pub struct SeccompFilter {
    mode: u32,
    insns: Vec<SockFilter>,
}

impl SeccompFilter {
    pub fn strict() -> Arc<Self> {
        Arc::new(Self {
            mode: MODE_STRICT,
            insns: Vec::new(),
        })
    }

    pub fn from_classic(insns: Vec<SockFilter>) -> Arc<Self> {
        Arc::new(Self {
            mode: MODE_FILTER,
            insns,
        })
    }

    pub fn allow_all() -> Arc<Self> {
        Arc::new(Self {
            mode: MODE_FILTER,
            insns: Vec::new(),
        })
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Evaluate against syscall `nr` / `args`. `Ok(())` = allow.
    pub fn check(&self, nr: u32, args: &[usize; 6]) -> LxResult<()> {
        if self.mode == MODE_STRICT {
            // read, write, _exit, sigreturn — everything else dies.
            match nr {
                0 | 1 | 15 | 60 | 231 => return Ok(()),
                _ => return Err(LxError::EPERM),
            }
        }
        if self.insns.is_empty() {
            return Ok(());
        }
        match self.eval(nr, args) {
            SECCOMP_RET_ALLOW => Ok(()),
            SECCOMP_RET_ERRNO => Err(LxError::EPERM),
            SECCOMP_RET_TRAP | SECCOMP_RET_KILL | SECCOMP_RET_KILL_PROCESS => Err(LxError::EPERM),
            action if action & SECCOMP_RET_ACTION == SECCOMP_RET_ERRNO => {
                let errno = (action & 0xffff) as i32;
                Err(map_errno(errno))
            }
            _ => Ok(()),
        }
    }

    fn eval(&self, nr: u32, args: &[usize; 6]) -> u32 {
        let data = seccomp_data(nr, args);
        let mut a: u32 = 0;
        let mut pc: usize = 0;
        let mut steps = 0u32;
        while pc < self.insns.len() && steps < 4096 {
            steps += 1;
            let insn = self.insns[pc];
            let class = insn.code & BPF_CLASS;
            match class {
                BPF_LD => {
                    if insn.code & BPF_SIZE == BPF_W && insn.code & BPF_MODE == BPF_ABS {
                        let off = insn.k as usize;
                        if off + 4 > data.len() {
                            return SECCOMP_RET_KILL;
                        }
                        let bytes: [u8; 4] = [
                            data[off],
                            data[off + 1],
                            data[off + 2],
                            data[off + 3],
                        ];
                        a = u32::from_ne_bytes(bytes);
                    } else if insn.code & BPF_MODE == BPF_K {
                        a = insn.k;
                    } else {
                        return SECCOMP_RET_KILL;
                    }
                    pc += 1;
                }
                BPF_ALU => {
                    let src = if insn.code & BPF_SRC == BPF_K_SRC {
                        insn.k
                    } else {
                        0
                    };
                    match insn.code & BPF_OP {
                        BPF_ADD => a = a.wrapping_add(src),
                        BPF_SUB => a = a.wrapping_sub(src),
                        BPF_AND => a &= src,
                        BPF_OR => a |= src,
                        BPF_XOR => a ^= src,
                        BPF_LSH => a = a.wrapping_shl(src),
                        BPF_RSH => a >>= src,
                        BPF_NEG => a = a.wrapping_neg(),
                        _ => return SECCOMP_RET_KILL,
                    }
                    pc += 1;
                }
                BPF_JMP => {
                    let k = insn.k;
                    let take = match insn.code & BPF_OP {
                        0 => false, // JA handled below
                        BPF_JEQ => a == k,
                        BPF_JGT => a > k,
                        BPF_JGE => a >= k,
                        BPF_JSET => (a & k) != 0,
                        _ => {
                            if insn.code & 0xf0 == 0 {
                                // BPF_JA
                                pc = pc.saturating_add(insn.k as usize).saturating_add(1);
                                continue;
                            }
                            return SECCOMP_RET_KILL;
                        }
                    };
                    if insn.code & BPF_OP == 0 {
                        pc = pc.saturating_add(insn.k as usize).saturating_add(1);
                    } else {
                        let off = if take { insn.jt } else { insn.jf };
                        pc = pc.saturating_add(off as usize).saturating_add(1);
                    }
                }
                BPF_RET => {
                    return if insn.code & BPF_RVAL == BPF_A_RET {
                        a
                    } else if insn.code & BPF_RVAL == BPF_K_RET {
                        insn.k
                    } else {
                        SECCOMP_RET_KILL
                    };
                }
                _ => return SECCOMP_RET_KILL,
            }
        }
        SECCOMP_RET_KILL
    }
}

fn map_errno(e: i32) -> LxError {
    match e {
        1 => LxError::EPERM,
        2 => LxError::ENOENT,
        13 => LxError::EACCES,
        22 => LxError::EINVAL,
        38 => LxError::ENOSYS,
        _ => LxError::EPERM,
    }
}

/// Packed `struct seccomp_data` (little-endian offsets as Linux documents).
fn seccomp_data(nr: u32, args: &[usize; 6]) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0..4].copy_from_slice(&nr.to_ne_bytes());
    // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "x86_64")]
    {
        b[4..8].copy_from_slice(&0xC000_003E_u32.to_ne_bytes());
    }
    #[cfg(target_arch = "aarch64")]
    {
        b[4..8].copy_from_slice(&0xC000_00B7_u32.to_ne_bytes());
    }
    #[cfg(target_arch = "riscv64")]
    {
        b[4..8].copy_from_slice(&0xC000_00F3_u32.to_ne_bytes());
    }
    for (i, arg) in args.iter().enumerate() {
        let off = 16 + i * 8;
        let v = *arg as u64;
        b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
    }
    b
}

/// Load a classic filter from userspace `sock_fprog`.
pub fn load_classic(insns: Vec<SockFilter>) -> LxResult<Arc<SeccompFilter>> {
    if insns.is_empty() || insns.len() > 4096 {
        return Err(LxError::EINVAL);
    }
    Ok(SeccompFilter::from_classic(insns))
}
