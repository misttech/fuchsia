// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Unit tests for `VmCompression` and `VmCompressor`.
#[cfg(ktest)]
#[unittest::suite]
mod compression_rs {
    use crate::vm::compression::{VmCompression, VmLz4Compressor, VmSlotPageStorage};
    use crate::vm::compressor::{CompressResult, CompressedRef, PageAndMetadata};
    use crate::vm::page::VmPagePtr;
    use crate::vm::physmap::paddr_to_physmap;
    use crate::vm::pmm;
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{assert_ok, assert_true, expect_eq, expect_true, unwrap_ok};

    /// # Safety
    ///
    /// `page` must be a valid, exclusively owned page, and `len` must be `<= PAGE_SIZE_USIZE`.
    unsafe fn write_zeros(page: VmPagePtr, len: usize) {
        debug_assert!(!page.as_ffi().is_null());
        debug_assert!(len <= PAGE_SIZE_USIZE);
        // SAFETY: By caller precondition, `page` is a valid, exclusively owned page.
        let paddr = unsafe { page.paddr() };
        let addr = paddr_to_physmap(paddr);
        let data = core::ptr::with_exposed_provenance_mut::<u8>(addr.0);
        // SAFETY: Physical pages mapped in the kernel physmap consist of `page::SIZE` initialized
        // `u8` bytes valid for reads and exclusive writes by the page owner, and
        // `len <= PAGE_SIZE_USIZE`.
        let slice = unsafe { core::slice::from_raw_parts_mut(data, len) };
        for byte in slice.iter_mut() {
            *byte = 0;
        }
    }

    /// # Safety
    ///
    /// `page` must be a valid, exclusively owned page, and `len` must be `<= PAGE_SIZE_USIZE`.
    unsafe fn write_pattern(page: VmPagePtr, len: usize, offset: u64) {
        debug_assert!(!page.as_ffi().is_null());
        debug_assert!(len <= PAGE_SIZE_USIZE);
        // SAFETY: By caller precondition, `page` is a valid, exclusively owned page.
        let paddr = unsafe { page.paddr() };
        let addr = paddr_to_physmap(paddr);
        let data = core::ptr::with_exposed_provenance_mut::<u8>(addr.0);
        // SAFETY: Physical pages mapped in the kernel physmap consist of `page::SIZE` initialized
        // `u8` bytes valid for reads and exclusive writes by the page owner, and
        // `len <= PAGE_SIZE_USIZE`.
        let slice = unsafe { core::slice::from_raw_parts_mut(data, len) };
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = ((i as u64 + offset) % 256) as u8;
        }
    }

    fn validate_pattern(data: &[u8], offset: u64) -> bool {
        for (i, &byte) in data.iter().enumerate() {
            if byte != ((i as u64 + offset) % 256) as u8 {
                return false;
            }
        }
        true
    }

    /// Test that the high-level compression-deconmpression flow preserves page data and metadata.
    #[test]
    fn compression_smoke_test() {
        const COMPRESSION_THRESHHOLD: usize = PAGE_SIZE_USIZE * 70 / 100;
        let lz4 = VmLz4Compressor::create();
        assert_true!(lz4.is_some());
        let lz4 = lz4.unwrap();
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();
        let compression = unwrap_ok!(VmCompression::new(storage, lz4, COMPRESSION_THRESHHOLD));

        const METADATA_START: u32 = 0xdeadbeef;
        const METADATA_UPDATE: u32 = 0xc0ffee;

        // Compress a page full of content and get its ref.
        let compressed_ref: CompressedRef;
        {
            let (page, _) = unwrap_ok!(pmm::alloc_page(0));
            // SAFETY: `page` was just allocated from the PMM and is exclusively owned.
            unsafe { write_pattern(page, PAGE_SIZE_USIZE, 0) };

            stack_pin_init!(let compressor_guard = compression.acquire_compressor());
            let compressor = compressor_guard.get();

            assert_ok!(compressor.arm());
            // SAFETY: We own `compressor` in the `Ready` state, `page` is valid and unmodified
            // until `finalize`, and this single-threaded test acts as the VMO lock holder.
            let temp_ref = unsafe {
                compressor.start(PageAndMetadata { page: Some(page), metadata: METADATA_START })
            };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_START, unsafe { compression.get_metadata(temp_ref) });

            // SAFETY: `compressor` is in the `Started` state, `page` remains valid, and no locks
            // are held.
            unsafe { compressor.compress() };

            // Modifying metadata while before acquiring the compression result is legal - the
            // final result should reflect the modifications.
            // SAFETY: `temp_ref` is valid and owned by this test.
            unsafe { compression.set_metadata(temp_ref, METADATA_UPDATE) };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_UPDATE, unsafe { compression.get_metadata(temp_ref) });

            // SAFETY: `compressor` is in the `Compressed` state and we hold logical ownership of
            // `temp_ref`.
            let compress_result = unsafe { compressor.take_compression_result() };
            assert_true!(matches!(compress_result, CompressResult::CompressedRef(_)));
            let CompressResult::CompressedRef(r) = compress_result else {
                unreachable!();
            };
            compressed_ref = r;
            // SAFETY: `temp_ref` and `compressed_ref` are valid and owned by this test.
            expect_eq!(METADATA_UPDATE, unsafe { compression.get_metadata(temp_ref) });
            // SAFETY: `compressed_ref` is valid and owned by this test.
            expect_eq!(METADATA_UPDATE, unsafe { compression.get_metadata(compressed_ref) });

            // SAFETY: We hold logical ownership of `temp_ref` and return it before `finalize`.
            unsafe {
                compressor.return_temp_reference(temp_ref);
                compressor.finalize();
                pmm::free_page(page);
            }
        }

        // Decompress the previous page and check that the data and metadata were preserved.
        {
            let (page, paddr) = unwrap_ok!(pmm::alloc_page(0));

            let mut page_metadata: u32 = 0;
            let page_addr = paddr_to_physmap(paddr);
            let page_data = core::ptr::with_exposed_provenance_mut::<u8>(page_addr.0);
            // SAFETY: `compressed_ref` is a valid reference produced by `compression`,
            // `page_data` points to `PAGE_SIZE_USIZE` writable bytes of `page`, and no locks are
            // held.
            unsafe {
                compression.decompress(compressed_ref, page_data, &mut page_metadata);
            }
            // SAFETY: `decompress` initialized all `PAGE_SIZE_USIZE` bytes at `page_data`.
            let page_slice = unsafe { core::slice::from_raw_parts(page_data, PAGE_SIZE_USIZE) };
            expect_true!(validate_pattern(page_slice, 0));
            expect_eq!(METADATA_UPDATE, page_metadata);

            // SAFETY: `page` was allocated from the PMM and is no longer used.
            unsafe { pmm::free_page(page) };
        }
    }

    /// Test that compression can detect zero pages as input and flag them appropriately.
    #[test]
    fn compression_zero_test() {
        const COMPRESSION_THRESHHOLD: usize = PAGE_SIZE_USIZE * 70 / 100;
        let lz4 = VmLz4Compressor::create();
        assert_true!(lz4.is_some());
        let lz4 = lz4.unwrap();
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();
        let compression = unwrap_ok!(VmCompression::new(storage, lz4, COMPRESSION_THRESHHOLD));

        // Compress a page full of zeros and ensure the compression system detects this.
        {
            let (page, _) = unwrap_ok!(pmm::alloc_page(0));
            // SAFETY: `page` was just allocated from the PMM and is exclusively owned.
            unsafe { write_zeros(page, PAGE_SIZE_USIZE) };

            stack_pin_init!(let compressor_guard = compression.acquire_compressor());
            let compressor = compressor_guard.get();

            assert_ok!(compressor.arm());
            // SAFETY: We own `compressor` in the `Ready` state and `page` is valid until
            // `finalize`.
            let temp_ref =
                unsafe { compressor.start(PageAndMetadata { page: Some(page), metadata: 0 }) };

            // SAFETY: `compressor` is in the `Started` state, `page` remains valid, and no locks
            // are held.
            unsafe { compressor.compress() };
            // SAFETY: `compressor` is in the `Compressed` state.
            let compress_result = unsafe { compressor.take_compression_result() };
            expect_true!(matches!(compress_result, CompressResult::Zero));

            // SAFETY: `temp_ref` is returned before `finalize`, and `page` is freed afterwards.
            unsafe {
                compressor.return_temp_reference(temp_ref);
                compressor.finalize();
                pmm::free_page(page);
            }
        }
    }

    /// Test that compression correctly handles failure to compress a page.
    #[test]
    fn compression_fail_test() {
        const COMPRESSION_THRESHHOLD: usize = 0; // Guarantee failure to compress.
        let lz4 = VmLz4Compressor::create();
        assert_true!(lz4.is_some());
        let lz4 = lz4.unwrap();
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();
        let compression = unwrap_ok!(VmCompression::new(storage, lz4, COMPRESSION_THRESHHOLD));

        const METADATA_START: u32 = 0xdeadbeef;
        const METADATA_UPDATE: u32 = 0xc0ffee;

        // Compress a page full of content, which fails, and ensure the compression system handles
        // this.
        {
            let (page, _) = unwrap_ok!(pmm::alloc_page(0));
            // SAFETY: `page` was just allocated from the PMM and is exclusively owned.
            unsafe { write_pattern(page, PAGE_SIZE_USIZE, 0) };

            stack_pin_init!(let compressor_guard = compression.acquire_compressor());
            let compressor = compressor_guard.get();

            assert_ok!(compressor.arm());
            // SAFETY: We own `compressor` in the `Ready` state and `page` is valid until
            // `finalize`.
            let temp_ref = unsafe {
                compressor.start(PageAndMetadata { page: Some(page), metadata: METADATA_START })
            };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_START, unsafe { compression.get_metadata(temp_ref) });

            // SAFETY: `compressor` is in the `Started` state, `page` remains valid, and no locks
            // are held.
            unsafe { compressor.compress() };

            // Modifying metadata while before acquiring the compression result is legal - the
            // final result should reflect the modifications.
            // SAFETY: `temp_ref` is valid and owned by this test.
            unsafe { compression.set_metadata(temp_ref, METADATA_UPDATE) };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_UPDATE, unsafe { compression.get_metadata(temp_ref) });

            // SAFETY: `compressor` is in the `Compressed` state.
            let compress_result = unsafe { compressor.take_compression_result() };
            assert_true!(matches!(compress_result, CompressResult::Fail(_)));
            let CompressResult::Fail(fail) = compress_result else {
                unreachable!();
            };
            expect_true!(fail.src_page.page == Some(page));
            expect_eq!(METADATA_UPDATE, fail.src_page.metadata);

            // SAFETY: `temp_ref` is returned before `finalize`, and `page` is freed afterwards.
            unsafe {
                compressor.return_temp_reference(temp_ref);
                compressor.finalize();
                pmm::free_page(page);
            }
        }
    }

    /// Test that compression correctly handles moving of temp references.
    #[test]
    fn compression_move_reference_test() {
        const COMPRESSION_THRESHHOLD: usize = PAGE_SIZE_USIZE * 70 / 100;
        let lz4 = VmLz4Compressor::create();
        assert_true!(lz4.is_some());
        let lz4 = lz4.unwrap();
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();
        let compression = unwrap_ok!(VmCompression::new(storage, lz4, COMPRESSION_THRESHHOLD));

        const METADATA_START: u32 = 0xdeadbeef;
        const METADATA_UPDATE: u32 = 0xc0ffee;

        // Compress a page full of content but move the ref while compression is in progress.
        {
            let (page, _) = unwrap_ok!(pmm::alloc_page(0));
            // SAFETY: `page` was just allocated from the PMM and is exclusively owned.
            unsafe { write_pattern(page, PAGE_SIZE_USIZE, 0) };

            stack_pin_init!(let compressor_guard = compression.acquire_compressor());
            let compressor = compressor_guard.get();

            assert_ok!(compressor.arm());
            // SAFETY: We own `compressor` in the `Ready` state and `page` is valid until
            // `finalize`.
            let temp_ref = unsafe {
                compressor.start(PageAndMetadata { page: Some(page), metadata: METADATA_START })
            };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_START, unsafe { compression.get_metadata(temp_ref) });

            // SAFETY: `compressor` is in the `Started` state, `page` remains valid, and no locks
            // are held.
            unsafe { compressor.compress() };

            // Modifying metadata while before acquiring the compression result is legal - the
            // final result should reflect the modifications.
            // SAFETY: `temp_ref` is valid and owned by this test.
            unsafe { compression.set_metadata(temp_ref, METADATA_UPDATE) };
            // SAFETY: `temp_ref` is valid and owned by this test.
            expect_eq!(METADATA_UPDATE, unsafe { compression.get_metadata(temp_ref) });

            // Move the temp reference before acquiring the compression result, which will
            // invalidate it.
            // SAFETY: `temp_ref` is currently in use and owned by this test.
            let maybe_page = unsafe { compression.move_reference(temp_ref) };
            assert_true!(maybe_page.is_some());
            let moved_page = maybe_page.unwrap();
            expect_eq!(METADATA_UPDATE, moved_page.metadata);
            // SAFETY: We hold logical ownership of the temporary reference state.
            expect_true!(unsafe { !compressor.is_temp_reference_in_use() });

            // The compression result can still be obtained, but the metadata will be empty.
            // SAFETY: `compressor` is in the `Compressed` state.
            let compress_result = unsafe { compressor.take_compression_result() };
            assert_true!(matches!(compress_result, CompressResult::CompressedRef(_)));
            let CompressResult::CompressedRef(compressed_ref) = compress_result else {
                unreachable!();
            };
            // SAFETY: `compressed_ref` is valid and owned by this test.
            expect_eq!(0u32, unsafe { compression.get_metadata(compressed_ref) });

            // SAFETY: `compressed_ref` is a valid non-temporary reference, `temp_ref` was already
            // returned by `move_reference`, and `moved_page.page` and `page` are owned by us.
            unsafe {
                compressor.free(compressed_ref);
                compressor.finalize();

                pmm::free_page(moved_page.page.unwrap());
                pmm::free_page(page);
            }
        }
    }
}
