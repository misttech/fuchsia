// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::relaxed_atomic::{RelaxedAtomicI64, RelaxedAtomicU64};
use crate::kernel::thread;
use crate::platform_rs::timer::{
    DurationMono, InstantMonoTicks, ticks_per_second, timer_current_mono_ticks,
};
use crate::vm::compressor::{
    CompressResult, CompressedRef, FailTag, PageAndMetadata, VmCompressor,
};
use crate::vm::lz4_compressor::VmLz4Compressor;
use crate::vm::page::VmPagePtr;
use crate::vm::slot_page_storage::VmSlotPageStorage;
use crate::vm::vm::{vm_ktrace_duration, vm_ktrace_duration_begin, vm_ktrace_duration_end};
use crate::vm::{physmap, pmm};
use boot_options::{BootOptions, CompressionStorageStrategy, CompressionStrategy};
use compression_bindings as bindings;
use core::array;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{Recyclable, RefCounted, RefPtr, ref_counted};
use kalloc::Box;
use kprint::kprintln;
use ksync::{KMutex, KMutexGuard, RawMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;

define_kcounter!(PAGES_DECOMPRESSED, "vm.reclamation.pages_decompressed", Sum);

/// Number of logarithmic second buckets used to track how long pages remain compressed before
/// decompression.
pub const NUM_LOG_BUCKETS: usize = 8;

/// Memory usage statistics for [`VmCompressedStorage`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryUsage {
    pub uncompressed_content_bytes: u64,
    pub compressed_storage_bytes: u64,
    pub compressed_storage_used_bytes: u64,
}

/// Statistics for [`VmCompression`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub memory_usage: MemoryUsage,
    pub compression_time: DurationMono,
    pub decompression_time: DurationMono,
    pub total_page_compression_attempts: u64,
    pub failed_page_compression_attempts: u64,
    pub total_page_decompressions: u64,
    pub compressed_page_evictions: u64,
    pub pages_decompressed_within_log_seconds: [u64; NUM_LOG_BUCKETS],
}

/// Defines an allocator-like interface for adding, removing, and accessing compressed data.
/// Instances of this trait may be accessed concurrently.
///
/// # Safety
///
/// Implementers must ensure that:
/// 1. If [`store`][Self::store] returns `(_, Some(returned_page))`, `returned_page` is a valid,
///    uniquely owned PMM page in the `Alloc` state that is not in any list, and whose physmap
///    mapping is valid for reads and exclusive writes of `page::SIZE` initialized bytes.
/// 2. If [`store`][Self::store] returns `(Some(reference), _)`, `reference` is a valid, live
///    [`CompressedRef`] for `self` (and not equal to [`VmCompression::TEMP_REFERENCE_VALUE`])
///    that remains valid for [`compressed_data`][Self::compressed_data],
///    [`get_metadata`][Self::get_metadata], [`set_metadata`][Self::set_metadata], and
///    [`free`][Self::free] until freed.
/// 3. For a valid, live `reference`, [`compressed_data`][Self::compressed_data] returns a slice
///    whose pointer is valid for reads of its full length, whose contents are not modified until
///    `reference` is passed to [`free`][Self::free], and which does not overlap any page owned by
///    the caller (including pages returned from [`store`][Self::store]).
pub unsafe trait VmCompressedStorage: Send + Sync {
    /// Attempts to store the data in `page` of size `len`. The data is assumed to start at offset
    /// 0 in the page, and `len` cannot exceed `page::SIZE`. The `page` becomes owned by this
    /// `VmCompressedStorage` instance, although ownership may be returned via the return result.
    ///
    /// The return value is an optional reference to compressed data, as well as an optional
    /// `VmPagePtr`, with `None` for the `VmPagePtr` being used to indicate absence. If a
    /// `VmPagePtr` is returned the caller owns it, and is responsible for freeing it.
    /// If the data is stored successfully then a valid [`CompressedRef`] will be returned and will
    /// be retained until that reference is passed to [`free`][Self::free]. Otherwise `None` is
    /// returned.
    ///
    /// Regardless of the success or failure of the storage, a `VmPagePtr` might be returned, and
    /// if one is returned it may or may not be the same as the `page` passed in. The returned
    /// `VmPagePtr` has undefined content and is now freely owned by the caller and can be used as
    /// the caller likes, or returned to the PMM.
    /// The expected usage of this is that a compressor puts data in a buffer page and passes it
    /// here as `page`, then takes any returned `VmPagePtr` as its next buffer page, or if none was
    /// returned allocates a new one. This provides the storage implementation with the freedom to
    /// either copy the data out of `page` and return it, or take the page to add to its storage
    /// instead of going to the PMM (and then returning `None`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `page` is a valid, uniquely owned page in the `Alloc` state
    /// that is not in any list, that `0 < len <= page::SIZE`, and that the physmap mapping of
    /// `page` contains at least `len` initialized bytes starting at offset 0.
    unsafe fn store(
        &self,
        page: VmPagePtr,
        len: usize,
    ) -> (Option<CompressedRef>, Option<VmPagePtr>);

    /// Free the data referenced by a [`CompressedRef`] that was returned from
    /// [`store`][Self::store]. After calling this the reference is no longer valid and must not be
    /// passed to [`compressed_data`][Self::compressed_data], and any previous result of
    /// [`compressed_data`][Self::compressed_data] must not be used.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] previously
    /// returned by [`store`][Self::store] on this storage instance, and that `reference` and any
    /// slice returned by [`compressed_data`][Self::compressed_data] for it are not used during or
    /// after this call.
    unsafe fn free(&self, reference: CompressedRef);

    /// Retrieve a reference to original data that was stored. The metadata and length of the data
    /// are also returned, alleviating the need to retain them separately.
    ///
    /// The returned slice remains valid as long as this specific reference is not freed, and
    /// otherwise any other calls to [`store`][Self::store] or [`free`][Self::free] do not
    /// invalidate.
    /// TODO(https://fxbug.dev/42138396): Restrict this if de-fragmentation of the compressed
    /// storage becomes necessary.
    ///
    /// No guarantee on alignment of the data is provided, and callers must tolerate arbitrary byte
    /// alignment.
    /// TODO(https://fxbug.dev/42138396): Consider providing an alignment guarantee if needed by
    /// decompressors.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] previously
    /// returned by [`store`][Self::store] on this storage instance, and that `reference` is not
    /// freed for the lifetime of the returned slice.
    unsafe fn compressed_data(&self, reference: CompressedRef) -> (&[u8], u32);

    /// Retrieve the metadata for the original page referred to by `reference`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] previously
    /// returned by [`store`][Self::store] on this storage instance and that access is synchronized
    /// with any concurrent [`set_metadata`][Self::set_metadata] or [`free`][Self::free] calls for
    /// `reference`.
    unsafe fn get_metadata(&self, reference: CompressedRef) -> u32;

    /// Set the metadata for the original page referred to by `reference`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] previously
    /// returned by [`store`][Self::store] on this storage instance and that access is exclusively
    /// synchronized with any concurrent metadata or [`free`][Self::free] calls for `reference`.
    unsafe fn set_metadata(&self, reference: CompressedRef, metadata: u32);

    /// Perform an information dump of the internal state to the debuglog.
    fn dump(&self);

    /// Returns memory usage statistics for this compressed storage instance.
    fn get_memory_usage(&self) -> MemoryUsage;
}

/// Result of a [`VmCompressionStrategy::compress`] attempt.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum StrategyCompressResult {
    /// Compression was successful and this value is `<= dst.len()` and represents the length of
    /// data stored to `dst`.
    Compressed(usize),
    /// The input data was noticed to be all zeroes.
    Zero,
    /// The input data could not be compressed within `dst.len()`.
    Fail,
}

/// Defines the interface for different compression algorithms.
/// Instances of this trait may be accessed concurrently.
///
/// # Safety
///
/// Implementers must ensure that:
/// 1. If [`compress`][Self::compress] returns [`StrategyCompressResult::Compressed`], the returned
///    size is `<= dst.len()` and the first `size` bytes of `dst` have been initialized.
/// 2. When [`decompress`][Self::decompress] returns, all `page::SIZE` bytes of `dst` have been
///    initialized.
pub unsafe trait VmCompressionStrategy: Send + Sync {
    /// Attempt to compress the data at `src` into `dst`. The input data is assumed to be
    /// `page::SIZE` in length, and the amount of output data is constrained by `dst.len()`.
    /// `compress` will never write more than `dst.len()` bytes to `dst`.
    ///
    /// The return value is one of:
    ///  - [`StrategyCompressResult::Compressed`]: Compression was successful and the contained
    ///    value is `<= dst.len()` and represents the length of data stored to `dst`.
    ///  - [`StrategyCompressResult::Zero`]: The input data was noticed to be all zeroes.
    ///  - [`StrategyCompressResult::Fail`]: The input data could not be compressed within
    ///    `dst.len()`.
    ///
    /// No guarantee on the alignment of `src` or `dst` is provided, and the caller is free to
    /// provide slices with arbitrary byte alignment.
    /// TODO(https://fxbug.dev/42138396): Consider requiring an alignment if needed by compressors.
    fn compress(&self, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> StrategyCompressResult;

    /// Decompress the data at `src` into `dst`. The input is assumed to have been produced by
    /// [`compress`][Self::compress] and this method cannot fail. Similar to the input of
    /// [`compress`][Self::compress] being assumed to be a page size, the `dst` output here is
    /// assumed to be of `page::SIZE` and will be exactly filled.
    ///
    /// No guarantee on the alignment of `src` or `dst` is provided, and the caller is free to
    /// provide slices with arbitrary byte alignment.
    /// TODO(https://fxbug.dev/42138396): Consider requiring an alignment if needed by
    /// decompressors.
    fn decompress(&self, src: &[u8], dst: &mut [MaybeUninit<u8>]);

    /// Perform an information dump of the internal state to the debuglog.
    fn dump(&self);
}

/// We always add `InstantMonoTicks` to any data that we store, so ensure that the maximum
/// size of the compressed data combined with that would not require us to store more than a page.
const fn ensure_threshold(threshold: usize) -> usize {
    let max_threshold = page::SIZE - core::mem::size_of::<InstantMonoTicks>();
    if threshold > max_threshold { max_threshold } else { threshold }
}

fn bucket_for_ticks(start: InstantMonoTicks, end: InstantMonoTicks) -> usize {
    // Turn the ticks range into seconds. As we want whole seconds we are happy to tolerate the
    // rounding behavior of integer division here.
    let seconds: u64 =
        if end > start { ((end.0 - start.0) / ticks_per_second()) as u64 } else { 0 };
    // Calculate the bucket and return it, clamping to the maximum number of buckets.
    let log2 = seconds.checked_ilog2().unwrap_or(0) as usize;
    core::cmp::min(log2, NUM_LOG_BUCKETS - 1)
}

/// An RAII wrapper around holding a locked reference to a [`VmCompressor`].
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct CompressorGuard<'a> {
    /// Guard that keeps the instance owned by us, must never be released for the lifetime of this
    /// object and the reference to `instance`.
    #[pin]
    instance_guard: KMutexGuard<'a, VmCompressionInstanceLockClass, RawMutex>,
    instance: &'a VmCompressor,
}

// Compile-time layout assertions for CompressorGuard against C++ VmCompression::CompressorGuard.
zr::static_assert!(
    core::mem::size_of::<CompressorGuard<'_>>()
        == core::mem::size_of::<bindings::VmCompression_CompressorGuard>()
);
zr::static_assert!(
    core::mem::align_of::<CompressorGuard<'_>>()
        == core::mem::align_of::<bindings::VmCompression_CompressorGuard>()
);

#[pinned_drop]
impl PinnedDrop for CompressorGuard<'_> {
    fn drop(self: Pin<&mut Self>) {
        // Ensure the compressor instance was not left in an in progress state before returning it.
        assert!(self.instance.is_idle());
    }
}

impl<'a> CompressorGuard<'a> {
    /// Return a reference to the [`VmCompressor`]. Reference must not outlive this object.
    #[inline]
    pub fn get(&self) -> &VmCompressor {
        self.instance
    }
}

/// Top level container for performing compression and is responsible for coordinating between the
/// provided storage and compression strategies. Each `VmCompression` instance has its own
/// [`CompressedRef`] namespace, and references are not transferable. Aside from testing it is
/// expected that there be a single global instance.
///
/// This also manages the [`VmCompressor`] instances that provide the state machine for a VMO to do
/// compression. Currently only a single instance is supported limiting to one simultaneous
/// compression.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmCompression {
    /// References to the backing storage and compression strategies.
    storage: Pin<Box<dyn VmCompressedStorage>>,
    strategy: Pin<Box<dyn VmCompressionStrategy>>,
    /// Pages must compress to less than or equal to this threshold for compression to be
    /// considered a success. The largest amount we might need to store is larger than this, as
    /// this threshold does not include the 8 bytes we add on as a timestamp for when a page was
    /// compressed.
    compression_threshold: usize,

    #[mutex]
    instance_lock: KMutex<RawMutex>,
    /// The compressor instance has a more complicated locking structure than can be expressed with
    /// annotations here. The `instance_lock` is used to control vending this out in
    /// [`acquire_compressor`][Self::acquire_compressor] to ensure it is only owned by one thread
    /// at a time, however certain mutation of the compressor requires holding the VMO lock of the
    /// relevant page, and this allows for usage with just holding the VMO lock and not the
    /// `instance_lock`. See [`VmCompressor`] for more details on what fields may be read/written
    /// with which locks held.
    instance: VmCompressor,

    /// Lock for compression state. In practice this should never be contended, since presently
    /// only a single [`VmCompressor`] is supported.
    #[mutex]
    compression_lock: KMutex<RawMutex>,
    /// The `buffer_page` is used as the destination for compressing any input page. To avoid going
    /// to and from the PMM every compression attempt we attempt to reuse a single buffer page as
    /// much as possible.
    // Safety invariant: If `Some(page)`, `page` is a valid, uniquely owned PMM-allocated page in
    // the `Alloc` state that is not in any list, and whose physmap mapping is valid for reads and
    // exclusive writes of `page::SIZE` initialized bytes.
    #[guarded_by(compression_lock)]
    buffer_page: Option<VmPagePtr>,

    /// Statistics counters. These are standalone diagnostic/telemetry counters that do not
    /// synchronize access to any other state, so relaxed atomic operations are sufficient.
    compression_time: RelaxedAtomicI64,
    decompression_time: RelaxedAtomicI64,
    compression_attempts: RelaxedAtomicU64,
    compression_success: RelaxedAtomicU64,
    compression_zero_page: RelaxedAtomicU64,
    compression_fail: RelaxedAtomicU64,
    decompressions: RelaxedAtomicU64,
    decompression_skipped: RelaxedAtomicU64,
    decompressions_within_log_seconds: [RelaxedAtomicU64; NUM_LOG_BUCKETS],
}

// Verify `ref_count` is at offset 0 so C++ `fbl::RefPtr<VmCompression>` and Rust
// `fbl::RefPtr<VmCompression>` have identical pointer representations.
zr::static_assert!(core::mem::offset_of!(VmCompression, ref_count) == 0);

// SAFETY: `VmCompression` owns no thread-affine state: the `kalloc::Box`es holding `storage` and
// `strategy` have no `Send` impls, but each uniquely owns its pointee, and the pointees
// `dyn VmCompressedStorage` and `dyn VmCompressionStrategy` are `Send` by supertrait bound. The
// `buffer_page` is a PMM page that may be freed from any thread, and `instance` only refers back to
// this `VmCompression` through a pointer whose validity does not depend on the current thread.
unsafe impl Send for VmCompression {}
// SAFETY: `VmCompression` is `!Sync` because `instance: VmCompressor` is `!Sync`, and because the
// `kalloc::Box`es holding `storage` and `strategy` have no `Sync` impls. Sharing it is sound
// because `instance` is private and is only reachable either through a `CompressorGuard`, which
// holds `instance_lock` and so gives one owner at a time, or through the temporary reference
// methods, which only access the fields of `instance` that the VMO lock protects (as required by
// their `# Safety` preconditions). `buffer_page` is guarded by `compression_lock`, `storage` and
// `strategy` only give shared access to pointees that are `Sync` by supertrait bound, and the
// statistics are atomics.
unsafe impl Sync for VmCompression {}

#[pinned_drop]
impl PinnedDrop for VmCompression {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if let Some(page) = this.buffer_page.get_inner_mut().take() {
            // SAFETY: By the `buffer_page` safety invariant, `page` is a valid, uniquely owned
            // PMM-allocated page in the `Alloc` state.
            unsafe { pmm::free_page(page) };
        }
    }
}

impl VmCompression {
    /// Number of logarithmic second buckets used to track how long pages remain compressed before
    /// decompression.
    pub const NUM_LOG_BUCKETS: usize = NUM_LOG_BUCKETS;

    /// Currently only a single [`VmCompressor`] instance is supported, so only a single temporary
    /// reference is needed.
    /// It is the responsibility of each underlying storage to not return this.
    const TEMP_REFERENCE_VALUE: u32 = !((1u32 << CompressedRef::ALIGN_BITS) - 1);

    /// Constructs a compression manager using the given storage and compression strategies. The
    /// `compression_threshold` is the number of bytes above which a compression is considered to
    /// have failed and should not be considered worth storing.
    /// The `strategy` is taken as `Pin<Box<C>>`, like `storage`, since strategies may hold pinned
    /// state such as a `KMutex`.
    /// TODO(https://fxbug.dev/42138396): Limit total amount of pages stored.
    pub fn new<S, C>(
        storage: Pin<Box<S>>,
        strategy: Pin<Box<C>>,
        compression_threshold: usize,
    ) -> Result<RefPtr<Self>, Status>
    where
        S: VmCompressedStorage + 'static,
        C: VmCompressionStrategy + 'static,
    {
        // SAFETY: The storage is not moved out of its heap allocation, which is pinned again
        // below.
        let boxed_storage = unsafe { Pin::into_inner_unchecked(storage) };
        let raw_storage: *mut dyn VmCompressedStorage = Box::<S>::into_raw(boxed_storage);
        // SAFETY: `raw_storage` was just produced by `Box::<S>::into_raw` from a `kalloc::Box`.
        let storage: Box<dyn VmCompressedStorage> = unsafe { Box::from_raw(raw_storage) };
        // SAFETY: The contents of `storage` were pinned when passed in and have not been moved.
        let storage = unsafe { Pin::new_unchecked(storage) };

        // SAFETY: The strategy is not moved out of its heap allocation, which is pinned again
        // below.
        let boxed_strategy = unsafe { Pin::into_inner_unchecked(strategy) };
        let raw_strategy: *mut dyn VmCompressionStrategy = Box::<C>::into_raw(boxed_strategy);
        // SAFETY: `raw_strategy` was just produced by `Box::<C>::into_raw` from a `kalloc::Box`.
        let strategy: Box<dyn VmCompressionStrategy> = unsafe { Box::from_raw(raw_strategy) };
        // SAFETY: The contents of `strategy` were pinned when passed in and have not been moved.
        let strategy = unsafe { Pin::new_unchecked(strategy) };

        let compression_threshold = ensure_threshold(compression_threshold);
        // Ensure we can steal space to store the compression timestamp.
        assert!(compression_threshold + core::mem::size_of::<InstantMonoTicks>() <= page::SIZE);

        // SAFETY: The closure initializes all fields of `*slot` in-place via `pin_init!` and
        // returns `Ok(())`.
        let init = unsafe {
            pin_init::pin_init_from_closure(
                move |slot: *mut Self| -> Result<(), core::convert::Infallible> {
                    let owner = NonNull::new(slot).expect("heap allocation must be non-null");
                    let inner = pin_init!(Self {
                        ref_count: RefCounted::new(),
                        __fbl_ref_counted_guard: (),
                        storage,
                        strategy,
                        compression_threshold,
                        instance_lock <- KMutex::init(),
                        // Currently only a single VmCompressor instance is supported, so only a
                        // single temporary reference value (the reserved value) is needed.
                        // SAFETY: `owner` points to the pinned heap allocation of this
                        // `VmCompression`, which owns `instance`, is fully initialized before the
                        // returned `RefPtr` (and so `instance`) can be used, and outlives
                        // `instance` as a field of it.
                        instance: VmCompressor::new(owner, Self::TEMP_REFERENCE_VALUE),
                        compression_lock <- KMutex::init(),
                        buffer_page: None.into(),
                        compression_time: RelaxedAtomicI64::new(0),
                        decompression_time: RelaxedAtomicI64::new(0),
                        compression_attempts: RelaxedAtomicU64::new(0),
                        compression_success: RelaxedAtomicU64::new(0),
                        compression_zero_page: RelaxedAtomicU64::new(0),
                        compression_fail: RelaxedAtomicU64::new(0),
                        decompressions: RelaxedAtomicU64::new(0),
                        decompression_skipped: RelaxedAtomicU64::new(0),
                        decompressions_within_log_seconds: array::from_fn(|_| {
                            RelaxedAtomicU64::new(0)
                        }),
                    });
                    inner.__pinned_init(slot)
                },
            )
        };
        RefPtr::pin_init(init).map_err(|_: kalloc::AllocError| Status::NO_MEMORY)
    }

    /// Construct a `VmCompression` instance using default options for the storage and compression
    /// strategies.
    pub fn create_default() -> Option<RefPtr<Self>> {
        let boot_options = BootOptions::get();
        // See if we even have a strategy.
        if (boot_options.compression_strategy == CompressionStrategy::None)
            || (boot_options.compression_storage_strategy == CompressionStorageStrategy::None)
        {
            // It is an error to only have one of a storage or compressor strategy set.
            if (boot_options.compression_strategy == CompressionStrategy::None)
                ^ (boot_options.compression_storage_strategy == CompressionStorageStrategy::None)
            {
                kprintln!(
                    "ERROR: Exactly one of kernel.compression.strategy and \
                     kernel.compression.storage-strategy was defined"
                );
            }
            return None;
        }

        let storage = match boot_options.compression_storage_strategy {
            CompressionStorageStrategy::Slot => {
                let Some(storage) = VmSlotPageStorage::create() else {
                    kprintln!("[ZRAM]: Failed to create slot compressed storage area");
                    return None;
                };
                kprintln!("[ZRAM]: Using compressed storage strategy: slot");
                storage
            }
            CompressionStorageStrategy::None => {
                // Original check should have handled this.
                panic!("Unreachable");
            }
        };

        if boot_options.compression_threshold == 0 || boot_options.compression_threshold > 100 {
            panic!("ERROR: kernel.compression.threshold must be between 1 and 100");
        }

        let threshold: u32 = (page::SIZE as u32) * boot_options.compression_threshold / 100u32;

        let strategy = match boot_options.compression_strategy {
            CompressionStrategy::Lz4 => {
                let Some(strategy) = VmLz4Compressor::create() else {
                    kprintln!("[ZRAM]: Failed to create lz4 compressor");
                    return None;
                };
                kprintln!("[ZRAM]: Using compression strategy: lz4");
                strategy
            }
            CompressionStrategy::None => {
                // Original check should have handled this.
                panic!("Unreachable");
            }
        };

        let Ok(compression) = Self::new(storage, strategy, threshold as usize) else {
            kprintln!("[ZRAM]: Failed to create compressor");
            return None;
        };
        Some(compression)
    }

    /// Domain-specific conversion: returns raw pointer for `bindings::VmCompression`.
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::VmCompression {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Retrieve a reference to a [`VmCompressor`], wrapped in the RAII [`CompressorGuard`]. Once
    /// the compressor is finished with it can be destroyed, which will release it for reuse.
    /// This method may block until a compressor becomes available and callers should be prepared
    /// for extended wait times.
    /// The returned [`CompressorGuard`] must not outlive this object.
    pub fn acquire_compressor(&self) -> impl PinInit<CompressorGuard<'_>> {
        pin_init!(CompressorGuard {
            instance_guard <- self.instance_lock.lock(),
            instance: &self.instance,
        })
    }

    /// Attempts to compress the page of data at `page_src`, which is assumed to be `page::SIZE`.
    /// This returns one of:
    ///  - [`CompressResult::CompressedRef`] - Compression was successful and the provided
    ///    reference can be passed to [`decompress`][Self::decompress] or [`free`][Self::free].
    ///  - [`CompressResult::Zero`] - Input was the zero page. The compressor is not required to
    ///    detect zero pages, and the absence of this value should not be used to assume the input
    ///    was not zero.
    ///  - [`CompressResult::Fail`] - Input could not be compressed or stored.
    ///
    /// TODO(https://fxbug.dev/42138396): Should different failures be exposed here?
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// 1. `page_src` is a non-null pointer valid for reads of `page::SIZE` initialized bytes that
    ///    are not concurrently modified for the duration of this call, and that do not overlap the
    ///    internal buffer page or any compressed storage.
    /// 2. No chain lock transaction is active and the current thread's lock is not held, as this
    ///    function may block and measures the current thread's runtime.
    pub unsafe fn compress(&self, page_src: *const u8) -> CompressResult {
        vm_ktrace_duration!(2, "compress_page");
        // Take the compression lock so we can use the buffer_page.
        ksync::lock!(let mut guard = self.lock_compression_lock());
        let buffer_page_slot = guard.as_mut().buffer_page_mut();

        // Ensure buffer page exists.
        let buffer_page = match *buffer_page_slot {
            Some(page) => page,
            None => {
                // Explicitly do not use delayed allocation since we might be under memory
                // pressure.
                let Ok((page, _)) = pmm::alloc_page(0) else {
                    return CompressResult::Fail(FailTag::default());
                };
                *buffer_page_slot = Some(page);
                page
            }
        };

        self.compression_attempts.fetch_add(1);

        // Compress into the buffer page, measuring the time taken to do so.
        // SAFETY: By this function's `# Safety` precondition, no chain lock transaction is active
        // and the current thread's lock is not held.
        let start_runtime = unsafe { thread::current_runtime() };
        // SAFETY: By the `buffer_page` safety invariant, `buffer_page` is a valid allocated PMM
        // page exclusively owned under `compression_lock`.
        let buffer_addr = physmap::paddr_to_physmap(unsafe { buffer_page.paddr() });
        assert!(buffer_addr.0 != 0);
        let buffer_ptr = core::ptr::with_exposed_provenance_mut::<MaybeUninit<u8>>(buffer_addr.0);
        // The slices over the source and buffer pages are confined to this block so that they
        // cannot be used after ownership of the buffer page is passed to the storage below.
        let storage_size = {
            // SAFETY: By this function's `# Safety` precondition, `page_src` is valid for reads of
            // `page::SIZE` initialized bytes and does not overlap the buffer page. By the
            // `buffer_page` safety invariant, `buffer_ptr` points to `page::SIZE` bytes in the
            // physmap that are valid for exclusive writes while `compression_lock` is held.
            let (src_slice, buffer_slice) = unsafe {
                (
                    core::slice::from_raw_parts(page_src, page::SIZE),
                    core::slice::from_raw_parts_mut(buffer_ptr, page::SIZE),
                )
            };
            let result =
                self.strategy.compress(src_slice, &mut buffer_slice[..self.compression_threshold]);
            // SAFETY: By this function's `# Safety` precondition, no chain lock transaction is
            // active and the current thread's lock is not held.
            let end_runtime = unsafe { thread::current_runtime() };
            if end_runtime > start_runtime {
                self.compression_time.fetch_add((end_runtime - start_runtime).into_nanos());
            }

            // The result is a different type so we need to convert, and it gives us a chance to
            // record statistics.
            let compressed_size = match result {
                StrategyCompressResult::Fail => {
                    self.compression_fail.fetch_add(1);
                    return CompressResult::Fail(FailTag::default());
                }
                StrategyCompressResult::Zero => {
                    self.compression_zero_page.fetch_add(1);
                    return CompressResult::Zero;
                }
                StrategyCompressResult::Compressed(size) => size,
            };

            // Have actual data to store.
            debug_assert!(compressed_size > 0 && compressed_size <= self.compression_threshold);

            // Store the current ticks for tracking how long pages remain compressed. We had
            // previously validated in the constructor that we would always have space on the page.
            let storage_size = compressed_size + core::mem::size_of::<InstantMonoTicks>();
            debug_assert!(storage_size <= page::SIZE);
            let now = timer_current_mono_ticks();
            // This slice operation is bounds checked, and so also guarantees that
            // `storage_size <= page::SIZE` even if the strategy returned an invalid size.
            buffer_slice[compressed_size..storage_size]
                .copy_from_slice(&now.0.to_ne_bytes().map(MaybeUninit::new));
            storage_size
        };

        // Store the data, it takes ownership of the buffer_page and might return ownership of a
        // page.
        // Metadata associated with the page will be stored later, when the caller reacquires the
        // VMO lock and requests compression results.
        let buffer_page = buffer_page_slot.take().unwrap();
        // SAFETY: By the `buffer_page` safety invariant, `buffer_page` is a valid, uniquely owned
        // allocated page in the `Alloc` state that is not in any list. By
        // `VmCompressionStrategy::compress`'s safety postcondition, the first `compressed_size`
        // bytes of `buffer_page` were initialized, and the timestamp write above initialized
        // `compressed_size..storage_size`, so `buffer_page` contains `storage_size` initialized
        // bytes starting at offset 0. `storage_size` includes the timestamp so is non-zero, and
        // `storage_size <= page::SIZE` was guaranteed by the bounds checked timestamp write above.
        // By `VmCompressedStorage::store`'s safety postcondition, any returned `page` satisfies
        // the `buffer_page` safety invariant.
        let (maybe_ref, page) = unsafe { self.storage.store(buffer_page, storage_size) };
        *buffer_page_slot = page;

        if let Some(reference) = maybe_ref {
            // Make sure the storage system never produced the temp reference.
            assert!(!self.is_temp_reference(reference));
            self.compression_success.fetch_add(1);
            return CompressResult::CompressedRef(reference);
        }
        self.compression_fail.fetch_add(1);
        CompressResult::Fail(FailTag::default())
    }

    /// Decompresses and frees the provided reference into `page_dest` and `metadata_dest`. This
    /// cannot fail and always produces `page::SIZE` worth of data. After calling this the
    /// reference is no longer valid.
    ///
    /// Note that the temporary reference may be passed into here, however the same locking
    /// requirements as [`move_reference`][Self::move_reference] must be observed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// 1. `reference` is a valid, live [`CompressedRef`] belonging to this `VmCompression`
    ///    instance that is owned by the caller and not used again after this call.
    /// 2. If `reference` is the temporary reference, the caller holds the VMO lock for the VMO
    ///    that owns the temporary reference.
    /// 3. `page_dest` is a non-null pointer valid for exclusive writes of `page::SIZE` bytes for
    ///    the duration of this call, that does not overlap any compressed storage or the page
    ///    backing the temporary reference.
    /// 4. If `reference` is not the temporary reference, no chain lock transaction is active and
    ///    the current thread's lock is not held, as the current thread's runtime is measured.
    pub unsafe fn decompress(
        &self,
        reference: CompressedRef,
        page_dest: *mut u8,
        metadata_dest: &mut u32,
    ) {
        vm_ktrace_duration_begin!(2, "decompress_page");
        if self.is_temp_reference(reference) {
            // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock
            // for the temporary reference and `page_dest` is valid for exclusive writes of
            // `page::SIZE` bytes.
            unsafe { self.decompress_temp_reference(reference, page_dest, metadata_dest) };
            return;
        }

        PAGES_DECOMPRESSED.add(1);
        self.decompressions.fetch_add(1);

        let now = timer_current_mono_ticks();

        // The slices over the compressed data and destination page are confined to this block so
        // that they cannot be used after the reference is freed below.
        let compressed_ticks = {
            // Lookup the data so we can decompress out.
            // SAFETY: By this function's `# Safety` precondition, `reference` is a valid
            // non-temporary compressed reference belonging to `self.storage` and is not freed
            // until after this block finishes reading `src`.
            let (src, metadata) = unsafe { self.storage.compressed_data(reference) };
            *metadata_dest = metadata;

            // Pull out the timestamp and determine how long this was compressed for.
            let ts_size = core::mem::size_of::<InstantMonoTicks>();
            debug_assert!(src.len() >= ts_size);
            let payload_len = src.len() - ts_size;
            let compressed_ticks =
                InstantMonoTicks(i64::from_ne_bytes(src[payload_len..].try_into().unwrap()));
            let bucket = bucket_for_ticks(compressed_ticks, now);
            self.decompressions_within_log_seconds[bucket].fetch_add(1);

            // Decompress the data, excluding our timestamp, and measure how long decompression
            // takes.
            // SAFETY: By this function's `# Safety` precondition, `page_dest` is non-null, valid
            // for exclusive writes of `page::SIZE` bytes for the duration of this call, and does
            // not overlap `src`.
            let dst_slice = unsafe {
                core::slice::from_raw_parts_mut(page_dest.cast::<MaybeUninit<u8>>(), page::SIZE)
            };
            // SAFETY: By this function's `# Safety` precondition, as `reference` is not the
            // temporary reference, no chain lock transaction is active and the current thread's
            // lock is not held.
            let start_runtime = unsafe { thread::current_runtime() };
            self.strategy.decompress(&src[..payload_len], dst_slice);
            // SAFETY: As above.
            let end_runtime = unsafe { thread::current_runtime() };
            if end_runtime > start_runtime {
                self.decompression_time.fetch_add((end_runtime - start_runtime).into_nanos());
            }
            compressed_ticks
        };

        // Now that decompression is finished, free the backing memory.
        // SAFETY: By this function's `# Safety` precondition, `reference` is owned by the caller
        // and not used again, and `src` has gone out of scope.
        unsafe { self.storage.free(reference) };
        vm_ktrace_duration_end!(
            2,
            "decompress_page",
            "compressed_time_s" => (now.0 - compressed_ticks.0) / ticks_per_second()
        );
    }

    /// Free the compressed reference without decompressing it.
    ///
    /// Note that the temporary reference may be passed into here, however the same locking
    /// requirements as [`move_reference`][Self::move_reference] must be observed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid [`CompressedRef`] belonging to this
    /// `VmCompression` instance, is owned by the caller and not used again, and if `reference` is
    /// the temporary reference, the lock for the VMO that owns the temporary reference is held.
    pub unsafe fn free(&self, reference: CompressedRef) {
        if self.is_temp_reference(reference) {
            core::hint::cold_path();
            // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock
            // for the temporary reference.
            unsafe { self.free_temp_reference(reference) };
            return;
        }
        // SAFETY: By this function's `# Safety` precondition, `reference` is a valid non-temporary
        // compressed reference owned by the caller and not used again.
        unsafe { self.storage.free(reference) };
        self.decompression_skipped.fetch_add(1);
    }

    /// Must be called if a reference is being moved in or from a `VmPageList`. Will return `None`
    /// if the reference is safe to move without being converted to a page. Otherwise returns a
    /// `VmPagePtr` along with associated metadata which is now owned by the caller and should be
    /// used to replace the reference before moving.
    ///
    /// For performance reasons the check is performed inline here, with the unlikely case handled
    /// by a separate method.
    ///
    /// This may only be called if the VMO lock for the VMO that the temporary reference was placed
    /// into is held.
    ///
    /// See [`VmCompressor`] for a full explanation of temporary references and why this is needed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the VMO lock for the VMO that owns `reference` is held for the
    /// duration of this call, and if `reference` is the temporary reference, that it is currently
    /// in use and owned by that VMO.
    #[inline]
    pub unsafe fn move_reference(&self, reference: CompressedRef) -> Option<PageAndMetadata> {
        if self.is_temp_reference(reference) {
            // SAFETY: Discharged by caller's `# Safety` precondition.
            Some(unsafe { self.move_temp_reference(reference) })
        } else {
            None
        }
    }

    /// Returns whether or not the provided reference is a temporary reference.
    ///
    /// See [`VmCompressor`] for a full explanation of temporary references.
    #[inline]
    pub fn is_temp_reference(&self, reference: CompressedRef) -> bool {
        reference.value() == Self::TEMP_REFERENCE_VALUE
    }

    /// Retrieve the metadata for the original page referred to by `reference`.
    ///
    /// This may only be called if the VMO lock for the VMO that `reference` was placed into is
    /// held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] belonging to
    /// this `VmCompression` instance and that the VMO lock for the VMO that owns `reference` is
    /// held.
    pub unsafe fn get_metadata(&self, reference: CompressedRef) -> u32 {
        // Metadata manipulations need to disable analysis. The caller is required to hold the lock
        // for the VMO who created the reference, but we can't refer to that lock here.
        if self.is_temp_reference(reference) {
            core::hint::cold_path();
            // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock
            // for the temporary reference.
            return unsafe { self.instance.temp_reference_metadata() };
        }
        // SAFETY: By this function's `# Safety` precondition, `reference` is a valid, live
        // non-temporary reference and access is synchronized by the VMO lock.
        unsafe { self.storage.get_metadata(reference) }
    }

    /// Set the metadata for the original page referred to by `reference`.
    ///
    /// This may only be called if the VMO lock for the VMO that `reference` was placed into is
    /// held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is a valid, live [`CompressedRef`] belonging to
    /// this `VmCompression` instance and that the VMO lock for the VMO that owns `reference` is
    /// held.
    pub unsafe fn set_metadata(&self, reference: CompressedRef, metadata: u32) {
        // Metadata manipulations need to disable analysis. The caller is required to hold the lock
        // for the VMO who created the reference, but we can't refer to that lock here.
        if self.is_temp_reference(reference) {
            core::hint::cold_path();
            // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock
            // for the temporary reference.
            unsafe { self.instance.set_temp_reference_metadata(metadata) };
        } else {
            // SAFETY: By this function's `# Safety` precondition, `reference` is a valid, live
            // non-temporary reference and access is synchronized by the VMO lock.
            unsafe { self.storage.set_metadata(reference, metadata) };
        }
    }

    /// Returns a snapshot of the compression statistics.
    pub fn get_stats(&self) -> Stats {
        let mut stats = Stats {
            memory_usage: self.storage.get_memory_usage(),
            compression_time: DurationMono::from_nanos(self.compression_time.load()),
            decompression_time: DurationMono::from_nanos(self.decompression_time.load()),
            total_page_compression_attempts: self.compression_attempts.load(),
            failed_page_compression_attempts: self.compression_fail.load(),
            total_page_decompressions: self.decompressions.load(),
            compressed_page_evictions: self.decompression_skipped.load(),
            pages_decompressed_within_log_seconds: [0; NUM_LOG_BUCKETS],
        };
        for i in 0..NUM_LOG_BUCKETS {
            stats.pages_decompressed_within_log_seconds[i] =
                self.decompressions_within_log_seconds[i].load();
        }
        stats
    }

    /// Perform an information dump of the internal state to the debuglog.
    pub fn dump(&self) {
        kprintln!(
            "[zram]: Compression / decompression time {}/{} ns",
            self.compression_time.load(),
            self.decompression_time.load()
        );
        kprintln!(
            "[zram]: Compression attempts: {} success: {} zero page: {} failed: {}",
            self.compression_attempts.load(),
            self.compression_success.load(),
            self.compression_zero_page.load(),
            self.compression_fail.load()
        );
        kprintln!(
            "[zram]: Total decompressions: {} skipped: {} within log seconds counts: \
             {}, {}, {}, {}, {}, {}, {}, {}",
            self.decompressions.load(),
            self.decompression_skipped.load(),
            self.decompressions_within_log_seconds[0].load(),
            self.decompressions_within_log_seconds[1].load(),
            self.decompressions_within_log_seconds[2].load(),
            self.decompressions_within_log_seconds[3].load(),
            self.decompressions_within_log_seconds[4].load(),
            self.decompressions_within_log_seconds[5].load(),
            self.decompressions_within_log_seconds[6].load(),
            self.decompressions_within_log_seconds[7].load()
        );
        self.strategy.dump();
        self.storage.dump();
    }

    /// # Safety
    ///
    /// The caller must ensure that `reference` is the temporary reference and that the lock for
    /// the VMO that owns the temporary reference is held.
    #[cold]
    unsafe fn move_temp_reference(&self, reference: CompressedRef) -> PageAndMetadata {
        // These need to disable analysis as there is a requirement on the caller that the lock for
        // the VMO who created this temporary reference is held, however at this point we have no
        // way to refer to that lock.
        debug_assert!(self.is_temp_reference(reference));
        // The owner of the temp ref is the owner as page. So if we are seeing the temporary
        // reference then we know page cannot progress (i.e. FinalizeState can be called), so we
        // can safely perform the copy.
        // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock for
        // the VMO that owns the temporary reference.
        assert!(unsafe { self.instance.is_temp_reference_in_use() });
        // SAFETY: The VMO lock is held, granting shared read access to `instance.page()`.
        assert!(unsafe { self.instance.page() }.is_some());
        // SAFETY: The VMO lock is held, granting exclusive access to `instance.spare_page()`.
        let spare_page = unsafe { self.instance.spare_page() }.expect("spare_page must exist");
        // SAFETY: `spare_page` is a valid PMM-allocated page owned by `instance` under the VMO
        // lock.
        let addr = physmap::paddr_to_physmap(unsafe { spare_page.paddr() });
        assert!(addr.0 != 0);
        let mut metadata: u32 = 0;
        // SAFETY: `reference` is the active temporary reference with the VMO lock held, and
        // `addr` points to the exclusively owned physmap mapping of `spare_page` (`page::SIZE`
        // initialized bytes).
        unsafe {
            self.decompress(
                reference,
                core::ptr::with_exposed_provenance_mut::<u8>(addr.0),
                &mut metadata,
            );
        }
        // SAFETY: The VMO lock is held, granting exclusive access to `spare_page`.
        let ret = unsafe {
            let ret = self.instance.spare_page();
            self.instance.set_spare_page(None);
            ret
        };
        PageAndMetadata { page: ret, metadata }
    }

    /// # Safety
    ///
    /// The caller must ensure that `reference` is the temporary reference and that the lock for
    /// the VMO that owns the temporary reference is held.
    #[cold]
    unsafe fn free_temp_reference(&self, reference: CompressedRef) {
        debug_assert!(self.is_temp_reference(reference));
        // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock for
        // the VMO that owns the temporary reference.
        unsafe { self.instance.return_temp_reference(reference) };
    }

    /// # Safety
    ///
    /// The caller must ensure that `reference` is the temporary reference, that the lock for the
    /// VMO that owns the temporary reference is held, and that `page_dest` is a non-null pointer
    /// valid for exclusive writes of `page::SIZE` bytes.
    #[cold]
    unsafe fn decompress_temp_reference(
        &self,
        reference: CompressedRef,
        page_dest: *mut u8,
        metadata_dest: &mut u32,
    ) {
        debug_assert!(self.is_temp_reference(reference));
        // SAFETY: By this function's `# Safety` precondition, the caller holds the VMO lock for
        // the VMO that owns the temporary reference.
        assert!(unsafe { self.instance.is_temp_reference_in_use() });
        // SAFETY: The VMO lock is held while the temporary reference is in use, so
        // `instance.page()` is valid and read-only.
        let page = unsafe { self.instance.page() }.expect("temp reference page must exist");
        // SAFETY: `page` is a valid PMM-allocated page owned by `instance`.
        let addr = physmap::paddr_to_physmap(unsafe { page.paddr() });
        assert!(addr.0 != 0);
        // SAFETY: `addr` points to `page::SIZE` readable bytes in `page`'s physmap mapping (which
        // is read-only while the temporary reference is in use), and by caller precondition
        // `page_dest` is valid for exclusive writes of `page::SIZE` bytes, so the two regions are
        // valid and non-overlapping.
        unsafe {
            core::ptr::copy_nonoverlapping(
                core::ptr::with_exposed_provenance::<u8>(addr.0),
                page_dest,
                page::SIZE,
            );
        }
        // SAFETY: The VMO lock is held, granting access to `temp_reference_metadata` and allowing
        // the temporary reference to be returned.
        unsafe {
            *metadata_dest = self.instance.temp_reference_metadata();
            self.free_temp_reference(reference);
        }
    }
}

/// Recycles a `VmCompression` raw pointer when its reference count drops to zero.
///
/// # Safety
///
/// `compression` must be a non-null, valid pointer to a `VmCompression` previously allocated by
/// [`VmCompression::new`] whose reference count has reached zero. The caller must transfer
/// exclusive ownership of `compression` and ensure it is not accessed again during or after this
/// call.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_recycle(compression: *mut VmCompression) {
    debug_assert!(!compression.is_null());
    // SAFETY: By this function's `# Safety` precondition, `compression` is non-null (satisfying
    // `NonNull::new_unchecked`) and points to a uniquely owned `VmCompression` from
    // `VmCompression::new` whose reference count has reached zero (satisfying
    // `VmCompression::recycle`).
    unsafe {
        VmCompression::recycle(NonNull::new_unchecked(compression));
    }
}

/// Decompresses `raw_ref` into `page_dest` and `metadata_dest`.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid shared reference, `metadata_dest` is a
/// valid mutable reference to an initialized `u32`, and the `# Safety` contract of
/// [`VmCompression::decompress`] is upheld.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_decompress(
    compression: &VmCompression,
    raw_ref: u32,
    page_dest: *mut c_void,
    metadata_dest: &mut u32,
) {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe {
        compression.decompress(CompressedRef::new(raw_ref), page_dest.cast::<u8>(), metadata_dest);
    }
}

/// Frees `raw_ref` without decompressing it.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid shared reference and uphold the `# Safety`
/// contract of [`VmCompression::free`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_free(compression: &VmCompression, raw_ref: u32) {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe {
        compression.free(CompressedRef::new(raw_ref));
    }
}

/// Moves the temporary reference `raw_ref` by copying its page into the spare page.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid shared reference and uphold the `# Safety`
/// contract of [`VmCompression::move_reference`] for a temporary reference.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_move_temp_reference(
    compression: &VmCompression,
    raw_ref: u32,
) -> PageAndMetadata {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe { compression.move_temp_reference(CompressedRef::new(raw_ref)) }
}

/// Retrieves metadata for `raw_ref`.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid shared reference and uphold the `# Safety`
/// contract of [`VmCompression::get_metadata`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_get_metadata(
    compression: &VmCompression,
    raw_ref: u32,
) -> u32 {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe { compression.get_metadata(CompressedRef::new(raw_ref)) }
}

/// Sets metadata for `raw_ref`.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid shared reference and uphold the `# Safety`
/// contract of [`VmCompression::set_metadata`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_set_metadata(
    compression: &VmCompression,
    raw_ref: u32,
    metadata: u32,
) {
    // SAFETY: Discharged by caller's `# Safety` precondition.
    unsafe {
        compression.set_metadata(CompressedRef::new(raw_ref), metadata);
    }
}

/// Acquires the compressor instance from `compression` in-place within `out_guard`.
///
/// # Safety
///
/// The caller must ensure that `compression` is a valid reference that outlives `out_guard`, and
/// that `out_guard` points to valid, properly aligned, uninitialized storage for a
/// [`CompressorGuard`] that remains pinned until [`rust_vm_compression_compressor_guard_destroy`]
/// is called.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_acquire_compressor<'a>(
    compression: &'a VmCompression,
    out_guard: *mut CompressorGuard<'a>,
) {
    // SAFETY: By this function's `# Safety` precondition, `out_guard` points to valid, aligned,
    // pinned uninitialized storage for `CompressorGuard`.
    let Ok(()) = unsafe { compression.acquire_compressor().__pinned_init(out_guard) };
}

/// Destroys a [`CompressorGuard`] in-place.
///
/// # Safety
///
/// The caller must ensure that `guard` points to a live, initialized [`CompressorGuard`]
/// previously initialized by [`rust_vm_compression_acquire_compressor`] that will not be used
/// again.
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_compressor_guard_destroy(
    guard: *mut CompressorGuard<'_>,
) {
    // SAFETY: By this function's `# Safety` precondition, `guard` is valid and initialized and
    // will not be accessed again.
    unsafe {
        core::ptr::drop_in_place(guard);
    }
}

/// Returns a pointer to the [`VmCompressor`] guarded by `guard`.
///
/// # Safety
///
/// The caller must ensure that `guard` is a valid reference to an initialized [`CompressorGuard`].
// SAFETY: Globally unique symbol name matching the C declaration in `vm/compression_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_compression_compressor_guard_get(
    guard: &CompressorGuard<'_>,
) -> *mut VmCompressor {
    core::ptr::from_ref(guard.get()).cast_mut()
}
