// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use counters_bindings as bindings;

/// The maximum number of CPUs that this counter descriptor supports.
/// This value is read from the `SMP_MAX_CPUS` environment variable at build time.
pub const SMP_MAX_CPUS: usize =
    zr::parse_usize(env!("SMP_MAX_CPUS")).expect("SMP_MAX_CPUS invalid");

pub use zr::to_array;

/// The aggregation type of a kernel counter.
///
/// This specifies how the diagnostic tools should combine the per-CPU slot values
/// of the counter to produce a single diagnostic value.
#[repr(u64)]
pub enum Type {
    /// Padding element (unused).
    Padding = 0,
    /// Standard summation counter (aggregates the sum across all CPUs).
    Sum = 1,
    /// Minimum tracker counter (finds the minimum value across all CPUs).
    Min = 2,
    /// Maximum tracker counter (finds the maximum value across all CPUs).
    Max = 3,
}

/// Binary-stable C-compatible representation of a kernel counter descriptor.
///
/// The memory layout of this structure matches Zircon's `counters::Descriptor` exactly,
/// enabling the linker and userspace diagnostic tools to parse Rust-declared counters
/// seamlessly from the kernel's binary segments.
#[repr(C, align(8))]
pub struct Descriptor {
    name: [u8; 56],
    type_: u64,
}

zr::static_assert!(
    core::mem::size_of::<Descriptor>() == core::mem::size_of::<bindings::counters_Descriptor>()
);
zr::static_assert!(
    core::mem::align_of::<Descriptor>() == core::mem::align_of::<bindings::counters_Descriptor>()
);
zr::static_assert!(
    core::mem::offset_of!(Descriptor, name)
        == core::mem::offset_of!(bindings::counters_Descriptor, name)
);
zr::static_assert!(
    core::mem::offset_of!(Descriptor, type_)
        == core::mem::offset_of!(bindings::counters_Descriptor, type_)
);
zr::static_assert!(Type::Padding as u64 == bindings::counters_Type_kPadding as u64);
zr::static_assert!(Type::Sum as u64 == bindings::counters_Type_kSum as u64);
zr::static_assert!(Type::Min as u64 == bindings::counters_Type_kMin as u64);
zr::static_assert!(Type::Max as u64 == bindings::counters_Type_kMax as u64);

impl Descriptor {
    /// Create a new raw `Descriptor` instance with the given packed name and type value.
    pub const fn new(name: [u8; 56], type_: u64) -> Self {
        Self { name, type_ }
    }
}

unsafe extern "C" {
    fn kcounter_add_ffi(desc: *const Descriptor, delta: i64);
    fn kcounter_min_ffi(desc: *const Descriptor, value: i64);
    fn kcounter_max_ffi(desc: *const Descriptor, value: i64);
}

/// A thread-safe diagnostic handle representing a self-declared kernel counter.
///
/// This structure contains a pointer to the counter's static `Descriptor` layout in memory,
/// and delegates increment, minimum, and maximum operations to highly optimized C++ FFI
/// handlers with zero runtime overhead under ThinLTO.
pub struct Counter {
    descriptor: *const Descriptor,
}

unsafe impl Sync for Counter {}
unsafe impl Send for Counter {}

impl Counter {
    /// Create a new Counter handle using the direct descriptor pointer address.
    ///
    /// # Safety
    /// This should only be called with a pointer to a valid, linker-defined
    /// static descriptor variable.
    pub const unsafe fn new_with_ptr(descriptor: *const Descriptor) -> Self {
        Self { descriptor }
    }

    /// Add the given delta value to the calling CPU's counter slot.
    #[inline]
    pub fn add(&self, delta: i64) {
        unsafe {
            kcounter_add_ffi(self.descriptor, delta);
        }
    }

    /// Update the calling CPU's counter slot to the minimum of its current value and the given
    /// value.
    #[inline]
    pub fn min(&self, value: i64) {
        unsafe {
            kcounter_min_ffi(self.descriptor, value);
        }
    }

    /// Update the calling CPU's counter slot to the maximum of its current value and the given
    /// value.
    #[inline]
    pub fn max(&self, value: i64) {
        unsafe {
            kcounter_max_ffi(self.descriptor, value);
        }
    }
}

/// Macro to safely define a new Counter in Rust that is visible to the kernel.
///
/// # Example
/// ```rust
/// define_kcounter!(MY_COUNTER, "my.custom.counter", Sum);
///
/// fn some_kernel_code() {
///     MY_COUNTER.add(1);
/// }
/// ```
#[macro_export]
macro_rules! define_kcounter {
    ($rust_var:ident, $name:expr, $type:ident) => {
        pub static $rust_var: $crate::Counter = {
            #[unsafe(link_section = concat!(".bss.kcounter.", $name))]
            #[used]
            static mut ARENA: [i64; $crate::SMP_MAX_CPUS] = [0; $crate::SMP_MAX_CPUS];

            #[unsafe(link_section = concat!("kcountdesc.", $name))]
            #[used]
            static DESC: $crate::Descriptor =
                $crate::Descriptor::new($crate::to_array::<56>($name), $crate::Type::$type as u64);

            unsafe { $crate::Counter::new_with_ptr(&DESC as *const $crate::Descriptor) }
        };
    };
}
