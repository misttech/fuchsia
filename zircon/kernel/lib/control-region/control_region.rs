// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod control_region_ffi;

use crate::kernel::relaxed_atomic::RelaxedAtomicU32;
use core::convert::Infallible;
use ksync::{KMutex, LockToken, RawSpinlock, guarded};
use pin_init::{PinInit, pin_init};

/// ControlRegion is a CPU synchronization tool.
///
/// Think of it as a region of a program that is protected by a gate.  When the gate is open, CPUs
/// may freely enter or leave the controlled region.  The gate may only be closed when all CPUs are
/// within the controlled region.  The last CPU to enter the region is responsible for closing the
/// gate.  CPUs may not leave the region until the gate is opened by one of them.
///
/// Usage:
///
/// ```ignore
///   // An instance shared by all CPUs.
///   stack_pin_init!(let region = ControlRegion::init());
///
///   // A managed resource of some kind, shared by all CPUs.
///   let resource = Resource::new();
///
///   ...
///
///   // Called once for each CPU before any other use of the region.
///   // Be sure to `unregister` when taking a CPU offline.
///   region.register();
///
///   ...
///
///   resource.r#use();
///
///   if !region.try_enter() {
///     // We are the last one in, shutdown the resource before closing and entering.
///     resource.shutdown();
///     region.close_enter();
///   }
///
///   // Start of the region.
///
///   // End of the region.
///
///   if !region.try_leave() {
///     // We are the first leave, we must restore the resource before opening and leaving.
///     resource.init();
///     region.open_leave();
///   }
///
///  resource.r#use();
/// ```
#[guarded]
#[repr(C)]
pub struct ControlRegion {
    #[mutex]
    lock: KMutex<RawSpinlock>,

    /// Note, this field is an atomic so that it may be read via `peek_state` without holding the
    /// lock.
    #[guarded_by(lock)]
    state: RelaxedAtomicU32,

    /// The number of CPUs outside the region.
    ///
    /// TODO(https://fxbug.dev/450973098): This value serves double duty in that it counts both the
    /// number of CPUs outside the region and the number of CPUs that could enter the region.
    /// Revisit and consider tracking them separately so we can better enforce the invariants.  See
    /// also code review discussion leading to this comment.
    #[guarded_by(lock)]
    count: u32,
}

zr::static_assert!(size_of::<ControlRegion>() == size_of::<RawSpinlock>() + 8);
zr::static_assert!(align_of::<ControlRegion>() == 8);

#[repr(u32)]
enum State {
    /// Gate is open.
    ///
    /// Valid transitions from Open:
    ///   try_enter:
    ///     count > 1:  --> Open
    ///     count == 1: --> Opening
    ///   try_leave:
    ///     count > 0:  --> Open
    ///     count == 0: --> Opening
    Open = 0,

    /// Gate is closing.
    ///
    /// Valid transitions from Closing:
    ///   close_enter:
    ///     count == 1: --> Closed
    ///   try_leave:
    ///     count == 1: --> try again later
    Closing,

    /// Gate is closed.
    ///
    /// Valid transitions from Closed:
    ///   try_leave:
    ///     count == 0: --> Opening
    Closed,

    /// Gate is opening.
    ///
    /// Valid transitions from Opening:
    ///   open_leave:
    ///     count == 0: --> Open
    ///   try_leave:
    ///     count == 0: --> try again later
    Opening,
}

impl TryFrom<u32> for State {
    type Error = ();
    fn try_from(val: u32) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(Self::Open),
            1 => Ok(Self::Closing),
            2 => Ok(Self::Closed),
            3 => Ok(Self::Opening),
            _ => Err(()),
        }
    }
}

impl ControlRegion {
    /// Creates an in-place pinned initializer for a `ControlRegion` with the gate open.
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KMutex::init(),
            state: RelaxedAtomicU32::new(State::Open as u32).into(),
            count: 0u32.into(),
        })
    }

    /// Registers a CPU for participation in a control region.
    ///
    /// This should be called once for each CPU that may participate.
    pub fn register(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        debug_assert!(
            fields.state.load() == State::Open as u32,
            "state={}, count={}",
            fields.state.load(),
            *fields.count
        );
        // See note at definition of `count`.
        *fields.count += 1;
    }

    /// Unregisters a CPU for participation in a control region.
    ///
    /// This method should be called before offlining a CPU that had been registered.
    pub fn unregister(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        debug_assert!(
            fields.state.load() == State::Open as u32 && *fields.count > 0,
            "state={}, count={}",
            fields.state.load(),
            *fields.count
        );
        // See note at definition of `count`.
        *fields.count -= 1;
    }

    /// Attempt to enter the region.
    ///
    /// Returns true if this CPU has successfully entered and isn't the last one in.
    ///
    /// Returns false and sets the state to closing if this CPU is the only remaining CPU outside
    /// the region.  This CPU is then obligated to first perform any resource management tasks
    /// associated with this region, and then finish closing the gate and entering by calling
    /// `close_enter`.
    ///
    /// It is an error to call this method with interrupts enabled.
    #[must_use]
    pub fn try_enter(&self) -> bool {
        ksync::lock!(let mut guard = self.lock_lock_policy::<ksync::NoIrqSavePolicy>());
        let fields = guard.as_mut().fields_mut();
        debug_assert!(
            fields.state.load() == State::Open as u32 && *fields.count > 0,
            "state={}, count={}",
            fields.state.load(),
            *fields.count
        );
        if *fields.count > 1 {
            *fields.count -= 1;
            return true;
        }
        // We're the last one.
        fields.state.store(State::Closing as u32);
        false
    }

    /// Close the gate and enter the region.  This is intended to be called after a call to
    /// `try_enter` has returned false and the caller has performed any necessary resource
    /// management tasks.
    ///
    /// It is an error to call this method with interrupts enabled.
    pub fn close_enter(&self) {
        ksync::lock!(let mut guard = self.lock_lock_policy::<ksync::NoIrqSavePolicy>());
        let fields = guard.as_mut().fields_mut();
        debug_assert!(
            fields.state.load() == State::Closing as u32 && *fields.count == 1,
            "state={}, count={}",
            fields.state.load(),
            *fields.count
        );
        fields.state.store(State::Closed as u32);
        *fields.count -= 1;
    }

    /// Attempt to leave the region.
    ///
    /// Returns true if this CPU has successfully left the region and there is already at least one
    /// CPU outside the region.
    ///
    /// Returns false and sets the state to opening if no other CPUs are outside the region.  This
    /// CPU is then obligated to then perform any resource management tasks associated with the
    /// region, and then finish opening the gate and leaving by calling `open_leave`.
    ///
    /// It is an error to call this method with interrupts enabled.
    ///
    /// Note: If another CPU is in the process of opening or closing the gate, this method may spin
    /// (busy-wait) until that other CPU has finished.
    pub fn try_leave(&self) -> bool {
        loop {
            {
                ksync::lock!(let mut guard = self.lock_lock_policy::<ksync::NoIrqSavePolicy>());
                let fields = guard.as_mut().fields_mut();
                let state = fields.state.load();
                match State::try_from(state) {
                    Ok(State::Open) => {
                        debug_assert!(*fields.count > 0, "count={}", *fields.count);
                        *fields.count += 1;
                        return true;
                    }

                    Ok(State::Closed) => {
                        // We must be the first to leave.  Start opening the gate.
                        debug_assert!(*fields.count == 0, "count={}", *fields.count);
                        fields.state.store(State::Opening as u32);
                        return false;
                    }

                    Ok(State::Closing) => {
                        debug_assert!(*fields.count == 1, "count={}", *fields.count);
                    }

                    Ok(State::Opening) => {
                        debug_assert!(*fields.count == 0, "count={}", *fields.count);
                    }

                    Err(()) => panic!("unexpected value {}", state),
                }
            }
            // Notice in the cases above where we observed that the state is either closing or
            // opening, we drop the lock and wait (spin) for the state to change using peek rather
            // that reacquiring the lock over and over.  We do this because our spinlocks are not
            // fair and we want to give the thread responsible for opening/closing the gate a better
            // chance of acquiring the lock so it can complete its task and transition the state to
            // either open or closed.
            let mut s = self.peek_state();
            while s != State::Open as u32 && s != State::Closed as u32 {
                libarch::r#yield();
                s = self.peek_state();
            }
        }
    }

    /// Open the gate and leave the region.  This is intended to be called after a call to
    /// `try_leave` has returned false and the caller has performed any necessary resource
    /// management tasks.
    ///
    /// It is an error to call this method with interrupts enabled.
    pub fn open_leave(&self) {
        ksync::lock!(let mut guard = self.lock_lock_policy::<ksync::NoIrqSavePolicy>());
        let fields = guard.as_mut().fields_mut();
        if cfg!(debug_assertions) {
            let s = fields.state.load();
            debug_assert!(
                s == State::Opening as u32 && *fields.count == 0,
                "state={}, count={}",
                s,
                *fields.count
            );
        }
        fields.state.store(State::Open as u32);
        *fields.count += 1;
    }

    /// Returns the state without having to hold the lock.
    fn peek_state(&self) -> u32 {
        // SAFETY: This deliberately violates `LockToken::new`'s precondition: the lock is not held.
        // The token only reaches `state` through `KCell::get`, which yields a shared reference to
        // an atomic, and a relaxed load of an atomic is sound without mutual exclusion. The token
        // is a temporary and cannot escape this expression.
        unsafe { self.state.get(&LockToken::new()).load() }
    }
}
