//! The one object a per-process RM client never uses and cannot do without.
//!
//! `eclipse_rm_ctx_alloc` builds each process's GPU context on a client of its
//! own (`_eclipse_rm_ctx_client`), which is what makes the process nameable in
//! `nvidia-smi`'s process table. The last step of allocating
//! `TURING_COMPUTE_A` / `TURING_A` on that context's channel is the RM's
//! `kgrobjPromoteContext`, and it opens with
//!
//! ```text
//! subdeviceGetByDeviceAndGpu(RES_GET_CLIENT(obj), pDevice, pGpu, &pSubdevice)
//! ```
//!
//! which iterates **the children of that device** looking for a `Subdevice`
//! and answers `NV_ERR_OBJECT_NOT_FOUND` (0x57) when there is none. A client
//! with only `NV01_ROOT` + `NV01_DEVICE_0` therefore builds a channel fine and
//! then fails on its first graphics class -- which is exactly what happened on
//! real hardware: `CTX n INCOMPLETE (chan=0x0 compute=0x57)`, the client fell
//! back to the shared context 0, got a discovery channel with no GPFIFO, and
//! its first EXEC answered `ENODEV` (`VK_ERROR_DEVICE_LOST` out of NVK).
//! Context 0 was the only context that ever worked because step16's shared
//! client has had a subdevice from the start.
//!
//! The C is compiled only for the kernel target, so there is no way to call
//! into it from a host test; this reads the source instead. It reads the
//! **live** lines -- comments are stripped first -- so the paragraph above,
//! which names every symbol this file asserts about, cannot satisfy it.

const SRC: &str = include_str!("../vendor/eclipse_rm_init.c");

/// `SRC` with C comments blanked out. Line structure is preserved so a
/// function still ends where it ends. String literals are not parsed: no
/// comment opener appears inside one in this file, and a test that fails
/// loudly if that ever changes is not worth more than this note.
fn live_source() -> String {
    let mut out = String::with_capacity(SRC.len());
    let mut chars = SRC.chars().peekable();
    let mut in_block = false;
    let mut in_line = false;
    while let Some(c) = chars.next() {
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push('\n');
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('*') => {
                    chars.next();
                    in_block = true;
                    continue;
                }
                Some('/') => {
                    chars.next();
                    in_line = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// The live body of one C function of this file, from its signature line to
/// the closing brace in column 0. Per function, not a search of the whole
/// file: several functions here allocate a subdevice and only one of them is
/// the per-process client.
fn live_function(name: &str) -> String {
    let src = live_source();
    let head = format!("static NV_STATUS {name}(");
    let lines: Vec<&str> = src.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with(&head))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "expected exactly one `{name}` in eclipse_rm_init.c"
    );
    let start = starts[0];
    let end = lines[start..]
        .iter()
        .position(|l| *l == "}")
        .map(|p| start + p)
        .unwrap_or_else(|| panic!("`{}` has no closing brace in column 0", name));
    lines[start..=end].join("\n")
}

/// Whitespace runs collapsed, so an assertion about a call does not also
/// assert about how rustfmt's C cousin happened to wrap it.
fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn a_per_process_client_carries_a_subdevice_under_its_own_device() {
    let body = live_function("_eclipse_rm_ctx_client");
    assert!(
        body.contains("NV20_SUBDEVICE_0"),
        "the per-process client has no subdevice, so every graphics class on \
         its channels answers NV_ERR_OBJECT_NOT_FOUND in kgrobjPromoteContext"
    );
    assert!(
        flat(&body)
            .contains("AllocWithHandle(pRmApi, hClient, hDevice, hSubdevice, NV20_SUBDEVICE_0"),
        "the subdevice must hang off THIS client's own device: \
         subdeviceGetByDeviceAndGpu iterates that device's children, so a \
         subdevice parented anywhere else is not found either"
    );
}

#[test]
fn the_subdevice_is_built_before_the_client_is_handed_out() {
    let body = flat(&live_function("_eclipse_rm_ctx_client"));
    let subdev = body
        .find("NV20_SUBDEVICE_0")
        .expect("no subdevice (the other test says why that matters)");
    let handed_out = body
        .find("*phClient = hClient;")
        .expect("the client is never handed out");
    assert!(
        subdev < handed_out,
        "a client handed out before its subdevice exists is a client whose \
         first graphics class can still lose the race"
    );
}
