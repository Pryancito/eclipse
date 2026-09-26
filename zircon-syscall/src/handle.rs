use {super::*, core::convert::TryFrom};

/// The rights a duplicated or replaced handle ends up with.
///
/// `ZX_RIGHT_SAME_RIGHTS` is a sentinel (bit 31), not a right: no object ever
/// has it, so a handle that ends up holding it holds nothing else either and
/// every later use of it is `ACCESS_DENIED`. Which is what
/// `zx_handle_replace(h, ZX_RIGHT_SAME_RIGHTS)` produced -- `duplicate` answered
/// the sentinel with the source handle's rights and `replace` answered it with
/// the sentinel itself. One decision, written twice, drifted.
///
/// Zircon spells the sentinel test as an exact comparison and lets anything
/// mixed with it fall through to the subset check below, where it is refused
/// because no source handle has bit 31. Testing it with `contains` -- as both
/// callers did -- quietly accepted `SAME_RIGHTS | READ`.
fn replacement_rights(handle_rights: Rights, requested: Rights) -> ZxResult<Rights> {
    if requested == Rights::SAME_RIGHTS {
        return Ok(handle_rights);
    }
    if (handle_rights & requested).bits() != requested.bits() {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(requested)
}

impl Syscall<'_> {
    /// Creates a duplicate of handle.
    ///
    /// Referring to the same underlying object, with new access rights rights.
    pub fn sys_handle_duplicate(
        &self,
        handle_value: HandleValue,
        rights: u32,
        mut new_handle_value: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let rights = Rights::try_from(rights)?;
        info!(
            "handle.dup: handle={:#x?}, rights={:?}",
            handle_value, rights
        );
        let proc = self.thread.proc();
        let new_value = proc.dup_handle_operating_rights(handle_value, |handle_rights| {
            if !handle_rights.contains(Rights::DUPLICATE) {
                return Err(ZxError::ACCESS_DENIED);
            }
            replacement_rights(handle_rights, rights)
        })?;
        install_handle_value(proc, new_value, &mut new_handle_value)
    }

    /// Close a handle and reclaim the underlying object if no other handles to it exist.
    pub fn sys_handle_close(&self, handle: HandleValue) -> ZxResult {
        info!("handle.close: handle={:?}", handle);
        if handle == INVALID_HANDLE {
            return Ok(());
        }
        let proc = self.thread.proc();
        proc.remove_handle(handle)?;
        Ok(())
    }

    /// Close a number of handles.
    pub fn sys_handle_close_many(
        &self,
        handles: UserInPtr<HandleValue>,
        num_handles: usize,
    ) -> ZxResult {
        info!(
            "handle.close_many: handles=({:#x?}; {:#x?})",
            handles, num_handles,
        );
        let proc = self.thread.proc();
        for handle in handles.as_slice(num_handles)? {
            if *handle != INVALID_HANDLE {
                proc.remove_handle(*handle)?;
            }
        }
        Ok(())
    }

    /// Creates a replacement for handle.
    ///
    /// Referring to the same underlying object, with new access rights rights.
    pub fn sys_handle_replace(
        &self,
        handle_value: HandleValue,
        rights: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let rights = Rights::try_from(rights)?;
        info!(
            "handle.replace: handle={:#x?}, rights={:?}",
            handle_value, rights
        );
        let proc = self.thread.proc();
        // The old handle goes away below, so a bad `out` used to leave the
        // caller with no handle to the object at all.
        check_out(proc, &out)?;
        let new_value = proc.dup_handle_operating_rights(handle_value, |handle_rights| {
            replacement_rights(handle_rights, rights)
        })?;
        proc.remove_handle(handle_value)?;
        install_handle_value(proc, new_value, &mut out)
    }
}

#[cfg(test)]
mod replacement_rights_tests {
    use super::*;

    #[test]
    fn the_same_rights_sentinel_gives_back_the_handles_own_rights() {
        // `replace` used to answer this with the sentinel itself, so the
        // replacement handle held bit 31 and nothing else: no TRANSFER, no READ,
        // no WRITE. The handle came back looking valid and then failed
        // ACCESS_DENIED on first use, with nothing saying why.
        let have = Rights::READ | Rights::WRITE | Rights::TRANSFER;
        let got = replacement_rights(have, Rights::SAME_RIGHTS).expect("accepted");
        assert_eq!(got, have);
        assert!(
            !got.contains(Rights::SAME_RIGHTS),
            "the sentinel must never end up on a handle"
        );
    }

    #[test]
    fn a_subset_is_granted_and_anything_more_is_refused() {
        let have = Rights::READ | Rights::WRITE;
        assert_eq!(replacement_rights(have, Rights::READ), Ok(Rights::READ));
        assert_eq!(replacement_rights(have, have), Ok(have));
        assert_eq!(
            replacement_rights(have, Rights::empty()),
            Ok(Rights::empty())
        );
        // Asking for a right the source does not have is INVALID_ARGS, never a
        // silently narrowed handle.
        assert_eq!(
            replacement_rights(have, Rights::READ | Rights::EXECUTE),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            replacement_rights(Rights::empty(), Rights::READ),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn the_sentinel_mixed_with_a_real_right_is_refused() {
        // Tested with `contains`, this took the sentinel branch and handed back
        // every right the source had -- an escalation from a request that Zircon
        // rejects. No object carries bit 31, so the subset check refuses it.
        let have = Rights::READ | Rights::WRITE;
        assert_eq!(
            replacement_rights(have, Rights::SAME_RIGHTS | Rights::READ),
            Err(ZxError::INVALID_ARGS)
        );
    }
}
