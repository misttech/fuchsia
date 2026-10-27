// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Unit tests for `VmCompression`, `VmCompressor`, `VmSlotPageStorage` and `VmLz4Compressor`.
#[cfg(ktest)]
#[unittest::suite]
mod compression_rs {
    use crate::vm::compression::{
        StrategyCompressResult, VmCompressedStorage, VmCompression, VmCompressionStrategy,
    };
    use crate::vm::compressor::{CompressResult, CompressedRef, PageAndMetadata};
    use crate::vm::lz4_compressor::VmLz4Compressor;
    use crate::vm::page::VmPagePtr;
    use crate::vm::physmap::paddr_to_physmap;
    use crate::vm::pmm;
    use crate::vm::slot_page_storage::{NUM_SLOTS, SLOT_SIZE, VmSlotPageStorage};
    use core::mem::MaybeUninit;
    use kalloc::Box;
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{assert_ok, assert_true, expect_eq, expect_true, unwrap_ok, unwrap_some};

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

    /// # Safety
    ///
    /// `reference` must be a valid, live reference returned by `storage.store`.
    unsafe fn validate_pattern_ref(
        storage: &VmSlotPageStorage,
        reference: CompressedRef,
        expected_len: usize,
        offset: u64,
    ) -> bool {
        // SAFETY: By caller precondition, `reference` is a valid, live reference of `storage`.
        let (data, _metadata) = unsafe { storage.compressed_data(reference) };
        if data.len() != expected_len {
            return false;
        }
        validate_pattern(data, offset)
    }

    /// # Safety
    ///
    /// `page` must be a valid, exclusively owned page in the `Alloc` state that is not in any list,
    /// with at least `len` (`0 < len <= PAGE_SIZE_USIZE`) initialized bytes.
    unsafe fn store(storage: &VmSlotPageStorage, page: VmPagePtr, len: usize) -> CompressedRef {
        // SAFETY: Discharged by caller's `# Safety` precondition.
        let (maybe_ref, return_page) = unsafe { storage.store(page, len) };
        if let Some(return_page) = return_page {
            // SAFETY: `return_page` is now exclusively owned by us.
            unsafe { pmm::free_page(return_page) };
        }
        assert!(maybe_ref.is_some());
        maybe_ref.unwrap()
    }

    fn store_pattern(storage: &VmSlotPageStorage, len: usize, offset: u64) -> CompressedRef {
        let status = pmm::alloc_page(0);
        assert!(status.is_ok());
        let (page, _) = status.unwrap();

        // SAFETY: `page` was just allocated from the PMM and is exclusively owned.
        unsafe { write_pattern(page, len, offset) };

        // SAFETY: `page` is exclusively owned, in the `Alloc` state, not in any list, and has `len`
        // initialized bytes.
        unsafe { store(storage, page, len) }
    }

    /// Test that slot page storage rounds item sizes to slots and tracks the remainder correctly.
    #[test]
    fn slot_page_storage_size_rounding() {
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();

        const NUM_ITEMS: usize = 5;
        const ITEMS: [usize; NUM_ITEMS] = [64, 128, 63, 1, 65];

        // Store items that are a perfect multiple of the slot size, as well as +/- 1 byte, to
        // ensure that the correct number of slots is used and the remainder is calculated
        // correctly.
        let refs: [CompressedRef; NUM_ITEMS] = [
            store_pattern(&storage, ITEMS[0], 0),
            store_pattern(&storage, ITEMS[1], 1),
            store_pattern(&storage, ITEMS[2], 2),
            store_pattern(&storage, ITEMS[3], 3),
            store_pattern(&storage, ITEMS[4], 4),
        ];

        // So far should have used 7 slots to store our 5 items. Using the remaining 57 slots
        // should not cause us to need a second storage page.
        let padding = store_pattern(&storage, 57 * 64, 5);
        expect_eq!(storage.get_internal_memory_usage().data_bytes, PAGE_SIZE_USIZE);

        for (i, (&r, &len)) in refs.iter().zip(ITEMS.iter()).enumerate() {
            // SAFETY: `r` is a live reference returned by `storage.store`.
            expect_true!(unsafe { validate_pattern_ref(&storage, r, len, i as u64) });
        }

        for r in refs {
            // SAFETY: `r` is a live reference returned by `storage.store` that is not used again.
            unsafe { storage.free(r) };
        }
        // SAFETY: `padding` is a live reference returned by `storage.store` that is not used
        // again.
        unsafe { storage.free(padding) };
    }

    /// Test that slot page storage handles items that need every slot of a page.
    #[test]
    fn slot_page_storage_full_page_items() {
        let storage = VmSlotPageStorage::create();
        assert_true!(storage.is_some());
        let storage = storage.unwrap();

        const NUM_ITEMS: usize = 2;
        // The smallest item that needs every slot, and the largest item that may be stored.
        const ITEMS: [usize; NUM_ITEMS] = [(NUM_SLOTS - 1) * SLOT_SIZE + 1, PAGE_SIZE_USIZE];

        let refs: [CompressedRef; NUM_ITEMS] =
            [store_pattern(&storage, ITEMS[0], 0), store_pattern(&storage, ITEMS[1], 1)];

        // Each item fills an entire page, so they cannot share a storage page.
        expect_eq!(storage.get_internal_memory_usage().data_bytes, NUM_ITEMS * PAGE_SIZE_USIZE);
        expect_eq!(
            storage.get_memory_usage().compressed_storage_used_bytes,
            (ITEMS[0] + ITEMS[1]) as u64
        );

        for (i, (&r, &len)) in refs.iter().zip(ITEMS.iter()).enumerate() {
            // SAFETY: `r` is a live reference returned by `storage.store`.
            expect_true!(unsafe { validate_pattern_ref(&storage, r, len, i as u64) });
        }

        for r in refs {
            // SAFETY: `r` is a live reference returned by `storage.store` that is not used again.
            unsafe { storage.free(r) };
        }

        // Freeing the items should have returned both storage pages to the PMM.
        expect_eq!(storage.get_internal_memory_usage().data_bytes, 0);
        expect_eq!(storage.get_memory_usage().uncompressed_content_bytes, 0);
        expect_eq!(storage.get_memory_usage().compressed_storage_used_bytes, 0);
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

    /// Test that lz4 compression of compressible data can be decompressed back to the original.
    #[test]
    fn lz4_compress_smoke_test() {
        let lz4 = unwrap_some!(VmLz4Compressor::create());

        let mut src = unwrap_some!(Box::<[u8]>::try_new_zeroed_slice(PAGE_SIZE_USIZE).ok());
        let mut compressed = unwrap_some!(Box::<[u8]>::try_new_uninit_slice(PAGE_SIZE_USIZE).ok());
        let mut uncompressed =
            unwrap_some!(Box::<[u8]>::try_new_uninit_slice(PAGE_SIZE_USIZE).ok());

        // Cannot generate random data, as it might not compress, so generate something that would
        // definitely be RLE compressible.
        for (i, byte) in src.iter_mut().enumerate() {
            *byte = ((i / 128) & 0xff) as u8;
        }

        let result = lz4.compress(&src, &mut compressed);
        assert_true!(matches!(result, StrategyCompressResult::Compressed(_)));
        let StrategyCompressResult::Compressed(compressed_size) = result else {
            unreachable!();
        };

        // SAFETY: `compress` initialized the first `compressed_size` bytes of `compressed`.
        let compressed = unsafe { compressed[..compressed_size].assume_init_ref() };
        lz4.decompress(compressed, &mut uncompressed);
        // SAFETY: `decompress` initialized all `PAGE_SIZE_USIZE` bytes of `uncompressed`.
        let uncompressed = unsafe { uncompressed.assume_init() };
        expect_true!(*src == *uncompressed);
    }

    /// Test that lz4 compression detects zero pages, even with a restricted output size.
    #[test]
    fn lz4_zero_dedupe_test() {
        let lz4 = unwrap_some!(VmLz4Compressor::create());

        let mut zero = unwrap_some!(Box::<[u8]>::try_new_zeroed_slice(PAGE_SIZE_USIZE).ok());
        let mut dst = unwrap_some!(Box::<[u8]>::try_new_uninit_slice(PAGE_SIZE_USIZE).ok());

        // Check the zero page is determined to be zero.
        expect_true!(lz4.compress(&zero, &mut dst) == StrategyCompressResult::Zero);

        // Restricting the output size somewhat significantly should not prevent zero detection.
        const MAX_SIZE: usize = if PAGE_SIZE_USIZE == 0x4000 { 128 } else { 64 };
        const {
            assert!(
                PAGE_SIZE_USIZE == 0x1000 || PAGE_SIZE_USIZE == 0x4000,
                "update test to handle 64k pages"
            )
        };
        expect_true!(lz4.compress(&zero, &mut dst[..MAX_SIZE]) == StrategyCompressResult::Zero);

        // Setting a byte should prevent zero detection.
        zero[4] = 1;
        expect_true!(matches!(
            lz4.compress(&zero, &mut dst),
            StrategyCompressResult::Compressed(_)
        ));
    }

    /// Test that lz4 compression fails cleanly, without overrunning `dst`, when it does not fit.
    #[test]
    fn lz4_compress_fail_test() {
        let lz4 = unwrap_some!(VmLz4Compressor::create());

        let mut src = unwrap_some!(Box::<[u8]>::try_new_zeroed_slice(PAGE_SIZE_USIZE).ok());
        let mut dst = unwrap_some!(Box::<[u8]>::try_new_uninit_slice(PAGE_SIZE_USIZE).ok());
        const LIMIT: usize = 8;
        const SENTINEL: u8 = 0xaa;
        dst.fill(MaybeUninit::new(SENTINEL));

        // Compressible, non-zero data that cannot fit in `LIMIT` bytes must fail, and must not
        // write beyond the provided output. `src` contains `PAGE_SIZE_USIZE / 128 >= 32` distinct
        // byte values, each of which must appear as a literal at least once in an LZ4 block, so it
        // can never compress into `LIMIT` bytes.
        for (i, byte) in src.iter_mut().enumerate() {
            *byte = ((i / 128) & 0xff) as u8;
        }
        expect_true!(lz4.compress(&src, &mut dst[..LIMIT]) == StrategyCompressResult::Fail);
        // SAFETY: `dst` was filled with `MaybeUninit::new(SENTINEL)` above and only `dst[..LIMIT]`
        // was passed to `compress`, so `dst[LIMIT..]` remains initialized.
        expect_true!(unsafe { dst[LIMIT..].assume_init_ref() }.iter().all(|&b| b == SENTINEL));

        // A zero page whose compressed form is truncated must report failure, not a zero page.
        src.fill(0);
        expect_true!(lz4.compress(&src, &mut dst[..4]) == StrategyCompressResult::Fail);
    }
}
