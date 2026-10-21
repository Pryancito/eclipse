//! `CLONE_PARENT`: a child that is its creator's sibling.
//!
//! `clone` refused it with `EINVAL`. Linux gives the child the caller's own
//! parent (`p->real_parent = current->real_parent`) and the caller's own
//! exit signal (`p->exit_signal = current->group_leader->exit_signal`), so
//! the caller cannot `wait` for it (`ECHILD`) and the one that does is told
//! of its death as it would be told of the caller's (`clone-survey`,
//! measured on Linux 6.18 with glibc and musl).

use super::*;

/// Hand `child`, which `caller` has just forked and not yet started, to
/// `caller`'s parent, with the signal `caller`'s own death sends.
///
/// The parent is the one `caller` has now, which after an adoption is the
/// adopter. One that is dying, or gone, is taken for what an orphan of it
/// gets ([`reaper_for`]), as Linux's `forget_original_parent` would do with
/// the child a moment later; with nobody at all to take it, the child stays
/// `caller`'s.
pub fn give_to_callers_parent(caller: &Arc<Process>, child: &Arc<Process>) {
    let callers = caller.linux();
    let Some(parent) = callers
        .parent()
        .and_then(|p| reaper_for(&p))
        .or_else(live_init)
    else {
        return;
    };
    child.linux().set_exit_signal(callers.exit_signal());
    callers.inner.lock().children.remove(&child.id());
    child.linux().set_parent(&parent);
    // As `fork_from` files a child: not under a parent that died meanwhile,
    // where no `wait` would look. The child's own death still finds a
    // reaper through its `parent`.
    let mut parents = parent.linux().inner.lock();
    if !matches!(parent.status(), Status::Exited(_)) {
        parents.children.insert(child.id(), child.clone());
    }
}

#[cfg(test)]
mod tests;
