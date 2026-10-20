// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Lock dependency graph diagnostics and console commands.

use core::ffi::{CStr, c_char};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use zr::OpaqueFacade;

const MAX_LOCK_DEPENDENCIES: usize = 193;

/// Opaque representation of C++ `lockdep::ValidatorLockClassState`.
#[repr(C, align(8))]
pub struct ValidatorLockClassState {
    _facade: OpaqueFacade,
}

unsafe extern "C" {
    /// Returns the first `ValidatorLockClassState` in the global lock class list, or null.
    ///
    /// # Safety
    ///
    /// Always safe to call; returns the head of the static lock class list (or null).
    fn cpp_lockdep_iter_begin() -> *mut ValidatorLockClassState;
    /// Returns the next `ValidatorLockClassState` after `state` in the global lock class list, or
    /// null.
    ///
    /// # Safety
    ///
    /// `state` must be a non-null pointer previously returned by `cpp_lockdep_iter_begin` or
    /// `cpp_lockdep_iter_next`.
    fn cpp_lockdep_iter_next(state: *mut ValidatorLockClassState) -> *mut ValidatorLockClassState;
    /// Returns the static null-terminated name string for `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ValidatorLockClassState`.
    fn cpp_lockdep_state_name(state: *const ValidatorLockClassState) -> *const c_char;
    /// Returns a pointer to `state`'s `LockDependencySet` array.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ValidatorLockClassState`.
    fn cpp_lockdep_state_dependency_set(
        state: *const ValidatorLockClassState,
    ) -> *const [AtomicPtr<ValidatorLockClassState>; MAX_LOCK_DEPENDENCIES];
    /// Returns the root of the connected set containing `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ValidatorLockClassState`.
    fn cpp_lockdep_state_connected_set(
        state: *mut ValidatorLockClassState,
    ) -> *mut ValidatorLockClassState;
    /// Triggers an asynchronous loop detection pass.
    ///
    /// # Safety
    ///
    /// Always safe to call; sets the atomic dirty flag to trigger loop detection.
    fn cpp_lockdep_trigger_loop_detection();
}

impl ValidatorLockClassState {
    /// Returns the name of this lock class (`ValidatorLockClassState::name()`).
    pub fn name(&self) -> &CStr {
        // SAFETY: `self` is a valid `ValidatorLockClassState` whose name is a static C string.
        unsafe { CStr::from_ptr(cpp_lockdep_state_name(self)) }
    }

    /// Returns the type name of the lock class for the given lock class ID
    /// (`ValidatorLockClassState::GetName(id)`).
    pub fn get_name(id: &Self) -> &CStr {
        // SAFETY: `id` is a valid `ValidatorLockClassState` whose name is a static C string.
        unsafe { CStr::from_ptr(cpp_lockdep_state_name(id)) }
    }

    /// Returns the root of the connected set containing `self`
    /// (`ValidatorLockClassState::connected_set()`).
    pub fn connected_set(&self) -> *const Self {
        // SAFETY: `self` is a valid `ValidatorLockClassState`.
        unsafe { cpp_lockdep_state_connected_set(ptr::from_ref(self).cast_mut()).cast_const() }
    }

    /// Returns an iterator over all registered `ValidatorLockClassState` instances
    /// (`ValidatorLockClassState::Iter()`).
    pub fn iter() -> impl Iterator<Item = &'static Self> {
        // SAFETY: `cpp_lockdep_iter_begin` returns the head of the static linked list of lock
        // classes (or null).
        let mut curr = unsafe { cpp_lockdep_iter_begin() };
        core::iter::from_fn(move || {
            let state = curr;
            if state.is_null() {
                None
            } else {
                // SAFETY: `state` is a valid node in the static linked list of lock classes.
                unsafe {
                    curr = cpp_lockdep_iter_next(state);
                    Some(&*state)
                }
            }
        })
    }

    /// Returns an iterator over the dependency set of `self`
    /// (`ValidatorLockClassState::dependency_set()`).
    pub fn dependency_set(&self) -> impl Iterator<Item = &'static Self> + '_ {
        // SAFETY: `cpp_lockdep_state_dependency_set` returns a non-null pointer to the lock
        // class's `LockDependencySet` (`[AtomicPtr<ValidatorLockClassState>; 193]`).
        let set = unsafe { &*cpp_lockdep_state_dependency_set(self) };
        set.iter().filter_map(|entry| {
            let ptr = entry.load(Ordering::Relaxed);
            // SAFETY: Any non-null entry in `LockDependencySet` points to a valid `'static`
            // `ValidatorLockClassState`.
            unsafe { ptr.as_ref() }
        })
    }
}

/// Triggers an asynchronous loop detection pass (`lockdep::SystemTriggerLoopDetection()`).
pub fn system_trigger_loop_detection() {
    // SAFETY: Safe FFI call into `lockdep::SystemTriggerLoopDetection`.
    unsafe { cpp_lockdep_trigger_loop_detection() };
}

#[cfg(console_enabled)]
mod console {
    use super::{ValidatorLockClassState, system_trigger_loop_detection};
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use core::ffi::c_int;
    use core::ptr;
    use kprint::kprintln;

    /// Dumps the state of the lock dependency graph.
    fn dump_lock_class_state() {
        kprintln!("Lock class states:");
        for state in ValidatorLockClassState::iter() {
            kprintln!("  {:s} {{", state.name());
            for id in state.dependency_set() {
                kprintln!("    {:s}", ValidatorLockClassState::get_name(id));
            }
            kprintln!("  }}");
        }
        kprintln!("\nConnected sets:");
        for state in ValidatorLockClassState::iter() {
            // Only handle root nodes in the outer loop. The nested loop will pick
            // up all of the child nodes under each parent node.
            if ptr::eq(state.connected_set(), state) {
                kprintln!("{{");
                for other_state in ValidatorLockClassState::iter() {
                    if ptr::eq(other_state.connected_set(), state) {
                        kprintln!("  {:s}", other_state.name());
                    }
                }
                kprintln!("}}");
            }
        }
    }

    /// Top-level lockdep command.
    ///
    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where
    /// `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn command_lock_dep(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let usage = || -> c_int {
            kprintln!("{:s} dump              : dump lock classes", argv[0].as_str());
            kprintln!("{:s} loop              : trigger loop detection pass", argv[0].as_str());
            -1
        };
        if argc < 2 {
            kprintln!("Not enough arguments:");
            return usage();
        }

        if argv[1].as_str() == "dump" {
            dump_lock_class_state();
        } else if argv[1].as_str() == "loop" {
            kprintln!("Triggering loop detection pass:");
            system_trigger_loop_detection();
        } else {
            kprintln!("Unrecognized subcommand: '{:s}'", argv[1].as_str());
            return usage();
        }

        0
    }

    static_command!(
        CMD_LOCKDEP,
        c"lockdep".as_ptr(),
        c"kernel lock diagnostics".as_ptr(),
        command_lock_dep,
        CMD_AVAIL_NORMAL
    );
}
