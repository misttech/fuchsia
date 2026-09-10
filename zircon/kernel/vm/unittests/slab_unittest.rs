// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Slab allocator tests duplicated from slab_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod slab_rs {
    use crate::vm::page::VmPage;
    use crate::vm::page_slab_allocator::{
        BaseSlabProvider, PageSlabAllocator, SlabAllocationError, SlabProvider,
    };
    use core::convert::Infallible;
    use core::mem::size_of;
    use core::pin::Pin;
    use core::ptr::NonNull;
    use pin_init::{PinInit, pin_data, pin_init, stack_pin_init};
    use unittest::{expect_eq, unwrap_ok};

    struct TestObject {
        _data: [u64; 32],
    }

    const OBJECTS_PER_SLAB: usize =
        PageSlabAllocator::<{ size_of::<TestObject>() }, BaseSlabProvider>::ALLOCS_PER_SLAB;

    struct TestSlabProvider {
        base: BaseSlabProvider,
        allocated: usize,
        freed: usize,
    }

    impl SlabProvider for TestSlabProvider {
        fn alloc_slab(self: Pin<&mut Self>) -> Result<NonNull<VmPage>, SlabAllocationError> {
            let this = self.get_mut();
            let slab = Pin::new(&mut this.base).alloc_slab()?;
            this.allocated += 1;
            Ok(slab)
        }

        unsafe fn free_slab(self: Pin<&mut Self>, slab: NonNull<VmPage>) {
            let this = self.get_mut();
            let base = Pin::new(&mut this.base);
            // SAFETY: Caller guarantees `slab` was allocated by `alloc_slab` and has not
            // been freed.
            unsafe {
                base.free_slab(slab);
            }
            this.freed += 1;
        }
    }

    #[pin_data(PinnedDrop)]
    struct TestSlabAllocator {
        #[pin]
        allocator: PageSlabAllocator<{ size_of::<TestObject>() }, TestSlabProvider>,
    }

    impl TestSlabAllocator {
        fn new() -> impl PinInit<Self, Infallible> {
            pin_init!(Self {
                allocator <- PageSlabAllocator::new_with(pin_init::init!(TestSlabProvider {
                    base: BaseSlabProvider {},
                    allocated: 0,
                    freed: 0,
                })),
            })
        }

        fn slabs_allocated(&self) -> usize {
            self.allocator.provider().allocated
        }

        fn slabs_freed(&self) -> usize {
            self.allocator.provider().freed
        }

        fn active_slabs(&self) -> usize {
            self.allocator.provider().allocated - self.allocator.provider().freed
        }

        fn as_allocator(
            self: Pin<&mut Self>,
        ) -> Pin<&mut PageSlabAllocator<{ size_of::<TestObject>() }, TestSlabProvider>> {
            self.project().allocator
        }
    }

    #[pin_init::pinned_drop]
    impl PinnedDrop for TestSlabAllocator {
        fn drop(self: Pin<&mut Self>) {
            self.project().allocator.debug_free_all_slabs();
        }
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

    /// Tests that emptying a slab returns it to the provider.
    #[test]
    fn slab_return_slab_test() {
        stack_pin_init!(let alloc = TestSlabAllocator::new());

        // Allocate one slab worth of objects.
        for _ in 0..OBJECTS_PER_SLAB {
            unwrap_ok!(alloc.as_mut().as_allocator().allocate_bytes());
        }
        expect_eq!(alloc.active_slabs(), 1);

        // Allocate a second slab worth of objects.
        let mut objects: [NonNull<TestObject>; OBJECTS_PER_SLAB] =
            [NonNull::dangling(); OBJECTS_PER_SLAB];
        for object in &mut objects {
            let obj: NonNull<TestObject> =
                unwrap_ok!(alloc.as_mut().as_allocator().allocate_object());
            // SAFETY: `obj` points to valid uninitialized memory allocated by `alloc`.
            unsafe {
                obj.write(TestObject { _data: [0; 32] });
            }
            *object = obj;
        }
        expect_eq!(alloc.active_slabs(), 2);

        // Allocate into a third slab.
        unwrap_ok!(alloc.as_mut().as_allocator().allocate_bytes());
        expect_eq!(alloc.active_slabs(), 3);

        // Free all the objects on the second slab and ensure the slab was returned.
        for object in objects {
            expect_eq!(alloc.active_slabs(), 3);
            // SAFETY: `object` was allocated by `alloc`.
            unsafe {
                alloc.as_mut().as_allocator().deallocate_bytes(object.cast());
            }
        }
        expect_eq!(alloc.active_slabs(), 2);
    }
}
