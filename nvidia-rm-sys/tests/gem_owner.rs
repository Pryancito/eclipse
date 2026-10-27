//! Every call about a GEM object must name the client that OWNS it.
//!
//! `nvidia-smi`'s per-process FB column (`_gpuCollectMemInfo`, gpu_rmapi.c)
//! keeps only the memory whose `Memory->pDevice` is the reporting client's
//! device. Allocating every buffer under the shared ladder client therefore
//! billed it to nobody: `labwc` and `Xwayland` appeared in the Processes table
//! and both reported 0 MiB. Duping the object into the process's client does
//! not fix it -- a dup shares the Memory resource, so `pDevice` stays the
//! ladder's -- so the object has to be ALLOCATED there.
//!
//! That makes the owning client part of the object's identity: `hMemory` is a
//! handle in that client and in no other. A free, a CPU map, an FBMEM offset
//! or a bind that resolves it anywhere else does not find it. These tests read
//! the source because the crate links the real RM and there is no double to
//! call these against.

/// The five entry points that take a bare `hMemory` and must resolve it, plus
/// the bind that dups it across clients.
const GEM_LIFETIME: [&str; 6] = [
    "eclipse_rm_gem_alloc",
    "eclipse_rm_gem_free",
    "eclipse_rm_gem_map_cpu",
    "eclipse_rm_gem_map_bar1",
    "eclipse_rm_gem_fbmem_offset",
    "eclipse_rm_vm_bind_map",
];

fn source() -> &'static str {
    include_str!("../vendor/eclipse_rm_init.c")
}

/// The body of a top-level `NV_STATUS <name>(...)` definition, comments
/// stripped -- a rule that is only written down in a comment is not a rule.
fn body(name: &str) -> String {
    let src = source();
    let head = format!("NV_STATUS {}(", name);
    let start = src
        .find(&format!("\n{}", head))
        .unwrap_or_else(|| panic!("no definition of `{}` in eclipse_rm_init.c", name))
        + 1;
    let rest = &src[start..];
    let end = rest[1..]
        .find("\nNV_STATUS ")
        .map(|p| p + 1)
        .unwrap_or(rest.len());
    let mut out = String::new();
    let mut in_block = false;
    for line in rest[..end].lines() {
        let t = line.trim_start();
        if in_block {
            if let Some(rest) = t.find("*/") {
                in_block = false;
                out.push_str(&t[rest + 2..]);
                out.push('\n');
            }
            continue;
        }
        if t.starts_with("/*") {
            in_block = !t.contains("*/");
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(!out.is_empty(), "`{}` has no live body left", name);
    out
}

/// None of them may reach for the shared ladder client any more. This is the
/// whole bug in one assertion: a single survivor is a handle looked up in a
/// client that does not hold it, which is a leak or a silent wrong answer, not
/// an error anyone sees.
#[test]
fn no_part_of_a_gem_object_s_life_goes_through_the_shared_ladder_client() {
    for name in GEM_LIFETIME {
        let b = body(name);
        assert!(
            !b.contains("g_grAllocCache[gpuInstance].hClient"),
            "`{}` still resolves a GEM object through the shared ladder client; \
             the object belongs to the asking process's client now",
            name
        );
    }
}

/// ...and each must take the owning context and resolve it the one way.
#[test]
fn every_entry_point_takes_the_owning_context_and_resolves_it() {
    for name in GEM_LIFETIME {
        let b = body(name);
        assert!(
            b.contains("_eclipse_rm_ctx_handles("),
            "`{}` must resolve its client through `_eclipse_rm_ctx_handles`, \
             which is the one place that knows ctx 0 means the shared pair",
            name
        );
    }
    assert!(
        body("eclipse_rm_gem_alloc").contains("NvU32 ctxIdx"),
        "the allocation is what fixes the owner; without ctxIdx there is nothing to fix it to"
    );
    assert!(
        body("eclipse_rm_vm_bind_map").contains("ownerCtxIdx"),
        "a bind has TWO contexts -- the one being bound into and the one that owns \
         the object -- and they differ for an imported buffer"
    );
}

/// The dup exists for a buffer that crosses processes. It has to source from
/// the owner; sourcing from the ladder would dup a handle nobody holds.
#[test]
fn the_cross_process_dup_sources_from_the_owner() {
    let b = body("eclipse_rm_vm_bind_map");
    assert!(
        b.contains("hCtxClient != hOwnerClient"),
        "the dup must be skipped exactly when the binder already owns the object"
    );
    assert!(
        b.contains("serverGetClientUnderLock(&g_resServ, hOwnerClient, &pGemClient)"),
        "the PTE-kind lookup reads the memdesc through the owner's handle; \
         looking it up elsewhere finds nothing and silently drops the tile kind, \
         which is a corrupted surface rather than an error"
    );
}
