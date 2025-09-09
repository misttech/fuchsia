// Copyright 2023 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::vdso::vdso_loader::MemoryMappedVvar;
use fuchsia_runtime::{
    UtcClock as UtcClockHandle, UtcClockTransform, UtcInstant, zx_utc_reference_get,
};
use starnix_logging::{log_info, log_warn};
use starnix_sync::Mutex;
use std::sync::LazyLock;
use zx::{self as zx, AsHandleRef, Unowned};

fn utc_clock() -> Unowned<'static, UtcClockHandle> {
    // SAFETY: basic FFI call which returns either a valid handle or ZX_HANDLE_INVALID.
    unsafe {
        let handle = zx_utc_reference_get();
        Unowned::from_raw_handle(handle)
    }
}
fn duplicate_utc_clock_handle(rights: zx::Rights) -> Result<UtcClockHandle, zx::Status> {
    utc_clock().duplicate(rights)
}

/// A Linux-compliant UTC clock.
///
/// Many Linux APIs need a running UTC clock to function. In contrast, Fuchsia does not
/// necessarily always start its UTC clock, so Fuchsia's UTC clock can not be directly
/// reused.
///
/// Since there can be a delay until the UTC clock in Zircon starts up
/// (https://fxbug.dev/42081426), Starnix provides a synthetic utc clock initially, and polls for
/// the signal `ZX_CLOCK_STARTED`. Once this signal is asserted, the synthetic utc clock is
/// replaced by a real utc clock.
#[derive(Debug)]
struct UtcClock {
    real_utc_clock: UtcClockHandle,
    current_transform: UtcClockTransform,
    real_utc_clock_started: bool,
}

impl UtcClock {
    /// Creates a new `UtcClock` instance.
    ///
    /// The `real_utc_clock` is a handle to an underlying Fuchsia UTC clock. It will
    /// be used once started.
    pub fn new(real_utc_clock: UtcClockHandle) -> Self {
        let offset = real_utc_clock.get_details().unwrap().backstop.into_nanos()
            - zx::BootInstant::get().into_nanos();
        let current_transform = zx::ClockTransformation {
            reference_offset: zx::BootInstant::default(),
            synthetic_offset: UtcInstant::from_nanos(offset),
            rate: zx::sys::zx_clock_rate_t { synthetic_ticks: 1, reference_ticks: 1 },
        };
        let mut utc_clock =
            Self { real_utc_clock, current_transform, real_utc_clock_started: false };
        utc_clock.poll_transform();
        if !utc_clock.real_utc_clock_started {
            log_warn!(
                "Waiting for real UTC clock to start, using synthetic clock in the meantime."
            );
        }
        utc_clock
    }

    fn check_real_utc_clock_started(&self) -> bool {
        // Poll the utc clock to check if CLOCK_STARTED is asserted.
        match self
            .real_utc_clock
            .wait_handle(zx::Signals::CLOCK_STARTED, zx::MonotonicInstant::INFINITE_PAST)
            .to_result()
        {
            Ok(e) if e.contains(zx::Signals::CLOCK_STARTED) => true,
            Ok(_) | Err(zx::Status::TIMED_OUT) => false,
            Err(e) => {
                log_warn!("Error checking if CLOCK_STARTED is asserted: {:?}", e);
                false
            }
        }
    }

    /// Returns the current UTC time.
    pub fn now(&self) -> UtcInstant {
        let boot_time = zx::BootInstant::get();
        // Utc time is calculated using the same transform as the one stored in vvar. This is
        // to ensure that utc calculations are the same whether using a syscall or the vdso
        // function.
        self.current_transform.apply(boot_time)
    }

    /// Estimates the boot time corresponding to `utc`.
    ///
    /// # Returns
    /// - zx::BootInstant: estimated boot time;
    /// - bool: true if the system UTC clock has been started.
    pub fn estimate_boot_deadline(&self, utc: UtcInstant) -> (zx::BootInstant, bool) {
        (self.current_transform.apply_inverse(utc), self.real_utc_clock_started)
    }

    fn poll_transform(&mut self) {
        if !self.real_utc_clock_started {
            if self.check_real_utc_clock_started() {
                log_info!("Real UTC clock has started");
                self.real_utc_clock_started = true;
            }
        }
        if self.real_utc_clock_started {
            self.current_transform =
                self.real_utc_clock.get_details().unwrap().reference_to_synthetic;
        }
    }

    /// Updates the UTC clock transform.
    ///
    /// Fetches the most up-to-date clock transform from Zircon, then updates the clock transform in
    /// both self (the UtcClock) and dest (the MemoryMappedVvar).
    pub fn update_utc_clock(&mut self, dest: &MemoryMappedVvar) {
        self.poll_transform();
        // TODO(https://fxbug.dev/356911500): Remove the parsing
        let reference_transform = zx::ClockTransformation {
            reference_offset: zx::BootInstant::from_nanos(
                self.current_transform.reference_offset.into_nanos(),
            ),
            synthetic_offset: self.current_transform.synthetic_offset,
            rate: self.current_transform.rate.clone(),
        };
        dest.update_utc_data_transform(&reference_transform);
    }
}

static UTC_CLOCK: LazyLock<Mutex<UtcClock>> = LazyLock::new(|| {
    Mutex::new(UtcClock::new(duplicate_utc_clock_handle(zx::Rights::SAME_RIGHTS).unwrap()))
});

/// Updates the UTC clock transform.
pub fn update_utc_clock(dest: &MemoryMappedVvar) {
    (*UTC_CLOCK).lock().update_utc_clock(dest);
}

/// Returns the current UTC time.
pub fn utc_now() -> UtcInstant {
    #[cfg(test)]
    {
        if let Some(test_time) = UTC_CLOCK_OVERRIDE_FOR_TESTING
            .with(|cell| cell.borrow().as_ref().map(|test_clock| test_clock.read().unwrap()))
        {
            return test_time;
        }
    }
    (*UTC_CLOCK).lock().now()
}

/// Estimates the boot time corresponding to `utc`, based on the currently
/// operating Starnix UTC clock.
///
/// # Returns
/// - zx::BootInstant: estimated boot time;
/// - bool: true if the system UTC clock has been started.
pub fn estimate_boot_deadline_from_utc(utc: UtcInstant) -> (zx::BootInstant, bool) {
    #[cfg(test)]
    {
        if let Some(test_time) = UTC_CLOCK_OVERRIDE_FOR_TESTING.with(|cell| {
            cell.borrow().as_ref().map(|test_clock| {
                test_clock.get_details().unwrap().reference_to_synthetic.apply_inverse(utc)
            })
        }) {
            return (test_time, true);
        }
    }
    (*UTC_CLOCK).lock().estimate_boot_deadline(utc)
}

#[cfg(test)]
thread_local! {
    static UTC_CLOCK_OVERRIDE_FOR_TESTING: std::cell::RefCell<Option<UtcClockHandle>> =
        std::cell::RefCell::new(None);
}

/// A guard that temporarily overrides the UTC clock for testing.
///
/// When this guard is created, it replaces the global UTC clock with a test clock. When the guard
/// is dropped, the original clock is restored.
#[cfg(test)]
pub struct UtcClockOverrideGuard(());

#[cfg(test)]
impl UtcClockOverrideGuard {
    /// Creates a new `UtcClockOverrideGuard`.
    ///
    /// This function replaces the global UTC clock with `test_clock`. The original clock is
    /// restored when the returned guard is dropped.
    pub fn new(test_clock: UtcClockHandle) -> Self {
        UTC_CLOCK_OVERRIDE_FOR_TESTING.with(|cell| {
            assert_eq!(*cell.borrow(), None); // We don't expect a previously set clock override when using this type.
            *cell.borrow_mut() = Some(test_clock);
        });
        Self(())
    }
}

#[cfg(test)]
impl Drop for UtcClockOverrideGuard {
    fn drop(&mut self) {
        UTC_CLOCK_OVERRIDE_FOR_TESTING.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}
