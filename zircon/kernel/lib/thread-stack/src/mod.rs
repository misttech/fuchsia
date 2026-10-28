// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel thread stack ABI: the sizes and guard regions of the machine stack and, when enabled,
//! the unsafe stack and shadow call stack.

use handoff::ZirconAbiSpec_Stack;

/// Stack size building blocks shared by the stack specifications below.
pub mod internal {
    /// A common, convenience value for machine and unsafe stack sizes. Not intended
    /// to be a part of the public API.
    // The kernel build sets CUSTOM_DEFAULT_STACK_SIZE in the environment from the same GN value
    // that it passes to C++ as a define.
    pub const DEFAULT_STACK_SIZE: u32 = match option_env!("CUSTOM_DEFAULT_STACK_SIZE") {
        Some(size) => {
            let size = zr::parse_usize(size).expect("CUSTOM_DEFAULT_STACK_SIZE invalid");
            assert!(size <= u32::MAX as usize, "CUSTOM_DEFAULT_STACK_SIZE too large");
            size as u32
        }
        None => 2 * page::SIZE as u32,
    };

    /// The would-be unsafe stack size, if enabled.
    pub const UNSAFE_STACK_SIZE: u32 = DEFAULT_STACK_SIZE;

    /// The size of the machine stack.
    pub const MACHINE_STACK_SIZE: u32 =
        // If there is no unsafe stack on x86, extend the machine stack by the
        // would-be unsafe stack size for consistency.
        DEFAULT_STACK_SIZE
            + if cfg!(all(target_arch = "x86_64", not(sanitize = "safestack"))) {
                UNSAFE_STACK_SIZE
            } else {
                0
            };

    // Physboot requires page-aligned stack sizes (`ZirconAbiSpec::Stack::AssertValid`), so a bad
    // CUSTOM_DEFAULT_STACK_SIZE fails the build here rather than at boot.
    zr::static_assert!(page::is_aligned(UNSAFE_STACK_SIZE as usize));
    zr::static_assert!(page::is_aligned(MACHINE_STACK_SIZE as usize));

    /// The would-be unsafe shadow call stack size, if enabled.
    pub const SHADOW_CALL_STACK_SIZE: u32 = page::SIZE as u32;

    /// The size of the unmapped 'guard' region to ensure lies above or below the
    /// mapped stack.
    pub const STACK_GUARD_REGION_SIZE: u32 = page::SIZE as u32;
}

/// The machine stack specification.
pub const MACHINE_STACK: ZirconAbiSpec_Stack = ZirconAbiSpec_Stack {
    size_bytes: internal::MACHINE_STACK_SIZE,
    lower_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
    upper_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
};

/// The unsafe stack specification.
#[cfg(sanitize = "safestack")]
pub const UNSAFE_STACK: ZirconAbiSpec_Stack = ZirconAbiSpec_Stack {
    size_bytes: internal::UNSAFE_STACK_SIZE,
    lower_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
    upper_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
};

/// The shadow call stack specification.
#[cfg(sanitize = "shadow-call-stack")]
pub const SHADOW_CALL_STACK: ZirconAbiSpec_Stack = ZirconAbiSpec_Stack {
    size_bytes: internal::SHADOW_CALL_STACK_SIZE,
    lower_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
    upper_guard_size_bytes: internal::STACK_GUARD_REGION_SIZE,
};

// `top/handoff.cc` initializes the kernel's `kZirconAbiSpec` from the C++
// `<lib/thread-stack/abi.h>` constants, and physboot uses it to set up the boot stacks.
/// Checks the `thread_stack` constants against the kernel's `kZirconAbiSpec`.
#[cfg(ktest)]
#[unittest::suite(name = "thread_stack")]
mod tests {
    use handoff::{ZirconAbiSpec, ZirconAbiSpec_Stack};
    use unittest::expect_eq;

    // SAFETY: Declaring this `safe` is sound because reads of it can never be UB:
    // - `ZirconAbiSpec` is the bindgen type for the C++ object, and its layout is pinned by the
    //   handoff-rs `layout_tests`.
    // - `kZirconAbiSpec` is `extern "C" constexpr` in `top/handoff.cc`, so it lives in
    //   RODATA/RELRO, is relocated by physboot before the kernel is entered, and is never written
    //   afterwards.
    // - Every field holds a valid value for its Rust type (`bool` is 0 or 1, the `entry` function
    //   pointer is non-null) because the C++ initializer sets them.
    unsafe extern "C" {
        /// The kernel's ABI specification for physboot.
        #[link_name = "kZirconAbiSpec"]
        safe static ZIRCON_ABI_SPEC: ZirconAbiSpec;
    }

    /// The value that `kZirconAbiSpec` has for a stack that is not enabled.
    const NO_STACK: ZirconAbiSpec_Stack =
        ZirconAbiSpec_Stack { size_bytes: 0, lower_guard_size_bytes: 0, upper_guard_size_bytes: 0 };

    /// Expects `$rust` to match `$cpp` field by field. This is a macro rather than a function
    /// because `expect_eq!` records failures in the enclosing `#[test]` function.
    macro_rules! expect_stack_eq {
        ($rust:expr, $cpp:expr) => {{
            let rust: &ZirconAbiSpec_Stack = $rust;
            let cpp: &ZirconAbiSpec_Stack = $cpp;
            expect_eq!(rust.size_bytes, cpp.size_bytes);
            expect_eq!(rust.lower_guard_size_bytes, cpp.lower_guard_size_bytes);
            expect_eq!(rust.upper_guard_size_bytes, cpp.upper_guard_size_bytes);
        }};
    }

    /// Tests that `MACHINE_STACK` matches the C++ machine stack.
    #[test]
    fn machine_stack() {
        expect_stack_eq!(&super::MACHINE_STACK, &ZIRCON_ABI_SPEC.machine_stack);
    }

    /// Tests that `UNSAFE_STACK` exists exactly when C++ has one and that it matches.
    #[test]
    fn unsafe_stack() {
        #[cfg(sanitize = "safestack")]
        expect_stack_eq!(&super::UNSAFE_STACK, &ZIRCON_ABI_SPEC.unsafe_stack);
        #[cfg(not(sanitize = "safestack"))]
        expect_stack_eq!(&NO_STACK, &ZIRCON_ABI_SPEC.unsafe_stack);
    }

    /// Tests that `SHADOW_CALL_STACK` exists exactly when C++ has one and that it matches.
    #[test]
    fn shadow_call_stack() {
        #[cfg(sanitize = "shadow-call-stack")]
        expect_stack_eq!(&super::SHADOW_CALL_STACK, &ZIRCON_ABI_SPEC.shadow_call_stack);
        #[cfg(not(sanitize = "shadow-call-stack"))]
        expect_stack_eq!(&NO_STACK, &ZIRCON_ABI_SPEC.shadow_call_stack);
    }
}
