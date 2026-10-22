// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::compression::VmCompression;
use crate::vm::page::VmPagePtr;
use crate::vm::vm_page_list::ReferenceValue;
use crate::vm::{physmap, pmm};
use compressor_bindings as bindings;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use zx_status::Status;
use zx_types::zx_status_t;

/// Convenience struct to contain both a page and its associated metadata.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct PageAndMetadata {
    pub page: Option<VmPagePtr>,
    pub metadata: u32,
}

// Compile-time layout assertions against C++ PageAndMetadata.
zr::static_assert!(
    core::mem::size_of::<PageAndMetadata>()
        == core::mem::size_of::<bindings::VmCompressor_PageAndMetadata>()
);
zr::static_assert!(
    core::mem::align_of::<PageAndMetadata>()
        == core::mem::align_of::<bindings::VmCompressor_PageAndMetadata>()
);
zr::static_assert!(
    core::mem::offset_of!(PageAndMetadata, page)
        == core::mem::offset_of!(bindings::VmCompressor_PageAndMetadata, page)
);
zr::static_assert!(
    core::mem::offset_of!(PageAndMetadata, metadata)
        == core::mem::offset_of!(bindings::VmCompressor_PageAndMetadata, metadata)
);

/// Possible results of a compression attempt. See [`VmCompressor::take_compression_result`] for
/// details.
pub type CompressedRef = ReferenceValue;

/// Result tag indicating that an input page could not be compressed or stored, containing the
/// original source page and its most up-to-date metadata.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct FailTag {
    pub src_page: PageAndMetadata,
}

// Compile-time layout assertions against C++ FailTag.
zr::static_assert!(
    core::mem::size_of::<FailTag>() == core::mem::size_of::<bindings::VmCompressor_FailTag>()
);
zr::static_assert!(
    core::mem::align_of::<FailTag>() == core::mem::align_of::<bindings::VmCompressor_FailTag>()
);
zr::static_assert!(
    core::mem::offset_of!(FailTag, src_page)
        == core::mem::offset_of!(bindings::VmCompressor_FailTag, src_page)
);

/// Result of a page compression attempt returned by [`VmCompressor::take_compression_result`].
#[repr(C, u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressResult {
    CompressedRef(CompressedRef),
    Zero,
    Fail(FailTag),
}

impl CompressResult {
    /// Converts this result into its C FFI tag and populates `out_ref` or `out_fail_src_page`.
    #[inline]
    pub(crate) fn into_ffi(self, out_ref: &mut u32, out_fail_src_page: &mut PageAndMetadata) -> u8 {
        match self {
            Self::CompressedRef(r) => {
                *out_ref = r.value();
                0
            }
            Self::Zero => 1,
            Self::Fail(fail) => {
                *out_fail_src_page = fail.src_page;
                2
            }
        }
    }
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum State {
    /// The compressor has been armed and is ready to start compression. The temporary reference
    /// has not been given out, and all fields may be mutated.
    Ready = 0,
    /// The temporary reference has been given out and `page` is non-null. `using_temp_reference`
    /// and `spare_page` will only read/written from the `VmCompression` coordinator at request of
    /// someone who holds the VMO lock. `compression_result` will be written to by us.
    Started = 1,
    /// Using `temp_reference` and `spare_page` may be read/written via `VmCompression`, similar to
    /// the `Started` state, and also by us, provided we hold the VMO lock. `compression_result`
    /// may be read by the holder of the VMO lock.
    Compressed = 2,
    /// Compression has completed, no fields are valid, and needs to be re-armed.
    Finalized = 3,
}

/// The `VmCompressor` represents a single instance of a compression operation that can be
/// performed with a minimal locking protocol between a VMO page owner, and the compressor. There
/// are two different actors that may access and interact with the compressor:
///  1. The owner of the `VmCompressor` instance.
///  2. The holder of the lock to the VMO that owns the temporary reference from this
///     `VmCompressor`.
///
/// The temporary reference exists to allow for the, hopefully unlikely, scenario where another
/// thread wants access to the page whilst compression is in progress, and it works as follows:
///
///  * With the VMO lock held, a `VmPage` in a page list is replaced with the temporary reference
///    of the `VmCompressor`.
///
/// At this point the `VmCompressor` owns the `VmPage`, but the VMO owns the temporary reference.
/// From here any reference to a VMO or the VMO lock refers to the owner of the temporary
/// reference.
///
///  * With the VMO lock *not* held the data in the page is compressed resulting in success or
///    failure.
///
/// During this process the VMO lock was not held, allowing another thread to lookup this page and
/// find the temporary reference. If the temporary reference is found it can treat it like any
/// other compressed reference and decompress it. Decompression of the temporary reference does not
/// have to synchronize on the compression attempt, rather it can just copy page (since the
/// compressor does not modify it).
///
/// Decompression is free to read the page, knowing it cannot be changed, due to holding the VMO
/// lock. This is because for the original thread to progress after performing the compress step it
/// must.
///
///  * Acquire the VMO lock, see if the temporary reference is still there, and resolve any success
///    or failure of the compression.
///
/// In this case if the temporary reference is still there then it is replaced with the compression
/// result (either a compress reference if it succeed, or the original page again if it failed).
///
/// As we are holding the VMO lock we know that either the temporary reference is there, and we
/// hold the lock, so no one else can be using it, or the temporary reference is not there and so
/// it must have been returned, and so no on else can be using it. Therefore the compressor can be
/// reset.
///
/// In order to know that the temporary reference is no longer in use we require that it not move
/// between slots in the page list (or between different page lists). This represents the one time
/// where a compressed reference must be eagerly decompressed even if the data is not being
/// accessed.
///
/// This process of using a temporary reference that can resolve to performing a parallel copy
/// provides some important properties:
///  1. Requesting a page blocks either on decompression, or a memcpy, but never compression.
///  2. Compression is performed on an owned page, and not on a page still in the `VmPageList`,
///     ensuring that the compression algorithm does not have to tolerate mutations during
///     compression.
///
/// The `VmCompressor` also helps to manage the update of metadata associated with the `VmPage`
/// being compressed. The owner of the `VmCompressor` provides the current value of this metadata
/// to [`start`][Self::start] along with the `VmPage`.
///
/// The holder of the lock to the VMO that owns the temporary reference may update this metadata at
/// any time after [`start`][Self::start] is called and prior to
/// [`take_compression_result`][Self::take_compression_result] being called.
///
/// Only the lock holder may call [`take_compression_result`][Self::take_compression_result], so at
/// this time the `VmCompressor` will ensure that the final compressed reference points to the
/// updated metadata (when compression succeeds) or the updated metadata is returned to the caller
/// (when compression fails).
///
/// The updated metadata is also returned to the caller when decompressing a page. In both the
/// cases of compression failure or active decompression, it is the caller's responsibiliy to
/// handle the updated metadata in whatever way it deems appropriate. The meaning of the metadata
/// is opaque to the `VmCompressor`.
#[repr(C)]
pub struct VmCompressor {
    /// Reference to the owning compression system.
    // Safety invariant: Points to a valid, initialized `VmCompression` that owns and outlives
    // `self`.
    compressor: NonNull<VmCompression>,
    /// The value given to us that represent our temporary reference to hand out.
    temp_reference: u32,
    /// Begin in the `Finalized` state to require `arm` to be called initially.
    // Safety invariant: Only read or written by the owner of this `!Sync` `VmCompressor`. When
    // `state` is `State::Ready` or `State::Finalized`, `using_temp_reference` is `false` and the
    // owner has exclusive access to all fields. When `state` is `State::Started` or
    // `State::Compressed`, `page` is `Some` and points to a valid read-only `VmPage`.
    state: UnsafeCell<State>,
    /// Has no load bearing functionality and is only used for assertion checking. Ownership
    /// permissions are the same as for `spare_page`.
    // Safety invariant: Accessed only by the `VmCompressor` owner when idle (`Ready`/`Finalized`)
    // or after the return of the temporary reference has been synchronized via the VMO lock, or by
    // the holder of the VMO lock for the VMO that owns the temporary reference when in the
    // `Started` or `Compressed` state.
    using_temp_reference: UnsafeCell<bool>,
    /// The page we are compressing, owned by us.
    /// In the `Started` and `Compressed` states this field, and the underlying data it points to,
    /// are read-only.
    // Safety invariant: Written only by the `VmCompressor` owner in `start` (from `Ready` state)
    // and `finalize` (after the temporary reference is no longer in use). Read-only in the
    // `Started` and `Compressed` states.
    page: UnsafeCell<Option<VmPagePtr>>,
    /// In the unlikely event that the temporary reference needs to be turned back into a page we
    /// may not be able to perform an allocation. Therefore a spare page is kept around that we can
    /// copy to. As there is only one temporary reference, only one page is needed, and is
    /// refreshed in the `arm` step.
    /// In the `Started` and `Compressed` states this field is wholly owned, for reads and writes,
    /// by the holder of the VMO lock.
    // Safety invariant: Accessed only by the `VmCompressor` owner when idle (`Ready`/`Finalized`),
    // or by the holder of the VMO lock for the VMO that owns the temporary reference when in the
    // `Started` or `Compressed` state.
    spare_page: UnsafeCell<Option<VmPagePtr>>,
    /// Result of the most recent compression attempt.
    /// Read-only, and must only be accessed in the `Compressed` state by the holder of the VMO
    /// lock.
    // Safety invariant: Accessed only by the `VmCompressor` owner (`compress`,
    // `take_compression_result`, and `finalize`).
    compression_result: UnsafeCell<Option<CompressResult>>,
    /// User-provided metadata for the page backing the temp reference.
    /// In the `Started` and `Compressed` states this field is wholly owned, for reads and writes,
    /// by the holder of the VMO lock.
    // Safety invariant: Accessed only by the `VmCompressor` owner when idle (`Ready`/`Finalized`),
    // or by the holder of the VMO lock for the VMO that owns the temporary reference when in the
    // `Started` or `Compressed` state.
    temp_reference_metadata: UnsafeCell<u32>,
}

impl Drop for VmCompressor {
    fn drop(&mut self) {
        // Should not have an in progress compression.
        assert!(self.is_idle());
        // SAFETY: `self.is_idle()` was just asserted above, satisfying the precondition of
        // `is_temp_reference_in_use`.
        assert!(!unsafe { self.is_temp_reference_in_use() });
    }
}

impl VmCompressor {
    /// Creates a new `VmCompressor` owned by `compressor` that hands out `temp_ref` as its
    /// temporary reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `compressor` points to the pinned `VmCompression` that will own
    /// the returned `VmCompressor`, that it is fully initialized before any method that accesses
    /// the owner is called, and that it outlives the returned `VmCompressor`.
    pub(crate) unsafe fn new(compressor: NonNull<VmCompression>, temp_ref: u32) -> Self {
        Self {
            compressor,
            temp_reference: temp_ref,
            state: UnsafeCell::new(State::Finalized),
            using_temp_reference: UnsafeCell::new(false),
            page: UnsafeCell::new(None),
            spare_page: UnsafeCell::new(None),
            compression_result: UnsafeCell::new(None),
            temp_reference_metadata: UnsafeCell::new(0),
        }
    }

    /// Domain-specific conversion: returns raw pointer for `VmCompressor`.
    pub fn as_raw(&self) -> *mut bindings::VmCompressor {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Arms the compressor, ensuring the backup page is allocated. This must be called prior to
    /// [`start`][Self::start].
    pub fn arm(&self) -> Result<(), Status> {
        assert!(self.is_idle());
        // SAFETY: `self.is_idle()` was checked above, `self` is `!Sync`, and `pmm::alloc_page`
        // cannot reenter `self`. By the `state` safety invariant, when idle the temporary
        // reference is not in use and the owner has exclusive access to `spare_page` and `state`.
        unsafe {
            if (*self.spare_page.get()).is_none() {
                // Allocate a new one. Explicitly do not use delayed allocations.
                let (page, _) = pmm::alloc_page(0)?;
                *self.spare_page.get() = Some(page);
            }
            *self.state.get() = State::Ready;
        }
        Ok(())
    }

    /// Start the compression process. Gives ownership of the page to the compressor, and returns
    /// the temporary compression reference that should be installed in the page list in its place.
    /// [`arm`][Self::arm] must be called prior to calling `start`.
    ///
    /// The caller may update the metadata associated with the page at any time after calling
    /// `start` and prior to calling [`take_compression_result`][Self::take_compression_result].
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// 1. The caller owns this `VmCompressor` (in the `Ready` state after [`arm`][Self::arm]) and
    ///    holds the lock for the VMO that will own the returned temporary reference.
    /// 2. `src_page.page` is `Some` and points to a live, initialized `VmPage` owned by the caller
    ///    whose physical page contents remain valid for reads and unmodified until
    ///    [`finalize`][Self::finalize] is called.
    pub unsafe fn start(&self, src_page: PageAndMetadata) -> CompressedRef {
        // SAFETY: We assert `*self.state.get() == State::Ready` below. By the `state` safety
        // invariant, in the `Ready` state the temporary reference has not been given out and the
        // compressor owner has exclusive access to all fields.
        unsafe {
            assert!(*self.state.get() == State::Ready);
            assert!((*self.spare_page.get()).is_some());

            *self.using_temp_reference.get() = true;
            *self.page.get() = src_page.page;
            *self.temp_reference_metadata.get() = src_page.metadata;
            *self.state.get() = State::Started;
        }
        CompressedRef::new(self.temp_reference)
    }

    /// Perform compression. [`start`][Self::start] must have been called prior, and this may be
    /// called without any other locks held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns this `VmCompressor` (in the `Started` state after
    /// calling [`start`][Self::start]), that the `VmPage` passed to [`start`][Self::start] and
    /// its `page::SIZE` bytes in the physmap remain live, valid for reads, and unmodified for the
    /// duration of this call, and that no chain lock transaction is active and the current
    /// thread's lock is not held.
    pub unsafe fn compress(&self) {
        // SAFETY: By the `state` and `page` safety invariants, `state` is exclusively accessed by
        // the compressor owner and `page` is read-only in the `Started` state.
        let page = unsafe {
            assert!(*self.state.get() == State::Started);
            (*self.page.get()).expect("page must be non-null in Started state")
        };
        // SAFETY: By this function's `# Safety` precondition, `page` points to a live `VmPage`.
        let addr = physmap::paddr_to_physmap(unsafe { page.paddr() });
        assert!(addr.0 != 0);

        // SAFETY: `addr` is a non-null physmap address for `page`, which by this function's
        // `# Safety` precondition is valid for reads of `page::SIZE` unmodified bytes for the
        // duration of `self.compressor().compress(...)`, and is owned by this compressor so does
        // not overlap the compression buffer or storage. The caller also guarantees that no chain
        // lock transaction is active and the current thread's lock is not held. By the field
        // safety invariants, `compression_result` and `state` are exclusively accessed by the
        // compressor owner.
        unsafe {
            let result =
                self.compressor().compress(core::ptr::with_exposed_provenance::<u8>(addr.0));
            *self.compression_result.get() = Some(result);
            *self.state.get() = State::Compressed;
        }
    }

    /// Takes the result of the prior compression attempt and ensures that page metadata is
    /// up-to-date. This function may only be called by the holder of the VMO lock, and
    /// [`compress`][Self::compress] must be called prior to calling `take_compression_result`.
    ///
    /// The caller must ensure that there was a prior compression attempt, and thus a result to
    /// take.
    ///
    /// This returns one of:
    ///  - [`CompressResult::CompressedRef`] - Compression was successful and the provided
    ///    reference can be passed to `Decompress` or [`free`][Self::free].
    ///  - [`CompressResult::Zero`] - Input was the zero page. The compressor is not required to
    ///    detect zero pages, and the absence of this value should not be used to assume the input
    ///    was not zero.
    ///  - [`CompressResult::Fail`] - Input could not be compressed or stored. The [`FailTag`]
    ///    contains the original source page provided to [`start`][Self::start] along with the most
    ///    up-to-date metadata for that page (as the metadata may have been altered while
    ///    compression was in-progress).
    ///
    /// If compression succeeded, the returned [`CompressedRef`] will refer to the metadata for the
    /// compressed page, including any metadata updates the caller made after calling
    /// [`start`][Self::start] but before calling `take_compression_result`.
    /// If compression failed, the returned [`FailTag`] will contain the original source page and
    /// the up-to-date metadata.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns this `VmCompressor` (in the `Compressed` state after
    /// calling [`compress`][Self::compress]) and holds the lock for the VMO that owns the
    /// temporary reference for the duration of this call.
    pub unsafe fn take_compression_result(&self) -> CompressResult {
        // SAFETY: By the `state` and `compression_result` safety invariants, both fields are
        // exclusively accessed by the compressor owner.
        let mut ret = unsafe {
            assert!(*self.state.get() == State::Compressed);
            assert!((*self.compression_result.get()).is_some());

            // Invalidate the result after it is retrieved.
            (*self.compression_result.get()).take().unwrap()
        };

        // Ensure any pending modifications to the temp reference's metadata are reflected in the
        // final result used by the caller.
        // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock, which
        // grants exclusive access to `using_temp_reference` and `temp_reference_metadata` and
        // shared read access to `page` in the `Compressed` state. Any `CompressedRef` in `ret` was
        // produced by `self.compressor().compress(...)` and therefore belongs to
        // `self.compressor()`.
        unsafe {
            if self.is_temp_reference_in_use() {
                if let CompressResult::CompressedRef(reference) = &ret {
                    let meta = *self.temp_reference_metadata.get();
                    self.compressor().set_metadata(*reference, meta);
                } else if let CompressResult::Fail(fail) = &mut ret {
                    fail.src_page.page = *self.page.get();
                    fail.src_page.metadata = *self.temp_reference_metadata.get();
                }
            }
        }

        ret
    }

    /// Indicates that the temporary reference is no longer in use, and the compressor is now able
    /// to be re-armed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns this `VmCompressor` (in the `Compressed` state) and
    /// that the temporary reference is no longer in use (with its return synchronized via the VMO
    /// lock).
    pub unsafe fn finalize(&self) {
        // SAFETY: By this function's `# Safety` precondition, the caller owns `self` and the
        // temporary reference is no longer in use, so all fields of `self` are exclusively
        // accessible by the owner.
        unsafe {
            assert!(*self.state.get() == State::Compressed);
            // The temporary reference must no longer be in use.
            assert!(!self.is_temp_reference_in_use());
            assert!((*self.page.get()).is_some());
            *self.page.get() = None;
            *self.compression_result.get() = None;
            *self.state.get() = State::Finalized;
        }
    }

    /// Convenience method to free unused compressed references returned from
    /// [`compress`][Self::compress]. A reference from [`compress`][Self::compress] might be unused
    /// if, after reacquiring locks, a race has occurred and the compression result is stale.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid non-temporary compressed reference
    /// belonging to the owning `VmCompression`, is owned by the caller, and is not used again.
    pub unsafe fn free(&self, reference: CompressedRef) {
        // Forbid returning the temporary reference this way.
        assert!(!self.is_temp_reference(reference));
        // SAFETY: By this function's `# Safety` precondition, `reference` is owned by the caller,
        // is not used again, and belongs to `self.compressor()`, and we asserted above that it is
        // not the temporary reference, discharging `VmCompression::free`.
        unsafe { self.compressor().free(reference) };
    }

    /// Return the temporary reference, indicating it is no longer in use.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the lock for the VMO that owns the temporary reference is held
    /// for the duration of this call.
    pub unsafe fn return_temp_reference(&self, reference: CompressedRef) {
        assert!(self.is_temp_reference(reference));
        // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock, which
        // grants exclusive access to `using_temp_reference` and `temp_reference_metadata`, and
        // shared read access to `page`.
        unsafe {
            assert!(*self.using_temp_reference.get());
            assert!((*self.page.get()).is_some());
            // We must not invalidate `page` here, as this can race with `compress` which reads
            // from `page`. We will invalidate `page` later when the caller reacquires the VMO lock
            // and invokes `finalize`.
            *self.using_temp_reference.get() = false;
            *self.temp_reference_metadata.get() = 0;
        }
    }

    /// Returns whether there is an active compression attempt in progress or not.
    pub fn is_idle(&self) -> bool {
        // SAFETY: By the `state` safety invariant, `state` is only accessed by the owner of this
        // `!Sync` `VmCompressor`.
        let state = unsafe { *self.state.get() };
        state == State::Ready || state == State::Finalized
    }

    /// Tests whether the supplied reference is the temporary reference from this compressor.
    pub fn is_temp_reference(&self, reference: CompressedRef) -> bool {
        reference.value() == self.temp_reference
    }

    /// Returns whether the temporary reference is currently in use or not. This is different from
    /// [`is_idle`][Self::is_idle], as compression could be in progress, but the temporary
    /// reference already returned due to a race.
    ///
    /// Calling this has the same locking requirements as the `spare_page` member, where either the
    /// compressor needs to be idle, or the VMO lock must be held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, the caller holds the lock for
    /// the VMO that owns the temporary reference, or the caller owns the `VmCompressor` after the
    /// return of the temporary reference has been synchronized via the VMO lock.
    pub unsafe fn is_temp_reference_in_use(&self) -> bool {
        // SAFETY: By this function's `# Safety` precondition, the caller satisfies the
        // `using_temp_reference` safety invariant.
        unsafe { *self.using_temp_reference.get() }
    }

    fn compressor(&self) -> &VmCompression {
        // SAFETY: By the `compressor` safety invariant, `self.compressor` points to a valid,
        // initialized `VmCompression` that owns and outlives `self`.
        unsafe { self.compressor.as_ref() }
    }

    /// Returns the page currently being compressed, if any.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, or the caller holds the VMO lock
    /// (or the compressor is in the `Started`/`Compressed` states where `page` is read-only).
    pub(crate) unsafe fn page(&self) -> Option<VmPagePtr> {
        // SAFETY: By this function's `# Safety` precondition, `page` is not concurrently mutated
        // during this read, satisfying the `page` safety invariant.
        unsafe { *self.page.get() }
    }

    /// Returns the spare page, if any.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, or the caller holds the lock for
    /// the VMO that owns the temporary reference.
    pub(crate) unsafe fn spare_page(&self) -> Option<VmPagePtr> {
        // SAFETY: By this function's `# Safety` precondition, either the compressor is idle or the
        // VMO lock is held, satisfying the `spare_page` safety invariant.
        unsafe { *self.spare_page.get() }
    }

    /// Sets the spare page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, or the caller holds the lock for
    /// the VMO that owns the temporary reference.
    pub(crate) unsafe fn set_spare_page(&self, page: Option<VmPagePtr>) {
        // SAFETY: By this function's `# Safety` precondition, either the compressor is idle or the
        // VMO lock is held, granting exclusive access to `spare_page`.
        unsafe {
            *self.spare_page.get() = page;
        }
    }

    /// Returns the user-provided metadata for the page backing the temporary reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, or the caller holds the lock for
    /// the VMO that owns the temporary reference.
    pub(crate) unsafe fn temp_reference_metadata(&self) -> u32 {
        // SAFETY: By this function's `# Safety` precondition, either the compressor is idle or the
        // VMO lock is held, satisfying the `temp_reference_metadata` safety invariant.
        unsafe { *self.temp_reference_metadata.get() }
    }

    /// Sets the user-provided metadata for the page backing the temporary reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either the compressor is idle, or the caller holds the lock for
    /// the VMO that owns the temporary reference.
    pub(crate) unsafe fn set_temp_reference_metadata(&self, metadata: u32) {
        // SAFETY: By this function's `# Safety` precondition, either the compressor is idle or the
        // VMO lock is held, granting exclusive access to `temp_reference_metadata`.
        unsafe {
            *self.temp_reference_metadata.get() = metadata;
        }
    }
}

/// Arms `compressor`, allocating a spare page if needed.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an initialized, idle
/// `VmCompressor` owned by the caller.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_arm(compressor: &VmCompressor) -> zx_status_t {
    Status::result_into_raw(compressor.arm())
}

/// Starts a compression attempt on `src_page` and returns the raw temporary reference value.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an armed `VmCompressor` owned
/// by the caller, and that `src_page` and the VMO lock state satisfy the `# Safety` contract of
/// [`VmCompressor::start`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_start(
    compressor: &VmCompressor,
    src_page: PageAndMetadata,
) -> u32 {
    // SAFETY: By this function's `# Safety` precondition, the caller owns `compressor` in the
    // `Ready` state, holds the VMO lock, and provides a valid read-only `src_page`, discharging
    // `VmCompressor::start`.
    unsafe { compressor.start(src_page).value() }
}

/// Performs the compression step on `compressor`.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to a started `VmCompressor` owned
/// by the caller, and that the rest of the `# Safety` contract of [`VmCompressor::compress`] is
/// upheld.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_compress(compressor: &VmCompressor) {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe {
        compressor.compress();
    }
}

/// Takes the result of the prior compression attempt from `compressor`.
///
/// Returns `0` for `CompressedRef` (writing the reference value to `*out_ref`), `1` for `ZeroTag`,
/// or `2` for `FailTag` (writing the source page and metadata to `*out_fail_src_page`).
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to a compressed `VmCompressor`
/// owned by the caller, that the caller holds the VMO lock for the VMO that owns the temporary
/// reference, and that `out_ref` and `out_fail_src_page` are valid, exclusively borrowed mutable
/// references.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_take_compression_result(
    compressor: &VmCompressor,
    out_ref: &mut u32,
    out_fail_src_page: &mut PageAndMetadata,
) -> u8 {
    // SAFETY: By this function's `# Safety` precondition, the caller owns `compressor` in the
    // `Compressed` state and holds the VMO lock, discharging
    // `VmCompressor::take_compression_result`.
    unsafe { compressor.take_compression_result() }.into_ffi(out_ref, out_fail_src_page)
}

/// Finalizes `compressor` after the temporary reference is no longer in use.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to a compressed `VmCompressor`
/// owned by the caller, and that the temporary reference is no longer in use as required by
/// [`VmCompressor::finalize`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_finalize(compressor: &VmCompressor) {
    // SAFETY: By this function's `# Safety` precondition, the caller owns `compressor` in the
    // `Compressed` state and the temporary reference is no longer in use, discharging
    // `VmCompressor::finalize`.
    unsafe {
        compressor.finalize();
    }
}

/// Frees an unused compressed reference `raw_ref` via `compressor`.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an initialized `VmCompressor`,
/// and that `raw_ref` is a valid non-temporary compressed reference belonging to the owning
/// `VmCompression` that is owned by the caller and not used again.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_free(compressor: &VmCompressor, raw_ref: u32) {
    // SAFETY: By this function's `# Safety` precondition, `raw_ref` is a valid non-temporary
    // compressed reference owned by the caller and not used again, discharging
    // `VmCompressor::free`.
    unsafe {
        compressor.free(CompressedRef::new(raw_ref));
    }
}

/// Returns the temporary reference `raw_ref` to `compressor`.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an initialized `VmCompressor`,
/// and that the caller holds the lock for the VMO that owns the temporary reference.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_return_temp_reference(
    compressor: &VmCompressor,
    raw_ref: u32,
) {
    // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock for the
    // VMO that owns the temporary reference, discharging `VmCompressor::return_temp_reference`.
    unsafe {
        compressor.return_temp_reference(CompressedRef::new(raw_ref));
    }
}

/// Returns whether `raw_ref` is the temporary reference from `compressor`.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an initialized `VmCompressor`.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_is_temp_reference(
    compressor: &VmCompressor,
    raw_ref: u32,
) -> bool {
    compressor.is_temp_reference(CompressedRef::new(raw_ref))
}

/// Returns whether the temporary reference from `compressor` is currently in use.
///
/// # Safety
///
/// The caller must ensure that `compressor` is a valid reference to an initialized `VmCompressor`,
/// and that either the compressor is idle or the caller holds the lock for the VMO that owns the
/// temporary reference.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compressor_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compressor_is_temp_reference_in_use(
    compressor: &VmCompressor,
) -> bool {
    // SAFETY: By this function's `# Safety` precondition, either the compressor is idle or the VMO
    // lock is held, discharging `VmCompressor::is_temp_reference_in_use`.
    unsafe { compressor.is_temp_reference_in_use() }
}
