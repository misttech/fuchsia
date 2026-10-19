// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::thread::AutoExpiringPreemptDisabler;
use crate::object::{HandleValue, ProcessDispatcher, remove_user_handles};
use crate::user_copy::UserInPtr;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_RIGHT_DUPLICATE, ZX_RIGHT_SAME_RIGHTS, zx_handle_t, zx_rights_t,
};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_handle_close
#[syscall]
pub fn sys_handle_close(handle_value: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    // Closing the "never a handle" invalid handle is not an error
    // It's like free(NULL).
    if handle_value.raw_value() == ZX_HANDLE_INVALID {
        return Ok(());
    }
    let up = ProcessDispatcher::get_current();
    let _handle = up.handle_table().remove_handle(&up, handle_value).ok_or(Status::BAD_HANDLE)?;
    Ok(())
}

// zx_status_t zx_handle_close_many
#[syscall]
pub fn sys_handle_close_many(
    handles: UserInPtr<zx_handle_t>,
    num_handles: usize,
) -> Result<(), Status> {
    ltracef!("handles {:p}, num_handles {}\n", handles.as_ptr(), num_handles);

    let up = ProcessDispatcher::get_current();
    remove_user_handles(handles, num_handles, &up)
}

/// Common implementation logic for `zx_handle_duplicate` and `zx_handle_replace` within `up`.
fn handle_dup_replace_process(
    up: &ProcessDispatcher,
    is_replace: bool,
    handle_value: HandleValue,
    mut rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    let table = up.handle_table();
    let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
    ksync::lock!(let mut guard = table.write_lock());
    let mut token_guard = guard.as_mut().as_token_guard_mut();
    let source = table
        .get_handle_entry_locked(&mut token_guard, up, handle_value)
        .ok_or(Status::BAD_HANDLE)?;

    if !is_replace && !source.as_ref().has_rights(ZX_RIGHT_DUPLICATE) {
        return Err(Status::ACCESS_DENIED);
    }

    // When replacing, the source handle is removed from the table (and closed when the returned
    // `HandleOwner` drops) exactly where C++ called `RemoveHandleLocked`: on a rights mismatch and
    // after a successful `Handle::dup`. A `NO_MEMORY` failure leaves the source handle in place.
    if rights == ZX_RIGHT_SAME_RIGHTS {
        rights = source.as_ref().rights();
    } else if (source.as_ref().rights() & rights) != rights {
        if is_replace {
            let _ = source.remove();
        }
        return Err(Status::INVALID_ARGS);
    }

    let handle = source.as_ref().dup(rights)?;

    if is_replace {
        let _ = source.remove();
    }

    *out = table.map_handle_to_value(handle.as_ref());
    table.add_handle_locked_guard(handle, &mut token_guard);
    Ok(())
}

/// Helper routine implementing common logic for `zx_handle_duplicate` and `zx_handle_replace`.
fn handle_dup_replace(
    is_replace: bool,
    handle_value: HandleValue,
    rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    let up = ProcessDispatcher::get_current();
    handle_dup_replace_process(&up, is_replace, handle_value, rights, out)
}

// zx_status_t zx_handle_duplicate
#[syscall]
pub fn sys_handle_duplicate(
    handle_value: HandleValue,
    rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    handle_dup_replace(false, handle_value, rights, out)
}

// zx_status_t zx_handle_replace
#[syscall]
pub fn sys_handle_replace(
    handle_value: HandleValue,
    rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    handle_dup_replace(true, handle_value, rights, out)
}

/// Tests for handle operations syscalls.
#[cfg(ktest)]
#[unittest::suite(name = "handle_ops_rust")]
mod tests {
    use super::{handle_dup_replace_process, sys_handle_close};
    use crate::object::{
        EventPairDispatcher, Handle, HandleValue, JobDispatcher, ProcessDispatcher,
    };
    use unittest::{expect_eq, expect_err, expect_ok};
    use zx_status::Status;
    use zx_types::{ZX_HANDLE_INVALID, ZX_RIGHT_DUPLICATE, ZX_RIGHT_SAME_RIGHTS, ZX_RIGHT_SIGNAL};

    /// Tests closing an invalid handle succeeds.
    #[test]
    fn test_sys_handle_close_invalid() {
        expect_ok!(sys_handle_close(HandleValue::new(ZX_HANDLE_INVALID)));
    }

    /// Tests handle duplication and replacement logic.
    #[test]
    fn test_handle_duplicate_and_replace_logic() {
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(JobDispatcher::get_root_job(), b"test-handle-ops", 0)
                .expect("failed to create process");
        let process = proc_handle.dispatcher();

        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();
        let val0 = process.handle_table().map_handle_to_value_owner(&ho);
        process.handle_table().add_handle(ho);
        expect_eq!(process.handle_table().handle_count(), 1);

        // Test unknown handle value fails with BAD_HANDLE
        let mut bad_val = HandleValue::default();
        expect_err!(
            handle_dup_replace_process(
                process,
                false,
                HandleValue::new(0x1234_5678),
                ZX_RIGHT_SAME_RIGHTS,
                &mut bad_val,
            ),
            Status::BAD_HANDLE
        );
        expect_err!(
            handle_dup_replace_process(
                process,
                true,
                HandleValue::new(0x1234_5678),
                ZX_RIGHT_SAME_RIGHTS,
                &mut bad_val,
            ),
            Status::BAD_HANDLE
        );

        // Test duplicate: same rights
        let mut dup_val = HandleValue::default();
        expect_ok!(handle_dup_replace_process(
            process,
            false,
            HandleValue::new(val0),
            ZX_RIGHT_SAME_RIGHTS,
            &mut dup_val,
        ));
        expect_eq!(process.handle_table().handle_count(), 2);

        // Test duplicate: reduced rights
        let mut dup_val2 = HandleValue::default();
        let reduced_rights = rights & !ZX_RIGHT_SIGNAL;
        expect_ok!(handle_dup_replace_process(
            process,
            false,
            HandleValue::new(val0),
            reduced_rights,
            &mut dup_val2,
        ));
        expect_eq!(process.handle_table().handle_count(), 3);

        // Test duplicate: expanded rights fails with INVALID_ARGS
        let mut fail_val = HandleValue::default();
        expect_err!(
            handle_dup_replace_process(process, false, dup_val2, rights, &mut fail_val,),
            Status::INVALID_ARGS
        );
        expect_eq!(process.handle_table().handle_count(), 3);

        // Test replace: source is removed and replaced
        let mut replace_val = HandleValue::default();
        expect_ok!(handle_dup_replace_process(
            process,
            true,
            dup_val2,
            ZX_RIGHT_SAME_RIGHTS,
            &mut replace_val,
        ));
        // Count should remain 3 because 1 was removed and 1 was added
        expect_eq!(process.handle_table().handle_count(), 3);

        // Test replace with invalid rights: source is invalidated and closed
        let mut fail_replace_val = HandleValue::default();
        expect_err!(
            handle_dup_replace_process(
                process,
                true,
                replace_val,
                rights | 0x8000_0000,
                &mut fail_replace_val,
            ),
            Status::INVALID_ARGS
        );
        // Source was invalidated, so count is now 2
        expect_eq!(process.handle_table().handle_count(), 2);

        // Close remaining handles
        let _ = process.remove_handle(HandleValue::new(val0));
        let _ = process.remove_handle(dup_val);
        expect_eq!(process.handle_table().handle_count(), 0);

        // ep1 peer should now be closed
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );

        // Test handle without ZX_RIGHT_DUPLICATE: fails on duplicate, succeeds on replace.
        let (ep_no_dup0, ep_no_dup1, rights_no_dup) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let no_dup_rights = rights_no_dup & !ZX_RIGHT_DUPLICATE;
        let ho_no_dup = Handle::make_from_kernel_handle(ep_no_dup0.cast(), no_dup_rights).unwrap();
        let val_no_dup = process.handle_table().map_handle_to_value_owner(&ho_no_dup);
        process.handle_table().add_handle(ho_no_dup);
        expect_eq!(process.handle_table().handle_count(), 1);

        let mut no_dup_out = HandleValue::default();
        // Duplicate without ZX_RIGHT_DUPLICATE must return ACCESS_DENIED.
        expect_err!(
            handle_dup_replace_process(
                process,
                false,
                HandleValue::new(val_no_dup),
                ZX_RIGHT_SAME_RIGHTS,
                &mut no_dup_out,
            ),
            Status::ACCESS_DENIED
        );
        expect_eq!(process.handle_table().handle_count(), 1);

        // Replace without ZX_RIGHT_DUPLICATE must succeed on the same handle.
        expect_ok!(handle_dup_replace_process(
            process,
            true,
            HandleValue::new(val_no_dup),
            ZX_RIGHT_SAME_RIGHTS,
            &mut no_dup_out,
        ));
        expect_eq!(process.handle_table().handle_count(), 1);

        let _ = process.remove_handle(no_dup_out);
        expect_eq!(process.handle_table().handle_count(), 0);
        expect_err!(
            ep_no_dup1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }
}
