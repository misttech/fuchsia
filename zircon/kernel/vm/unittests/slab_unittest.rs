// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Slab allocator tests duplicated from slab_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod slab_rs {
    use crate::vm::page_slab_allocator::{BaseSlabProvider, PageSlabAllocator};
    use core::mem::size_of;
    use core::ptr::NonNull;
    use pin_init::stack_pin_init;
    use unittest::unwrap_ok;

    struct TestObject {
        _data: [u64; 32],
    }

    /// Smoke test allocating and freeing a single object from a slab allocator.
    #[test]
    fn slab_smoke_test() {
        stack_pin_init!(
            let alloc = PageSlabAllocator::<{ size_of::<TestObject>() }, _>::new_with(
                pin_init::init!(BaseSlabProvider {})
            )
        );
        let obj: NonNull<TestObject> = unwrap_ok!(alloc.as_mut().allocate_object());
        // SAFETY: `obj` is valid for writing TestObject.
        unsafe {
            obj.write(TestObject { _data: [0; 32] });
        }
        // SAFETY: `obj` was allocated by `alloc`.
        unsafe {
            alloc.as_mut().deallocate_bytes(obj.cast());
        }
    }
}
