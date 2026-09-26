//! What the three `switch.S` files take for granted, written down and checked.
//!
//! The context switch is assembly, one file per architecture, and each of them
//! reaches into `ContextData` by **hardcoded byte offset** and performs the
//! page-base-register dance by hand. Nothing tied those numbers to the struct,
//! and nothing said what the dance has to consist of -- so the three drifted,
//! and one of them was wrong for as long as it has existed:
//!
//! * x86_64 writes `cr3`, which on that architecture *is* a flush of every
//!   non-global TLB entry;
//! * riscv64 writes `satp` and follows it with `sfence.vma x0, x0`, which
//!   invalidates the whole of this hart's translations;
//! * aarch64 wrote `ttbr0_el1` and then `tlbi vaae1is, x10` with
//!   `x10 = ttbr0 >> 12` -- the **physical** page number of the root table
//!   handed to an instruction that takes a **virtual** address. It invalidated
//!   one arbitrary page and left every translation of the outgoing address
//!   space live, and nothing else would have removed them: the kernel hands
//!   out no ASIDs, and aarch64 user leaves are not marked non-global, so the
//!   TLB cannot tell one `TTBR0` from the next.
//!
//! These are host tests and read the `.S` files as text, because that is the
//! only way to check assembly from a machine that is not the target. They are
//! a contract, not a disassembler: each one names the instruction the switch
//! must contain and why, so a rewrite that still honours it passes and one
//! that quietly drops the invalidation does not.

/// Where each architecture's `ContextData` keeps its page-base register, and
/// how big the struct is -- the two numbers its `switch.S` spells out.
struct Layout {
    arch: &'static str,
    source: &'static str,
    /// Byte offset of the page-base register field.
    pgbr_offset: usize,
    /// `size_of::<ContextData>()`.
    size: usize,
}

const LAYOUTS: [Layout; 3] = [
    Layout {
        // `{cr3, r15, r14, r13, r12, rbp, rbx, rip}`, and the switch builds it
        // by pushing in the reverse order, so the offsets are the push order
        // read backwards. `[rsp + 0x38]` is `rip` and `[rsp]` is `cr3`: the
        // two slots the last-instant dead-frame check reads.
        arch: "x86_64",
        source: include_str!("arch/x86_64/switch.S"),
        pgbr_offset: 0,
        size: 64,
    },
    Layout {
        // `{ra, sp, s[12], satp}`.
        arch: "riscv64",
        source: include_str!("arch/riscv64/switch.S"),
        pgbr_offset: 112,
        size: 120,
    },
    Layout {
        // `{s[11], lr, sp, ttbr0}`. The save side walks backwards from the end
        // of the struct, which is why the size appears in the file at all.
        arch: "aarch64",
        source: include_str!("arch/aarch64/switch.S"),
        pgbr_offset: 104,
        size: 112,
    },
];

/// Everything outside a `//` comment, which is where the `.S` files keep their
/// prose -- including, now, the description of the bug this module exists for.
fn instructions(source: &str) -> alloc::string::String {
    use alloc::string::String;
    let mut out = String::new();
    for line in source.lines() {
        let code = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        let code = match code.find('#') {
            // `#112` is an immediate, `# Context switch` is a comment.
            Some(i) if !code[i + 1..].starts_with(|c: char| c.is_ascii_digit()) => &code[..i],
            _ => code,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    /// How many slots the save side pushes onto the frame.
    fn n_pushes(lines: &[&str]) -> usize {
        lines.iter().filter(|s| s.starts_with("push ")).count()
    }

    #[test]
    fn every_switch_writes_the_page_base_register() {
        // Without this the restored context runs on whatever address space the
        // previous one left loaded.
        for l in LAYOUTS.iter() {
            let code = instructions(l.source);
            let writes = match l.arch {
                "x86_64" => code.contains("mov cr3,"),
                "riscv64" => code.contains("csrw satp"),
                _ => code.contains("msr     ttbr0_el1") || code.contains("msr ttbr0_el1"),
            };
            assert!(writes, "{} never loads the page-base register", l.arch);
        }
    }

    #[test]
    fn every_switch_invalidates_the_whole_address_space_it_left() {
        // The entries that go stale are this CPU's own, and nothing else will
        // remove them: there are no ASIDs, and aarch64's user leaves are not
        // even marked non-global. Anything narrower than "all of it" leaves
        // the outgoing process's translations answering for the incoming one.
        for l in LAYOUTS.iter() {
            let code = instructions(l.source);
            let invalidates = match l.arch {
                // Writing CR3 is itself the flush.
                "x86_64" => code.contains("mov cr3,"),
                "riscv64" => code.contains("sfence.vma x0, x0"),
                // `vmalle1`: every EL1 translation of this PE. NOT
                // `tlbi vaae1is, <reg>`, which takes a virtual address and was
                // being handed the root table's physical page number.
                _ => code.contains("tlbi    vmalle1") || code.contains("tlbi vmalle1"),
            };
            assert!(
                invalidates,
                "{} switches address space without invalidating it",
                l.arch
            );
        }
    }

    #[test]
    fn no_switch_invalidates_by_an_address_it_never_computed() {
        // The shape of the bug, said as a rule: `tlbi va...` takes a virtual
        // address, and the only register in reach at that point holds the root
        // table's physical address. There is no VA to invalidate by here, so
        // there must be no invalidation by VA.
        for l in LAYOUTS.iter() {
            let code = instructions(l.source);
            assert!(
                !code.contains("tlbi    vaae1is") && !code.contains("tlbi vaae1is"),
                "{} invalidates by a virtual address it does not have",
                l.arch
            );
        }
    }

    #[test]
    fn each_switch_reaches_the_page_base_register_at_its_real_offset() {
        // The `.S` files index `ContextData` by hardcoded byte offset. A field
        // added or reordered on the Rust side moves them all, and the only
        // symptom would be a context switch restoring the wrong qword.
        for l in LAYOUTS.iter() {
            let code = instructions(l.source);
            let lines: Vec<&str> = code.lines().map(|s| s.trim()).collect();
            let found = match l.arch {
                // Pushed and popped, never indexed: `cr3` is the *last* push,
                // so it sits at offset 0, and the last-instant dead-frame
                // check is the one place that names it by `rsp`.
                "x86_64" => {
                    // `cr3` is read into `r15` and pushed *last*, which is
                    // what puts it at offset 0; the last-instant dead-frame
                    // check is the one place that names that slot by `rsp`.
                    let pushes: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.starts_with("push "))
                        .map(|(i, _)| i)
                        .collect();
                    match lines.iter().position(|s| *s == "mov r15, cr3") {
                        Some(c) => {
                            l.pgbr_offset == 0
                                && code.contains("[rsp], 0")
                                && pushes.last() == Some(&(c + 1))
                        }
                        None => false,
                    }
                }
                // The satp slot is named once beside each `satp` access, and
                // a bare `contains` would match any of the fourteen other
                // slots this file names, so pin it to those two lines.
                "riscv64" => {
                    let save = format!("sd s11, {}(a0)", l.pgbr_offset);
                    let load = format!("ld s11, {}(a1)", l.pgbr_offset);
                    let i = lines.iter().position(|s| *s == "csrr s11, satp");
                    let j = lines.iter().position(|s| s.starts_with("csrw satp"));
                    match (i, j) {
                        (Some(i), Some(j)) => lines[i + 1] == save && lines[j - 1] == load,
                        _ => false,
                    }
                }
                _ => code.contains(&format!("#{}]", l.pgbr_offset)),
            };
            assert!(
                found,
                "{} does not name offset {} of ContextData",
                l.arch, l.pgbr_offset
            );
        }
    }

    #[test]
    fn each_switch_agrees_with_its_struct_on_how_deep_the_frame_is() {
        // aarch64 walks backwards with `stp ..., [x0, #-16]!`, so its very
        // first instruction adds `size_of::<ContextData>()` to the pointer;
        // x86_64 builds the frame out of pushes, so its depth is the push
        // count (plus the `rip` the caller pushed); riscv64 spells out every
        // slot, so its last one is `size - 8`. Get the number wrong and every
        // register is saved one slot out.
        for l in LAYOUTS.iter() {
            let code = instructions(l.source);
            let lines: Vec<&str> = code.lines().map(|s| s.trim()).collect();
            match l.arch {
                "x86_64" => {
                    let slots = n_pushes(&lines) + 1;
                    assert_eq!(slots * 8, l.size, "x86_64 pushes {} slots", slots);
                    // ...and the caller's `rip` is the top one, which is the
                    // other half of the dead-frame check.
                    assert!(
                        code.contains(&format!("[rsp + {:#x}], 0", l.size - 8)),
                        "x86_64 does not check the return slot at {}",
                        l.size - 8
                    );
                }
                "riscv64" => {
                    let top = lines
                        .iter()
                        .filter_map(|s| s.strip_prefix("sd "))
                        .filter_map(|s| s.split(", ").nth(1))
                        .filter_map(|s| s.split('(').next())
                        .filter_map(|s| s.parse::<usize>().ok())
                        .max();
                    assert_eq!(top, Some(l.size - 8), "riscv64 saves past its struct");
                }
                _ => assert!(
                    code.contains(&format!("add     x0, x0, #{}", l.size))
                        || code.contains(&format!("add x0, x0, #{}", l.size)),
                    "aarch64 does not start its save at offset {}",
                    l.size
                ),
            }
        }
    }

    #[test]
    fn the_comment_stripper_keeps_the_immediates_and_drops_the_prose() {
        // The tests above look for instructions, and the `.S` files describe
        // this very bug in their comments -- so a search that read the
        // comments would find `tlbi vaae1is` in the text explaining that it is
        // gone, and pass for ever after.
        let code = instructions(
            "        // tlbi    vaae1is, x10 is what this used to be\n\
             # Context switch\n\
             \x20       add     x0, x0, #112\n",
        );
        assert!(!code.contains("vaae1is"), "a `//` comment survived");
        assert!(!code.contains("Context switch"), "a `#` comment survived");
        assert!(
            code.contains("#112"),
            "an immediate was taken for a comment"
        );
    }
}
