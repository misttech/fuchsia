// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// A helper macro to initialize a pinned object in-place using a raw FFI constructor.
///
/// This generates a `PinInit` block that passes a type-cast pointer of the allocated
/// slot to the FFI function, returning `Ok(())`.
#[macro_export]
macro_rules! pin_init_ffi {
    ($ffi_fn:expr) => {
        unsafe {
            pin_init::pin_init_from_closure(|slot| {
                let ptr = slot as *mut _ as *mut core::ffi::c_void;
                $ffi_fn(ptr);
                Ok(())
            })
        }
    };
}
