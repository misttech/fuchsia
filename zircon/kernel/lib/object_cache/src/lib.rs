// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Power-of-two slab object cache allocator.
//!
//! `ObjectCache` retains freed slabs up to a configurable reserve limit to reduce contention on
//! the underlying slab allocator. Both single-cache (`Single`) and per-CPU cache (`PerCpu`) modes
//! are supported.
//!
//! # Custom Allocators
//!
//! Custom slab allocators may be either globally stateful (`GlobalSlabAllocator`, stateless value)
//! or locally stateful (`SlabAllocator`, carrying per-instance state). Slabs must have a
//! power-of-two `SLAB_SIZE` and be aligned to `SLAB_SIZE`.
//!
//! # Orphan Slab Support
//!
//! An `ObjectCache` using a `GlobalSlabAllocator` can be dropped before all objects allocated from
//! it have been freed. Active slabs are marked as orphans when the cache is dropped, and each
//! orphan slab frees itself back to the global allocator once its last allocated object is freed.

mod ffi;

use crate::arch_rs::curr_cpu_num;
use crate::counters::define_kcounter;
use crate::kernel::percpu::PerCpu as KernelPerCpu;
use crate::kernel::thread::{AutoPreemptDisabler, current_memory_allocation_state_is_enabled};
use crate::kernel::types::VAddr;
use crate::vm::page_state::VmPageState;
use crate::vm::page_state::bindings::vm_page_state;
use crate::vm::{physmap, pmm};
use core::convert::Infallible;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit, align_of, size_of};
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use fbl::{
    Array, Canary, DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable,
    DoublyLinkedListNode, HasRefCount, ManagedPtr, MutPtr, PtrTraits, Recyclable, RefCounted,
    RefPtr, TrackingSize, magic,
};
use ksync::{KCell, KMutex, RawMutex};
use pin_init::{Init, PinInit, init_from_closure, pin_data, pin_init, pinned_drop};
use zx_status::Status;

const TRACE_LEVEL: i32 = 0;
const BASIC: i32 = 1;
const DETAIL: i32 = 2;

define_kcounter!(CACHE_OBJECT_ALLOCATED, "cache.object.allocated", Sum);
define_kcounter!(CACHE_SLAB_ALLOCATED, "cache.slab.allocated", Sum);
define_kcounter!(CACHE_OBJECT_FREED, "cache.object.freed", Sum);
define_kcounter!(CACHE_SLAB_FREED, "cache.slab.freed", Sum);

fn count_object_allocation() {
    CACHE_OBJECT_ALLOCATED.add(1);
}

fn count_slab_allocation() {
    CACHE_SLAB_ALLOCATED.add(1);
}

fn count_object_free() {
    CACHE_OBJECT_FREED.add(1);
}

fn count_slab_free() {
    CACHE_SLAB_FREED.add(1);
}

fn get_processor_count() -> usize {
    KernelPerCpu::processor_count()
}

/// Trait for slab allocators (potentially carrying per-instance state) backing an `ObjectCache`.
///
/// # Safety
///
/// Implementers must guarantee:
/// - `SLAB_SIZE` is a power of two.
/// - `allocate` returns a valid, writable, exclusively owned block of at least `SLAB_SIZE`
///   bytes aligned to `SLAB_SIZE`.
/// - `IS_STATEFUL` accurately indicates whether `Slab::destroy` must dereference
///   `object_cache` (`true`, meaning slabs cannot outlive their `ObjectCache`) or may run on
///   an orphan slab after the cache is dropped (`false`, using `release_global`).
pub unsafe trait SlabAllocator {
    /// Size of each slab in bytes. Must be a power of two.
    const SLAB_SIZE: usize;

    /// True if this allocator carries per-instance state and cannot outlive its `ObjectCache`.
    const IS_STATEFUL: bool = true;

    /// Allocates a new slab aligned to `Self::SLAB_SIZE`.
    fn allocate(&self) -> Result<NonNull<u8>, Status>;

    /// Releases a slab previously returned by `allocate`.
    ///
    /// # Safety
    ///
    /// `slab` must be the base address of a slab allocated from `self` that is no longer in use.
    unsafe fn release(&self, slab: NonNull<u8>);

    /// Increments the object allocation counter.
    fn count_object_allocation(&self);

    /// Increments the object free counter.
    fn count_object_free(&self);

    /// Increments the slab allocation counter.
    fn count_slab_allocation(&self);

    /// Increments the slab free counter.
    fn count_slab_free(&self);

    /// Releases an orphan slab without an `ObjectCache` instance.
    ///
    /// # Safety
    ///
    /// `slab` must be the base address of an orphan slab allocated from this global allocator.
    unsafe fn release_global(_slab: NonNull<u8>) {
        unreachable!("stateful allocator cannot release orphan slabs");
    }

    /// Increments the object free counter for an orphan slab.
    fn count_object_free_global() {
        unreachable!("stateful allocator cannot free objects on orphan slabs");
    }

    /// Increments the slab free counter for an orphan slab.
    fn count_slab_free_global() {
        unreachable!("stateful allocator cannot free orphan slabs");
    }
}

/// Trait for stateless / globally stateful slab allocators that support orphan slabs.
///
/// # Safety
///
/// Implementers must guarantee:
/// - `SLAB_SIZE` is a power of two.
/// - `allocate` returns a valid, writable, exclusively owned block of at least `SLAB_SIZE`
///   bytes aligned to `SLAB_SIZE`.
/// - Slabs allocated by `allocate` can be freed via `release` without an `ObjectCache`
///   instance, allowing orphan slabs to outlive the cache.
pub unsafe trait GlobalSlabAllocator: Default {
    /// Size of each slab in bytes. Must be a power of two.
    const SLAB_SIZE: usize;

    /// Allocates a new slab aligned to `Self::SLAB_SIZE`.
    fn allocate() -> Result<NonNull<u8>, Status>;

    /// Releases a slab previously returned by `allocate`.
    ///
    /// # Safety
    ///
    /// `slab` must be the base address of a slab allocated from `Self` that is no longer in use.
    unsafe fn release(slab: NonNull<u8>);

    /// Increments the object allocation counter.
    fn count_object_allocation();

    /// Increments the object free counter.
    fn count_object_free();

    /// Increments the slab allocation counter.
    fn count_slab_allocation();

    /// Increments the slab free counter.
    fn count_slab_free();
}

// SAFETY: `G` upholds the `GlobalSlabAllocator` alignment, size, and validity invariants, and
// stateless / globally stateful allocators support orphan slabs (`IS_STATEFUL == false`).
unsafe impl<G: GlobalSlabAllocator> SlabAllocator for G {
    const SLAB_SIZE: usize = G::SLAB_SIZE;
    const IS_STATEFUL: bool = false;

    #[inline]
    fn allocate(&self) -> Result<NonNull<u8>, Status> {
        G::allocate()
    }

    /// # Safety
    ///
    /// `slab` must be the base address of a slab allocated from `G` that is no longer in use.
    #[inline]
    unsafe fn release(&self, slab: NonNull<u8>) {
        // SAFETY: Caller upholds the `SlabAllocator::release` contract.
        unsafe { G::release(slab) };
    }

    #[inline]
    fn count_object_allocation(&self) {
        G::count_object_allocation();
    }

    #[inline]
    fn count_object_free(&self) {
        G::count_object_free();
    }

    #[inline]
    fn count_slab_allocation(&self) {
        G::count_slab_allocation();
    }

    #[inline]
    fn count_slab_free(&self) {
        G::count_slab_free();
    }

    /// # Safety
    ///
    /// `slab` must be the base address of an orphan slab allocated from `G`.
    #[inline]
    unsafe fn release_global(slab: NonNull<u8>) {
        // SAFETY: Caller upholds the `SlabAllocator::release_global` contract.
        unsafe { G::release(slab) };
    }

    #[inline]
    fn count_object_free_global() {
        G::count_object_free();
    }

    #[inline]
    fn count_slab_free_global() {
        G::count_slab_free();
    }
}

/// The default allocator for `ObjectCache`. Allocates page-sized slabs from the PMM.
///
/// This allocator is globally stateful, allowing orphan slabs.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAllocator;

impl DefaultAllocator {
    /// Size of each slab in bytes.
    pub const SLAB_SIZE: usize = page::SIZE;

    /// Allocates a page-sized slab from the PMM.
    pub fn allocate() -> Result<NonNull<u8>, Status> {
        let (vm_page, paddr) = pmm::alloc_page(pmm::ALLOC_FLAG_ANY)?;
        // SAFETY: `vm_page` was just allocated from the PMM and we have exclusive access to it.
        unsafe {
            vm_page.as_ref().set_state(VmPageState(vm_page_state::SLAB));
        }
        let vaddr = physmap::paddr_to_physmap(paddr);
        let ptr = core::ptr::with_exposed_provenance_mut::<u8>(vaddr.0);
        // SAFETY: `paddr_to_physmap` returns a valid non-null kernel virtual address.
        Ok(unsafe { NonNull::new_unchecked(ptr) })
    }

    /// Releases a slab previously returned by `DefaultAllocator::allocate`.
    ///
    /// # Safety
    ///
    /// `slab` must be the base address of a slab allocated from `DefaultAllocator` and no
    /// longer in use.
    pub unsafe fn release(slab: NonNull<u8>) {
        let paddr = physmap::physmap_to_paddr(VAddr(slab.addr().get()));
        let page = pmm::paddr_to_vm_page(paddr).expect("slab maps to valid vm_page");
        // SAFETY: `page` is the valid SLAB page being freed back to the PMM.
        unsafe {
            debug_assert_eq!(page.as_ref().state(), VmPageState(vm_page_state::SLAB));
            pmm::free_page(page);
        }
    }

    /// Increments the object allocation counter.
    pub fn count_object_allocation() {
        count_object_allocation();
    }

    /// Increments the object free counter.
    pub fn count_object_free() {
        count_object_free();
    }

    /// Increments the slab allocation counter.
    pub fn count_slab_allocation() {
        count_slab_allocation();
    }

    /// Increments the slab free counter.
    pub fn count_slab_free() {
        count_slab_free();
    }
}

// SAFETY: `DefaultAllocator::SLAB_SIZE` (`page::SIZE`) is a power of two, `allocate` returns a
// page-aligned writable page from the PMM, and `release` frees it globally via the PMM.
unsafe impl GlobalSlabAllocator for DefaultAllocator {
    const SLAB_SIZE: usize = Self::SLAB_SIZE;

    fn allocate() -> Result<NonNull<u8>, Status> {
        Self::allocate()
    }

    /// # Safety
    ///
    /// `slab` must be the base address of a slab allocated from `DefaultAllocator` that is no
    /// longer in use.
    unsafe fn release(slab: NonNull<u8>) {
        // SAFETY: Caller upholds the `GlobalSlabAllocator::release` contract.
        unsafe { Self::release(slab) };
    }

    fn count_object_allocation() {
        Self::count_object_allocation();
    }

    fn count_object_free() {
        Self::count_object_free();
    }

    fn count_slab_allocation() {
        Self::count_slab_allocation();
    }

    fn count_slab_free() {
        Self::count_slab_free();
    }
}

/// Mode marker for a single shared `ObjectCache`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Single;

/// Mode marker for a per-CPU `ObjectCache`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PerCpu;

mod sealed {
    use super::SlabAllocator;

    pub trait Sealed<T, A: SlabAllocator> {}
}

/// Trait mapping cache mode markers (`Single`, `PerCpu`) to their internal representation.
pub trait CacheMode<T, A: SlabAllocator>: sealed::Sealed<T, A> {
    /// Internal cache storage type for this mode.
    type Inner;
}

impl<T, A: SlabAllocator> sealed::Sealed<T, A> for Single {}
impl<T, A: SlabAllocator> CacheMode<T, A> for Single {
    type Inner = SingleCache<T, A>;
}

impl<T, A: SlabAllocator> sealed::Sealed<T, A> for PerCpu {}
impl<T, A: SlabAllocator> CacheMode<T, A> for PerCpu {
    type Inner = PerCpuCache<T, A>;
}

/// Owned smart pointer to an object allocated from an `ObjectCache<T, _, A>`.
///
/// Dropping `UniquePtr<T, A>` drops `T` in place and returns its slab slot to the slab it was
/// allocated from.
#[repr(transparent)]
pub struct UniquePtr<T, A: SlabAllocator = DefaultAllocator> {
    ptr: NonNull<T>,
    _phantom: PhantomData<A>,
}

// SAFETY: `UniquePtr` uniquely owns `T`, and dropping it on another thread is safe when `T: Send`
// and `A: Send + Sync`.
unsafe impl<T: Send, A: SlabAllocator + Send + Sync> Send for UniquePtr<T, A> {}
// SAFETY: `UniquePtr` only hands out `&T` from `&self`, so it is `Sync` when `T: Sync` and
// `A: Send + Sync`.
unsafe impl<T: Sync, A: SlabAllocator + Send + Sync> Sync for UniquePtr<T, A> {}

impl<T, A: SlabAllocator> UniquePtr<T, A> {
    /// Returns a const raw pointer to the managed object.
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        this.ptr.as_ptr()
    }

    /// Returns a mutable raw pointer to the managed object.
    #[inline]
    pub fn as_mut_ptr(this: &mut Self) -> *mut T {
        this.ptr.as_ptr()
    }

    /// Consumes the `UniquePtr`, returning the raw pointer without destroying the object.
    #[inline]
    pub fn into_raw(this: Self) -> *mut T {
        let ptr = this.ptr.as_ptr();
        core::mem::forget(this);
        ptr
    }

    /// Reconstructs a `UniquePtr` from a raw pointer previously returned by `into_raw`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, uniquely owned pointer to a `T` allocated from an
    /// `ObjectCache<T, _, A>`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut T) -> Self {
        // SAFETY: Caller guarantees `ptr` is non-null and valid.
        Self { ptr: unsafe { NonNull::new_unchecked(ptr) }, _phantom: PhantomData }
    }

    /// Adopts a freshly allocated ref-counted object into a `fbl::RefPtr<T>`.
    #[inline]
    pub fn into_ref_ptr(this: Self) -> RefPtr<T>
    where
        T: HasRefCount + ObjectCacheDeletable<A>,
    {
        let raw = Self::into_raw(this);
        // SAFETY: `raw` points to a freshly constructed, unadopted `T`, and
        // `T: ObjectCacheDeletable<A>` guarantees its `Recyclable::recycle` implementation
        // drops `T` in place and returns its slot to `ObjectCache<T, Single, A>` via
        // `delete_raw`.
        unsafe { RefPtr::adopt_raw(raw) }
    }
}

impl<T, A: SlabAllocator> Deref for UniquePtr<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.ptr` points to a live, initialized `T` owned by `self`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T, A: SlabAllocator> DerefMut for UniquePtr<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `self` has exclusive ownership of `self.ptr`.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T, A: SlabAllocator> Drop for UniquePtr<T, A> {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is uniquely owned by `self` and was allocated from an
        // `ObjectCache<T, _, A>`.
        unsafe {
            core::ptr::drop_in_place(self.ptr.as_ptr());
            SingleCache::<T, A>::delete_raw(self.ptr);
        }
    }
}

// SAFETY: `UniquePtr<T, A>` uniquely owns `T` and round-trips losslessly through `into_raw` and
// `from_raw`.
unsafe impl<T, A: SlabAllocator> PtrTraits for UniquePtr<T, A> {
    type Target = T;
    const IS_MANAGED: bool = true;

    #[inline]
    fn into_raw(self) -> *mut T {
        Self::into_raw(self)
    }

    /// # Safety
    ///
    /// `raw` must have been returned by a previous call to `UniquePtr::into_raw`.
    #[inline]
    unsafe fn from_raw(raw: *mut T) -> Self {
        // SAFETY: Caller guarantees `raw` came from `into_raw`.
        unsafe { Self::from_raw(raw) }
    }

    #[inline]
    fn get_ref(&self) -> &T {
        self
    }
}

// SAFETY: `UniquePtr<T, A>` manages the lifetime of `T` and drops it when dropped.
unsafe impl<T, A: SlabAllocator> ManagedPtr for UniquePtr<T, A> {}

// SAFETY: `UniquePtr<T, A>` holds exclusive ownership of `T`.
unsafe impl<T, A: SlabAllocator> MutPtr for UniquePtr<T, A> {}

/// Marker trait for types whose [`fbl::Recyclable::recycle`] implementation returns the object to
/// an `ObjectCache<Self, Single, A>`.
///
/// This is the Rust equivalent of C++ `object_cache::Deletable<T, Allocator>`. Use
/// [`impl_object_cache_deletable!`] to implement both [`fbl::Recyclable`] and
/// `ObjectCacheDeletable<A>` for a type.
///
/// # Safety
///
/// Implementers must guarantee that `<Self as fbl::Recyclable>::recycle` drops `Self` in place and
/// returns the backing slot to `ObjectCache<Self, Single, A>` via [`ObjectCache::delete_raw`].
pub unsafe trait ObjectCacheDeletable<A: SlabAllocator = DefaultAllocator>:
    Recyclable
{
}

/// Implements [`fbl::Recyclable`] and [`ObjectCacheDeletable`] for a type allocated from an
/// `ObjectCache` so it can be held in `fbl::RefPtr<T>`.
///
/// This is the Rust equivalent of C++ `object_cache::Deletable<T, Allocator>`.
#[macro_export]
macro_rules! impl_object_cache_deletable {
    ($ty:ty) => {
        $crate::impl_object_cache_deletable!($ty, $crate::object_cache::DefaultAllocator);
    };
    ($ty:ty, $alloc:ty) => {
        // SAFETY: `$ty` instances are allocated from `ObjectCache<$ty, _, $alloc>`, so dropping
        // in place and calling `ObjectCache::delete_raw` is the matching deallocation path.
        unsafe impl ::fbl::Recyclable for $ty {
            /// # Safety
            ///
            /// `ptr` must be the last reference to a valid, initialized `Self` allocated from an
            /// `ObjectCache<Self, _, $alloc>`.
            unsafe fn recycle(ptr: ::core::ptr::NonNull<Self>) {
                type Cache<T> =
                    $crate::object_cache::ObjectCache<T, $crate::object_cache::Single, $alloc>;
                // SAFETY: `ptr` is the last reference to a valid, initialized `Self` allocated
                // from an `ObjectCache<Self, _, $alloc>`.
                unsafe {
                    ::core::ptr::drop_in_place(ptr.as_ptr());
                    Cache::<Self>::delete_raw(ptr);
                }
            }
        }

        // SAFETY: The `Recyclable::recycle` implementation above drops `Self` in place and returns
        // the slot to `ObjectCache<Self, Single, $alloc>` via `ObjectCache::delete_raw`.
        unsafe impl $crate::object_cache::ObjectCacheDeletable<$alloc> for $ty {}
    };
}

pub use impl_object_cache_deletable;

// Optimization flags that impact the efficiency of the Slab destructor. May
// be set to `false` to force manually clearing the free list for debugging
// purposes.
const ENTRY_ALLOW_CLEAR_UNSAFE: bool = true;

// An entry in the Slab object array. Exists either as a node in the slab free
// list or as an allocated instance of T.
#[repr(C)]
union Entry<T> {
    list_node: ManuallyDrop<DoublyLinkedListNode<Entry<T>>>,
    object: ManuallyDrop<MaybeUninit<T>>,
}

impl<T> DoublyLinkedListContainable<Entry<T>> for Entry<T> {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Entry<T>> {
        // SAFETY: `get_node` is only called while the entry is linked in `SlabControl::free_list`,
        // where `list_node` is the active union field.
        unsafe { &self.list_node }
    }
}

impl<T> Entry<T> {
    /// Initializes this entry's free-list node in uninitialized slab memory.
    ///
    /// # Safety
    ///
    /// `entry` must point to valid, aligned, uninitialized storage in the slab entry array.
    unsafe fn init_list_node(entry: NonNull<Self>) {
        // SAFETY: `entry` points to valid, uninitialized storage in the slab entry array.
        unsafe {
            (&raw mut (*entry.as_ptr()).list_node)
                .write(ManuallyDrop::new(DoublyLinkedListNode::new()));
        }
    }

    /// Converts this entry from a list node to an object, constructing the instance of `T` with
    /// the given initializer. This should be called after the entry is allocated, outside of
    /// internal locks to avoid unnecessary lock dependencies.
    ///
    /// # Safety
    ///
    /// `slot` must point to an entry just popped from `free_list` with an active `list_node`.
    unsafe fn to_object_init<E>(
        slot: NonNull<Self>,
        init: impl Init<T, E>,
    ) -> Result<NonNull<T>, E> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Entry::ToObject"
        );
        // SAFETY: `slot` was just popped from `free_list`, so `list_node` is initialized and not
        // in any container.
        unsafe {
            ManuallyDrop::drop(&mut (*slot.as_ptr()).list_node);
        }
        let object_ptr = slot.cast::<T>();
        // SAFETY: `object_ptr` points to valid, aligned, uninitialized storage for `T`.
        unsafe {
            init.__init(object_ptr.as_ptr())?;
        }
        Ok(object_ptr)
    }

    /// Converts this entry to a list node. The object must already be destroyed.
    ///
    /// # Safety
    ///
    /// `pointer` must point to a valid slab entry slot whose `T` value has already been dropped.
    unsafe fn to_list_node(pointer: NonNull<T>) -> NonNull<Self> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Entry::ToListNode"
        );
        let entry = pointer.cast::<Self>();
        // SAFETY: `entry` points to valid, unborrowed storage in the slab entry array.
        unsafe {
            (&raw mut (*entry.as_ptr()).list_node)
                .write(ManuallyDrop::new(DoublyLinkedListNode::new()));
        }
        entry
    }
}

type FreeList<T> = DoublyLinkedList<NonNull<Entry<T>>, DefaultObjectTag, TrackingSize>;
type SlabList<T, A> = DoublyLinkedList<RefPtr<Slab<T, A>>>;

// Slab control block. Separate from the Entry array to simplify computing the
// size of the Entry array and to avoid aliasing `&Slab` with `&mut T` entries.
#[ksync::guarded]
#[pin_data]
#[repr(C)]
struct SlabControl<T, A: SlabAllocator> {
    canary: Canary<{ magic(b"slab") }>,
    ref_count: RefCounted,
    #[pin]
    list_node: DoublyLinkedListNode<Slab<T, A>>,
    orphan_flag: AtomicBool,
    #[mutex]
    lock: KMutex<RawMutex>,
    #[guarded_by(lock)]
    object_cache: NonNull<SingleCache<T, A>>,
    #[pin]
    #[guarded_by(lock)]
    free_list: FreeList<T>,
}

// A slab of objects in the object cache. Constructed at `base + CONTROL_OFFSET`
// inside a power-of-two aligned memory block of size `A::SLAB_SIZE`, with
// `ENTRIES_PER_SLAB` `Entry<T>` slots placed at `base + 0 .. base + CONTROL_OFFSET`.
#[pin_data(PinnedDrop)]
#[repr(C)]
struct Slab<T, A: SlabAllocator> {
    #[pin]
    control: SlabControl<T, A>,
}

// SAFETY: `Slab` synchronizes all mutable access through `control.lock` and atomics.
unsafe impl<T: Send, A: SlabAllocator + Send + Sync> Send for Slab<T, A> {}
// SAFETY: `Slab` synchronizes all shared access through `control.lock` and atomics.
unsafe impl<T: Send, A: SlabAllocator + Send + Sync> Sync for Slab<T, A> {}

impl<T, A: SlabAllocator> HasRefCount for Slab<T, A> {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        &self.control.ref_count
    }
}

impl<T, A: SlabAllocator> DoublyLinkedListContainable<Slab<T, A>> for Slab<T, A> {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Slab<T, A>> {
        &self.control.list_node
    }
}

// SAFETY: `Slab` is allocated via `A::allocate` and destroyed by `Slab::destroy`.
unsafe impl<T, A: SlabAllocator> Recyclable for Slab<T, A> {
    /// # Safety
    ///
    /// `ptr` must be the last `RefPtr` reference to this `Slab`.
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` is the last `RefPtr` reference to this `Slab`.
        unsafe { Self::destroy(ptr) };
    }
}

#[pinned_drop]
impl<T, A: SlabAllocator> PinnedDrop for Slab<T, A> {
    fn drop(self: Pin<&mut Self>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::~Slab"
        );
        let entries_start = Self::base_ptr(self.as_ref().get_ref()).cast::<Entry<T>>();
        let mut free_list = self.project().control.project().free_list.get_pinned_mut();
        debug_assert!(Self::is_empty(&free_list));
        if ENTRY_ALLOW_CLEAR_UNSAFE {
            // SAFETY: This slab is being destroyed after the last reference was released. Every
            // entry's `list_node` is wrapped in `ManuallyDrop` inside the `Entry` union and is
            // never dropped or used in any list or cursor operation again before `Slab::destroy`
            // releases the backing slab memory immediately after `drop`.
            unsafe {
                free_list.clear_unsafe();
            }
        } else {
            // Consistency check that every entry is on the free list. Attempting to
            // erase an entry that is not on the free list will assert.
            for i in 0..Self::ENTRIES_PER_SLAB {
                // SAFETY: `i < Self::ENTRIES_PER_SLAB`, so `entry_ptr` points to an `Entry<T>`
                // slot within `[base, base + CONTROL_OFFSET)`. Every allocated object holds a
                // reference to its slab, so by the time the last reference is dropped every entry
                // has been returned to this slab's `free_list` (debug-checked by `is_empty()`
                // above); its active field is therefore `list_node`, which is linked in
                // `free_list`. After erasing, the node is unlinked and never used again, so
                // dropping it is sound.
                unsafe {
                    let entry_ptr = entries_start.add(i);
                    let erased = free_list.as_mut().erase(&*entry_ptr);
                    debug_assert!(erased.is_some());
                    ManuallyDrop::drop(&mut (*entry_ptr).list_node);
                }
            }
        }
    }
}

impl<T, A: SlabAllocator> Slab<T, A> {
    const SLAB_ADDR_MASK: usize = A::SLAB_SIZE - 1;
    const SLAB_CONTROL_SIZE: usize =
        size_of::<SlabControl<T, A>>().next_multiple_of(align_of::<SlabControl<T, A>>());
    const ENTRIES_PER_SLAB: usize =
        (A::SLAB_SIZE - Self::SLAB_CONTROL_SIZE) / size_of::<Entry<T>>();
    const CONTROL_OFFSET: usize =
        (Self::ENTRIES_PER_SLAB * size_of::<Entry<T>>()).next_multiple_of(align_of::<Self>());

    const _LAYOUT_ASSERT: () = {
        assert!(A::SLAB_SIZE.is_power_of_two(), "Slabs must be a power of two!");
        assert!(A::SLAB_SIZE > Self::SLAB_CONTROL_SIZE);
        assert!(Self::ENTRIES_PER_SLAB > 0);
        assert!(align_of::<Entry<T>>() <= A::SLAB_SIZE);
        assert!(align_of::<Self>() <= A::SLAB_SIZE);
        assert!(Self::CONTROL_OFFSET + size_of::<Self>() <= A::SLAB_SIZE);
    };

    // Allocates and initializes a new slab for `object_cache`.
    fn try_new(object_cache: NonNull<SingleCache<T, A>>, base: NonNull<u8>) -> RefPtr<Self> {
        let () = Self::_LAYOUT_ASSERT;
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::Slab"
        );
        debug_assert_eq!(base.addr().get() & Self::SLAB_ADDR_MASK, 0);

        // Expose the full-slab allocation provenance so `base_ptr`, `destroy`, and
        // `from_allocated_pointer` can recover provenance over the entire `[base, base +
        // A::SLAB_SIZE)` block from any pointer into the slab.
        let _ = base.as_ptr().expose_provenance();
        let slab_ptr =
            // SAFETY: `CONTROL_OFFSET + size_of::<Self>() <= A::SLAB_SIZE` is checked at compile
            // time, and `base` is aligned to `A::SLAB_SIZE >= align_of::<Self>()`.
            unsafe { base.as_ptr().add(Self::CONTROL_OFFSET) }.cast::<Self>();

        let init = pin_init!(Self {
            control <- pin_init!(SlabControl {
                canary: Canary::new(),
                ref_count: RefCounted::new(),
                list_node: DoublyLinkedListNode::new(),
                orphan_flag: AtomicBool::new(false),
                lock <- KMutex::init(),
                object_cache: KCell::new(object_cache),
                free_list <- KCell::pin_init(FreeList::new()),
            }),
        });
        // SAFETY: `slab_ptr` points to valid, aligned, uninitialized memory that stays pinned in
        // place until `Slab::destroy` drops it.
        unsafe {
            let _ = init.__pinned_init(slab_ptr);
        }

        // SAFETY: `slab_ptr` was just initialized above and no other thread can see it yet.
        let mut slab_pin = unsafe { Pin::new_unchecked(&mut *slab_ptr) };
        let mut free_list =
            slab_pin.as_mut().project().control.project().free_list.get_pinned_mut();
        for i in 0..Self::ENTRIES_PER_SLAB {
            // SAFETY: `i < Self::ENTRIES_PER_SLAB`, so `entry` lies within `[base, base +
            // CONTROL_OFFSET)` and does not overlap `slab_ptr`.
            let entry = unsafe { base.cast::<Entry<T>>().add(i) };
            // SAFETY: `entry` is valid uninitialized slot memory in the slab.
            unsafe {
                Entry::init_list_node(entry);
                free_list.as_mut().push_front_raw(entry);
            }
        }

        // SAFETY: `slab_ptr` is freshly initialized and has not been adopted yet.
        unsafe { RefPtr::adopt_raw(slab_ptr) }
    }

    // Recovers the power-of-two aligned base pointer of the slab from a `Slab` pointer.
    #[inline]
    fn base_ptr(slab: *const Self) -> *mut u8 {
        core::ptr::with_exposed_provenance_mut::<u8>(slab.addr() & !Self::SLAB_ADDR_MASK)
    }

    /// Destructs the slab and returns its memory to the allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid `Slab` whose last `RefPtr` reference is being released.
    unsafe fn destroy(ptr: NonNull<Self>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::Destroy"
        );

        let slab_ptr = core::ptr::with_exposed_provenance_mut::<Self>(ptr.addr().get());
        // SAFETY: `ptr` is non-null, so `slab_ptr` with the same address is non-null.
        let slab_nonnull = unsafe { NonNull::new_unchecked(slab_ptr) };
        // SAFETY: `slab_nonnull` is valid and uniquely owned by `destroy` when the last `RefPtr`
        // drops.
        let slab_ref = unsafe { slab_nonnull.as_ref() };

        // Slabs cannot be orphaned when the allocator is stateful.
        debug_assert!(!A::IS_STATEFUL || !slab_ref.is_orphan());

        // The lock should not be held or acquired when the slab is being destroyed.
        // SAFETY: `slab_ptr` is valid and uniquely owned here, with full-slab mutable provenance.
        let object_cache = unsafe {
            let mut slab_pin = Pin::new_unchecked(&mut *slab_ptr);
            *slab_pin.as_mut().project().control.project().object_cache.get_inner_mut()
        };

        let base_raw = Self::base_ptr(slab_ptr);
        // SAFETY: `slab_ptr` is valid and uniquely owned; `PinnedDrop` checks `is_empty()` and
        // empties `free_list` via `clear_unsafe` (or the per-entry erase loop when
        // `ENTRY_ALLOW_CLEAR_UNSAFE` is false), and `RawMutex` drop asserts the lock is not held.
        unsafe {
            core::ptr::drop_in_place(slab_ptr);
        }
        // SAFETY: `base_raw` is the non-null aligned base of this slab.
        let base = unsafe { NonNull::new_unchecked(base_raw) };

        if A::IS_STATEFUL {
            // SAFETY: Stateful allocators never orphan slabs, so `object_cache` outlives `Slab`.
            let cache = unsafe { object_cache.as_ref() };
            cache.allocator.count_slab_free();
            // SAFETY: `base` was allocated from `cache.allocator`.
            unsafe {
                cache.allocator.release(base);
            }
        } else {
            A::count_slab_free_global();
            // SAFETY: `base` was allocated from global allocator `A`.
            unsafe {
                A::release_global(base);
            }
        }
    }

    #[inline]
    fn in_container(&self) -> bool {
        self.control.list_node.in_container()
    }

    fn set_orphan(&self) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::SetOrphan"
        );
        self.control.orphan_flag.store(true, Ordering::SeqCst);
    }

    #[inline]
    fn available_objects(free_list: &FreeList<T>) -> usize {
        free_list.len()
    }

    #[inline]
    fn is_empty(free_list: &FreeList<T>) -> bool {
        Self::available_objects(free_list) == Self::ENTRIES_PER_SLAB
    }

    #[inline]
    fn is_full(free_list: &FreeList<T>) -> bool {
        free_list.is_empty()
    }

    #[inline]
    fn is_orphan(&self) -> bool {
        self.control.orphan_flag.load(Ordering::SeqCst)
    }

    /// Returns a reference to a slab given a pointer to an entry.
    ///
    /// # Safety
    ///
    /// `pointer` must point to an allocated entry slot inside a live `Slab<T, A>`.
    unsafe fn from_allocated_pointer(pointer: NonNull<T>) -> RefPtr<Self> {
        // Slab addresses are guaranteed to be power-of-two aligned. This contract
        // is checked in `try_new`.
        let slab_addr = (pointer.addr().get() & !Self::SLAB_ADDR_MASK) + Self::CONTROL_OFFSET;
        let slab_ptr = core::ptr::with_exposed_provenance_mut::<Self>(slab_addr);
        // SAFETY: `pointer` was allocated from this slab, which kept a live reference count on
        // `slab_ptr` via `RefPtr::add_ref` in `Slab::allocate`.
        unsafe {
            (*slab_ptr).control.canary.assert();
            RefPtr::from_raw(slab_ptr)
        }
    }

    // Allocates an entry from the slab and moves the slab to the appropriate
    // list in the cache. The caller must hold a reference to this slab.
    fn allocate(&self) -> Result<NonNull<Entry<T>>, Status> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::Allocate"
        );

        ksync::lock!(let mut slab_guard = self.control.lock_lock());
        debug_assert!(!self.is_orphan());

        // Retry on another slab if another thread allocated the last object in
        // this slab between releasing the object cache lock in `get_slab` and
        // acquiring the slab lock here.
        if Self::available_objects(slab_guard.fields().free_list) == 0 {
            return Err(Status::NO_MEMORY);
        }

        // SAFETY: `!self.is_orphan()` holds under `self.control.lock`, so `object_cache` is alive.
        let object_cache = unsafe { slab_guard.fields().object_cache.as_ref() };
        ksync::lock!(let mut cache_guard = object_cache.lock_lock());

        // Retry on another slab if another thread removed this slab from the
        // object cache between releasing the object cache lock in `get_slab` and
        // acquiring the slab lock here. Technically, this slab could be re-added
        // to the cache, however, there is a good chance that another partial slab
        // exists that would reduce fragmentation.
        if !self.in_container() {
            return Err(Status::NO_MEMORY);
        }

        let mut slab_fields = slab_guard.as_mut().fields_mut();
        let was_empty = Self::is_empty(&slab_fields.free_list);
        let entry = slab_fields.free_list.as_mut().pop_front().expect("free_list not empty");
        let is_full = Self::is_full(&slab_fields.free_list);

        if was_empty || is_full {
            let mut cache_fields = cache_guard.as_mut().fields_mut();
            let erased = if was_empty {
                // SAFETY: `self` was empty and in a container, so it is in `empty_list`.
                unsafe { cache_fields.empty_list.as_mut().erase(self) }
            } else {
                // SAFETY: `self` was partial and in a container, so it is in `partial_list`.
                unsafe { cache_fields.partial_list.as_mut().erase(self) }
            }
            .expect("slab in source list");

            if is_full {
                cache_fields.full_list.as_mut().push_front(erased);
            } else {
                cache_fields.partial_list.as_mut().push_front(erased);
            }
        }

        // The allocated object maintains a reference to prevent prematurely
        // releasing the slab in back reference scenarios.
        RefPtr::add_ref(self);

        object_cache.allocator.count_object_allocation();
        Ok(entry)
    }

    // Returns the given entry to the free list. The caller must hold a
    // reference to the slab.
    fn free(&self, entry: NonNull<Entry<T>>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::Free"
        );
        let entries_start = Self::base_ptr(self).cast::<Entry<T>>();
        // SAFETY: `ENTRIES_PER_SLAB` entries fit within the slab allocation.
        let entries_end = unsafe { entries_start.add(Self::ENTRIES_PER_SLAB) };
        let entry_ptr = entry.as_ptr();
        debug_assert!(entry_ptr >= entries_start && entry_ptr < entries_end);

        ksync::lock!(let mut control_guard = self.control.lock_lock());
        debug_assert!(
            Self::available_objects(control_guard.fields().free_list) < Self::ENTRIES_PER_SLAB
        );

        // If the cache containing this slab was destroyed while destroying the
        // last object in the cache (e.g. back reference) this slab will be marked
        // orphan and should not attempt to access the object cache.
        if self.is_orphan() {
            // Just return the entry to the free list and release the additional
            // reference to the orphan slab.
            // SAFETY: `entry` is a valid freed entry in this slab and not currently in any list.
            unsafe {
                control_guard.as_mut().fields_mut().free_list.as_mut().push_front_raw(entry);
            }
            A::count_object_free_global();
        } else {
            // Our ObjectCache instance was alive while we held our lock, and our
            // slab must be a member of either the full or partial lists (meaning
            // that an ObjectCache in the process of destructing will need to bounce
            // through our lock on its way out).
            //
            // Increment the free operation in-flight count in our OC. This
            // guarantees that we will have a chance to fully release our OC's lock
            // and properly synchronize with the OC destructor in the case that we
            // end up moving our slab to the free list, or removing it entirely from
            // the object cache, just before we drop the lock. Disable preemption
            // while we do this. We don't want the destructor spinning waiting for
            // us to drop the in-flight count if we become preempted during this
            // operation.
            // SAFETY: `!self.is_orphan()` holds under `self.control.lock`, so `object_cache` is
            // alive and valid.
            let object_cache = unsafe { control_guard.fields().object_cache.as_ref() };
            object_cache.canary.assert();
            let _preempt_disable = AutoPreemptDisabler::new();
            object_cache.slab_free_ops_in_flight.fetch_add(1, Ordering::Release);

            {
                // Explicit scope for the OC guard.
                ksync::lock!(let mut cache_guard = object_cache.lock_lock());

                let was_full = Self::is_full(control_guard.fields().free_list);
                // SAFETY: `entry` is a valid freed entry in this slab and not in any list.
                unsafe {
                    control_guard.as_mut().fields_mut().free_list.as_mut().push_front_raw(entry);
                }
                let is_empty = Self::is_empty(control_guard.fields().free_list);

                // This slab may have been orphaned while blocking on the cache lock
                // above if the cache destructor ran concurrently with this free
                // operation.
                if !self.is_orphan() {
                    let mut cache_fields = cache_guard.as_mut().fields_mut();
                    if was_full || is_empty {
                        let erased = if was_full {
                            // SAFETY: `self` was full and not orphaned, so it is in `full_list`.
                            unsafe { cache_fields.full_list.as_mut().erase(self) }
                        } else {
                            // SAFETY: `self` was partial and not orphaned, so it is in
                            // `partial_list`.
                            unsafe { cache_fields.partial_list.as_mut().erase(self) }
                        }
                        .expect("slab in source list");

                        if is_empty {
                            cache_fields.empty_list.as_mut().push_front(erased);
                        } else {
                            cache_fields.partial_list.as_mut().push_front(erased);
                        }
                    }

                    if is_empty && object_cache.should_trim_locked(&cache_fields) {
                        SingleCache::remove_slab_locked(&mut cache_fields, self);
                    }
                }
            }

            object_cache.allocator.count_object_free();

            // Drop the in-flight count back to what it was, and never attempt to
            // touch our object cache again. It might already be in the process of
            // destructing.
            let prev = object_cache.slab_free_ops_in_flight.fetch_sub(1, Ordering::Release);
            debug_assert_ne!(prev, 0);
        }
    }
}

/// Internal implementation of `ObjectCache<T, Single, A>`.
#[ksync::guarded]
#[pin_data(PinnedDrop)]
pub struct SingleCache<T, A: SlabAllocator = DefaultAllocator> {
    canary: Canary<{ magic(b"obj$") }>,
    reserve_slabs: usize,
    slab_free_ops_in_flight: AtomicU32,
    #[mutex]
    lock: KMutex<RawMutex>,
    #[guarded_by(lock)]
    slab_count: usize,
    // Lists of slabs in the object cache with the following functions:
    //  - The partial list contains slabs with some allocated objects and some
    //    entries in the free list.
    //  - The empty list contains retained slabs with no allocated objects and all
    //    entries in the free list.
    //  - The full list contains slabs with all objects allocated and no entries
    //    in the free list.
    //
    // Allocation from slabs in the partial list is preferred over the empty list
    // to reduce fragmentation.
    #[pin]
    #[guarded_by(lock)]
    partial_list: SlabList<T, A>,
    #[pin]
    #[guarded_by(lock)]
    empty_list: SlabList<T, A>,
    #[pin]
    #[guarded_by(lock)]
    full_list: SlabList<T, A>,
    // Placed last so `allocator` outlives the slab lists during field drop.
    allocator: A,
}

// SAFETY: `SingleCache` synchronizes all mutable state via `lock` and atomics.
unsafe impl<T: Send, A: SlabAllocator + Send + Sync> Send for SingleCache<T, A> {}
// SAFETY: `SingleCache` synchronizes all shared access via `lock` and atomics.
unsafe impl<T: Send, A: SlabAllocator + Send + Sync> Sync for SingleCache<T, A> {}

#[pinned_drop]
impl<T, A: SlabAllocator> PinnedDrop for SingleCache<T, A> {
    fn drop(self: Pin<&mut Self>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "ObjectCache::~ObjectCache"
        );

        let this = self.as_ref().get_ref();
        {
            ksync::lock!(let guard = this.lock_lock());
            let fields = guard.fields();

            // Slabs cannot be orphaned if the allocator is stateful, since the
            // allocator state is destroyed when this destructor completes.
            debug_assert!(!A::IS_STATEFUL || fields.full_list.is_empty());
            debug_assert!(!A::IS_STATEFUL || fields.partial_list.is_empty());

            // Mark active slabs orphan. Threads racing in `Slab::free` may not observe
            // this state before attempting to acquire the cache lock.
            for slab in fields.full_list.iter() {
                slab.set_orphan();
            }
            for slab in fields.partial_list.iter() {
                slab.set_orphan();
            }
        }

        // Wait for any threads racing in `Slab::free` to release their slab locks to
        // ensure that the cache lock is not destroyed while they hold it.
        // We have exclusive `&mut Self` in `PinnedDrop` and all slabs in `full_list` and
        // `partial_list` are marked orphan, so no thread in `Slab::free` will mutate the lists.
        let mut projected = self.project();
        let mut full_list = projected.full_list.as_mut().get_pinned_mut();
        for slab in full_list.iter() {
            ksync::lock!(let _guard = slab.control.lock_lock());
        }
        let mut partial_list = projected.partial_list.as_mut().get_pinned_mut();
        for slab in partial_list.iter() {
            ksync::lock!(let _guard = slab.control.lock_lock());
        }

        // Finally, wait until the `slab_free_ops_in_flight` count hits zero. There
        // is an extremely small chance that there is a thread still in the process
        // of releasing our `lock` which we contested during the "orphan" pass above.
        // If we failed to synchronize with this thread (because we had already
        // moved our slab to the free list, or removed it entirely), the
        // `slab_free_ops_in_flight` count provides us one last way to make sure that
        // this thread has completely exited our lock before we allow it to become
        // destructed. Acq/Rel semantics here guarantee that we will properly see
        // the full release of the lock as we destruct.
        while projected.slab_free_ops_in_flight.load(Ordering::Acquire) != 0 {
            libarch::r#yield();
        }

        // Clear all slab lists explicitly while `self` and `self.allocator` are still live.
        let mut empty_list = projected.empty_list.as_mut().get_pinned_mut();
        full_list.as_mut().clear();
        empty_list.as_mut().clear();
        partial_list.as_mut().clear();
    }
}

impl<T, A: SlabAllocator> SingleCache<T, A> {
    fn new_with(reserve_slabs: usize, allocator: A) -> impl PinInit<Self, Infallible> {
        let () = Slab::<T, A>::_LAYOUT_ASSERT;
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "ObjectCache::ObjectCache"
        );
        pin_init!(Self {
            canary: Canary::new(),
            reserve_slabs,
            slab_free_ops_in_flight: AtomicU32::new(0),
            lock <- KMutex::init(),
            slab_count: KCell::new(0),
            partial_list <- KCell::pin_init(SlabList::new()),
            empty_list <- KCell::pin_init(SlabList::new()),
            full_list <- KCell::pin_init(SlabList::new()),
            allocator,
        })
    }

    fn allocate_init<E>(&self, init: impl Init<T, E>) -> Result<UniquePtr<T, A>, E>
    where
        E: From<Status>,
    {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= BASIC,
            "kernel:sched",
            "ObjectCache::Allocate"
        );
        debug_assert!(current_memory_allocation_state_is_enabled());
        let _preempt_disable = AutoPreemptDisabler::new();

        loop {
            let slab = self.get_slab().map_err(E::from)?;

            // Allocation can fail if one or more allocating threads race with a
            // thread that is either filling or releasing a slab. Retry on another
            // slab until allocation succeeds or slab allocation fails due to
            // insufficient memory.
            if let Ok(entry) = slab.allocate() {
                // SAFETY: `entry` was just allocated from `slab`, and we construct `T` outside
                // internal locks. If `init` fails, we return the uninitialized slot to the slab.
                match unsafe { Entry::to_object_init(entry, init) } {
                    Ok(object_ptr) => {
                        return Ok(UniquePtr { ptr: object_ptr, _phantom: PhantomData });
                    }
                    Err(err) => {
                        // SAFETY: `entry` has no constructed `T`; `delete_raw` reinitializes its
                        // list node and returns it to `slab`.
                        unsafe {
                            Self::delete_raw(entry.cast::<T>());
                        }
                        return Err(err);
                    }
                }
            }
        }
    }

    fn slab_count(&self) -> usize {
        ksync::lock!(let guard = self.lock_lock());
        *guard.fields().slab_count
    }

    /// Returns the given object that has already been destroyed to the slab it was allocated from.
    ///
    /// # Safety
    ///
    /// `pointer` must point to a slot allocated from a `Slab<T, A>` whose `T` value has already
    /// been destroyed.
    unsafe fn delete_raw(pointer: NonNull<T>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= BASIC,
            "kernel:sched",
            "ObjectCache::Release"
        );
        let _preempt_disable = AutoPreemptDisabler::new();
        // SAFETY: `pointer` was allocated from a `Slab<T, A>`, and `from_allocated_pointer`
        // reclaims the object's reference to the slab.
        let slab = unsafe { Slab::<T, A>::from_allocated_pointer(pointer) };
        // SAFETY: The caller already destroyed `*pointer`, and we exclusively own its slot.
        let entry = unsafe { Entry::<T>::to_list_node(pointer) };
        slab.free(entry);
        // Release the freed object's reference to the slab.
        drop(slab);
    }

    // Returns a reference to a slab with at least one available entry. Allocates
    // a new slab if no slabs have available entries.
    fn get_slab(&self) -> Result<RefPtr<Slab<T, A>>, Status> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::GetSlab"
        );
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.fields();

        if fields.partial_list.is_empty() && fields.empty_list.is_empty() {
            return self.allocate_slab_locked(&mut guard.as_mut().fields_mut());
        }

        let slab_ref = if fields.partial_list.is_empty() {
            fields.empty_list.front().expect("empty_list not empty")
        } else {
            fields.partial_list.front().expect("partial_list not empty")
        };
        Ok(RefPtr::from_ref(slab_ref))
    }

    // Allocates a new slab and adds it to the empty list.
    fn allocate_slab_locked(
        &self,
        fields: &mut SingleCacheLockFieldsMut<'_, T, A>,
    ) -> Result<RefPtr<Slab<T, A>>, Status> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::AllocateSlab"
        );

        let base = self.allocator.allocate()?;
        self.allocator.count_slab_allocation();

        let slab = Slab::try_new(NonNull::from(self), base);
        fields.empty_list.as_mut().push_front(slab.clone());
        *fields.slab_count += 1;

        Ok(slab)
    }

    // Removes the given slab from this cache.
    fn remove_slab_locked(fields: &mut SingleCacheLockFieldsMut<'_, T, A>, slab: &Slab<T, A>) {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_LEVEL >= DETAIL,
            "kernel:sched",
            "Slab::RemoveSlab"
        );
        // SAFETY: `slab` is in `empty_list` when `remove_slab_locked` is called.
        let erased = unsafe { fields.empty_list.as_mut().erase(slab) };
        debug_assert!(erased.is_some());
        *fields.slab_count -= 1;
    }

    // Predicate indicating whether the number of slabs is greater than the reserve value.
    fn should_trim_locked(&self, fields: &SingleCacheLockFieldsMut<'_, T, A>) -> bool {
        *fields.slab_count > self.reserve_slabs
    }
}

/// Internal implementation of `ObjectCache<T, PerCpu, A>`.
pub struct PerCpuCache<T, A: SlabAllocator = DefaultAllocator> {
    processor_count: usize,
    cpu_caches: Option<Pin<Array<ObjectCache<T, Single, A>>>>,
}

impl<T, A: SlabAllocator> Default for PerCpuCache<T, A> {
    fn default() -> Self {
        Self { processor_count: 0, cpu_caches: None }
    }
}

/// A power-of-two slab allocator cache for objects of type `T`.
///
/// Use `Single` (the default) for a single shared cache or `PerCpu` for per-CPU slab caches.
#[pin_data]
pub struct ObjectCache<T, M: CacheMode<T, A> = Single, A: SlabAllocator = DefaultAllocator> {
    #[pin]
    inner: M::Inner,
}

impl<T, M: CacheMode<T, A>, A: SlabAllocator> ObjectCache<T, M, A> {
    /// Number of objects that fit in a single slab.
    pub const OBJECTS_PER_SLAB: usize = Slab::<T, A>::ENTRIES_PER_SLAB;

    /// Size of the slab control block in bytes.
    pub const SLAB_CONTROL_SIZE: usize = Slab::<T, A>::SLAB_CONTROL_SIZE;

    /// Size of a single slab entry in bytes.
    pub const ENTRY_SIZE: usize = size_of::<Entry<T>>();
}

impl<T, A: SlabAllocator> ObjectCache<T, Single, A> {
    /// Creates an in-place initializer for a single `ObjectCache` using `A::default()`.
    ///
    /// Reserve slabs are not immediately allocated.
    pub fn new(reserve_slabs: usize) -> impl PinInit<Self, Infallible>
    where
        A: Default,
    {
        Self::new_with(reserve_slabs, A::default())
    }

    /// Creates an in-place initializer for a single `ObjectCache` with the given `allocator`.
    ///
    /// Reserve slabs are not immediately allocated.
    pub fn new_with(reserve_slabs: usize, allocator: A) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            inner <- SingleCache::new_with(reserve_slabs, allocator),
        })
    }

    /// Allocates an instance of `T` from a slab and initializes it with `value`.
    ///
    /// If `T` is reference-counted, it is not yet adopted; use [`UniquePtr::into_ref_ptr`] to
    /// adopt it into a `fbl::RefPtr<T>`.
    pub fn allocate(&self, value: T) -> Result<UniquePtr<T, A>, Status> {
        // SAFETY: The closure writes `value` into `slot` and returns `Ok(())`.
        let init = unsafe {
            init_from_closure(move |slot: *mut T| -> Result<(), Status> {
                slot.write(value);
                Ok(())
            })
        };
        self.allocate_init(init)
    }

    /// Allocates an instance of `T` from a slab and initializes it in place with `init` outside
    /// of internal cache locks.
    pub fn allocate_init<E>(&self, init: impl Init<T, E>) -> Result<UniquePtr<T, A>, E>
    where
        E: From<Status>,
    {
        self.inner.allocate_init(init)
    }

    /// Returns the number of slabs currently held by this cache.
    pub fn slab_count(&self) -> usize {
        self.inner.slab_count()
    }

    /// Returns a shared reference to the underlying slab allocator.
    pub fn allocator(&self) -> &A {
        &self.inner.allocator
    }

    /// Returns an already-destroyed object slot to the slab it was allocated from.
    ///
    /// # Safety
    ///
    /// `pointer` must point to a slot allocated from an `ObjectCache<T, _, A>` whose `T` value
    /// has already been dropped in place and has not yet been returned to its slab.
    pub unsafe fn delete_raw(pointer: NonNull<T>) {
        // SAFETY: Caller upholds the `delete_raw` safety preconditions.
        unsafe { SingleCache::<T, A>::delete_raw(pointer) };
    }
}

impl<T, A: SlabAllocator> Default for ObjectCache<T, PerCpu, A> {
    fn default() -> Self {
        Self { inner: PerCpuCache::default() }
    }
}

impl<T, A: SlabAllocator> ObjectCache<T, PerCpu, A> {
    /// Creates a per-CPU `ObjectCache` using `A::default()` for each CPU cache.
    ///
    /// The `reserve_slabs` value applies to each per-CPU cache independently. Reserve slabs are
    /// not immediately allocated.
    pub fn try_new(reserve_slabs: usize) -> Result<Self, Status>
    where
        A: Default,
    {
        Self::try_new_with(reserve_slabs, A::default)
    }

    /// Creates a per-CPU `ObjectCache` constructing each CPU's allocator with `factory`.
    ///
    /// The `reserve_slabs` value applies to each per-CPU cache independently. Reserve slabs are
    /// not immediately allocated.
    pub fn try_new_with(
        reserve_slabs: usize,
        mut factory: impl FnMut() -> A,
    ) -> Result<Self, Status> {
        let processor_count = get_processor_count();
        let cpu_caches = Array::try_new_pin_init(processor_count, || {
            ObjectCache::<T, Single, A>::new_with(reserve_slabs, factory())
        })
        .map_err(|_| Status::NO_MEMORY)?;

        Ok(Self { inner: PerCpuCache { processor_count, cpu_caches: Some(cpu_caches) } })
    }

    /// Allocates an instance of `T` from the current CPU's slab cache and initializes it with
    /// `value`.
    ///
    /// If `T` is reference-counted, it is not yet adopted; use [`UniquePtr::into_ref_ptr`] to
    /// adopt it into a `fbl::RefPtr<T>`.
    pub fn allocate(&self, value: T) -> Result<UniquePtr<T, A>, Status> {
        // SAFETY: The closure writes `value` into `slot` and returns `Ok(())`.
        let init = unsafe {
            init_from_closure(move |slot: *mut T| -> Result<(), Status> {
                slot.write(value);
                Ok(())
            })
        };
        self.allocate_init(init)
    }

    /// Allocates an instance of `T` from the current CPU's slab cache and initializes it in place
    /// with `init` outside of internal cache locks.
    pub fn allocate_init<E>(&self, init: impl Init<T, E>) -> Result<UniquePtr<T, A>, E>
    where
        E: From<Status>,
    {
        debug_assert!(self.inner.cpu_caches.is_some());
        debug_assert!(current_memory_allocation_state_is_enabled());

        let _preempt_disable = AutoPreemptDisabler::new();
        let current_cpu = curr_cpu_num() as usize;
        debug_assert!(current_cpu < self.inner.processor_count);
        let cpu_caches = self.inner.cpu_caches.as_ref().expect("cpu_caches initialized");
        cpu_caches[current_cpu].allocate_init(init)
    }

    /// Returns the total number of slabs currently held across all per-CPU caches.
    pub fn slab_count(&self) -> usize {
        let mut count = 0;
        for i in 0..self.inner.processor_count {
            count += self.inner.cpu_caches.as_ref().unwrap()[i].slab_count();
        }
        count
    }
}

/// object_cache tests
#[cfg(ktest)]
#[unittest::suite(name = "object_cache")]
mod tests {
    use crate::kernel::event::{AutounsignalEvent, Event};
    use crate::kernel::thread::{self, ThreadPtr};
    use crate::kernel::types::{cpu_mask_t, cpu_num_to_mask};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicI32, AtomicPtr};
    use fbl::Vector;
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_true, expect_eq, expect_false, expect_true};

    static GLOBAL_ALLOCATED_SLABS: AtomicI32 = AtomicI32::new(0);
    static GLOBAL_FREED_SLABS: AtomicI32 = AtomicI32::new(0);

    #[derive(Default, Clone, Copy)]
    struct TestGlobalAllocator;

    impl TestGlobalAllocator {
        // Ignores the allocator arguments passed to the ObjectCache ctor.
        const fn new(_a: i32, _b: i32) -> Self {
            Self
        }

        fn reset_counts() {
            GLOBAL_ALLOCATED_SLABS.store(0, Ordering::SeqCst);
            GLOBAL_FREED_SLABS.store(0, Ordering::SeqCst);
        }

        fn allocated_slabs() -> i32 {
            GLOBAL_ALLOCATED_SLABS.load(Ordering::SeqCst)
        }

        fn freed_slabs() -> i32 {
            GLOBAL_FREED_SLABS.load(Ordering::SeqCst)
        }
    }

    // SAFETY: `TestGlobalAllocator` delegates `page::SIZE` aligned allocation and global release
    // to `DefaultAllocator`, allowing orphan slabs to outlive the cache.
    unsafe impl GlobalSlabAllocator for TestGlobalAllocator {
        const SLAB_SIZE: usize = DefaultAllocator::SLAB_SIZE;

        fn allocate() -> Result<NonNull<u8>, Status> {
            GLOBAL_ALLOCATED_SLABS.fetch_add(1, Ordering::Relaxed);
            DefaultAllocator::allocate()
        }

        /// # Safety
        ///
        /// `slab` must be the base address of a slab allocated from `DefaultAllocator`.
        unsafe fn release(slab: NonNull<u8>) {
            GLOBAL_FREED_SLABS.fetch_add(1, Ordering::Relaxed);
            // SAFETY: Caller guarantees `slab` came from `DefaultAllocator::allocate`.
            unsafe { DefaultAllocator::release(slab) };
        }

        fn count_object_allocation() {
            DefaultAllocator::count_object_allocation();
        }

        fn count_object_free() {
            DefaultAllocator::count_object_free();
        }

        fn count_slab_allocation() {
            DefaultAllocator::count_slab_allocation();
        }

        fn count_slab_free() {
            DefaultAllocator::count_slab_free();
        }
    }

    static LOCAL_TOTAL_ALLOCATED_SLABS: AtomicI32 = AtomicI32::new(0);
    static LOCAL_TOTAL_FREED_SLABS: AtomicI32 = AtomicI32::new(0);

    struct TestLocalAllocator {
        local_state_a: i32,
        local_state_b: i32,
        local_allocated_slabs: AtomicI32,
        local_freed_slabs: AtomicI32,
    }

    impl TestLocalAllocator {
        // Stores the allocator arguments passed to the ObjectCache ctor for verification.
        const fn new(a: i32, b: i32) -> Self {
            Self {
                local_state_a: a,
                local_state_b: b,
                local_allocated_slabs: AtomicI32::new(0),
                local_freed_slabs: AtomicI32::new(0),
            }
        }

        fn reset_counts() {
            LOCAL_TOTAL_ALLOCATED_SLABS.store(0, Ordering::SeqCst);
            LOCAL_TOTAL_FREED_SLABS.store(0, Ordering::SeqCst);
        }

        fn allocated_slabs() -> i32 {
            LOCAL_TOTAL_ALLOCATED_SLABS.load(Ordering::SeqCst)
        }

        fn freed_slabs() -> i32 {
            LOCAL_TOTAL_FREED_SLABS.load(Ordering::SeqCst)
        }
    }

    // SAFETY: `TestLocalAllocator` delegates `page::SIZE` aligned allocation to `DefaultAllocator`
    // and sets `IS_STATEFUL = true` because it tracks per-instance slab counters.
    unsafe impl SlabAllocator for TestLocalAllocator {
        const SLAB_SIZE: usize = DefaultAllocator::SLAB_SIZE;
        const IS_STATEFUL: bool = true;

        fn allocate(&self) -> Result<NonNull<u8>, Status> {
            LOCAL_TOTAL_ALLOCATED_SLABS.fetch_add(1, Ordering::Relaxed);
            self.local_allocated_slabs.fetch_add(1, Ordering::Relaxed);
            DefaultAllocator::allocate()
        }

        /// # Safety
        ///
        /// `slab` must be the base address of a slab allocated from `DefaultAllocator`.
        unsafe fn release(&self, slab: NonNull<u8>) {
            LOCAL_TOTAL_FREED_SLABS.fetch_add(1, Ordering::Relaxed);
            self.local_freed_slabs.fetch_add(1, Ordering::Relaxed);
            // SAFETY: Caller guarantees `slab` came from `DefaultAllocator::allocate`.
            unsafe { DefaultAllocator::release(slab) };
        }

        fn count_object_allocation(&self) {
            DefaultAllocator::count_object_allocation();
        }

        fn count_object_free(&self) {
            DefaultAllocator::count_object_free();
        }

        fn count_slab_allocation(&self) {
            DefaultAllocator::count_slab_allocation();
        }

        fn count_slab_free(&self) {
            DefaultAllocator::count_slab_free();
        }
    }

    static TEST_OBJECT_CONSTRUCTOR_COUNT: AtomicI32 = AtomicI32::new(0);
    static TEST_OBJECT_DESTRUCTOR_COUNT: AtomicI32 = AtomicI32::new(0);

    #[repr(C)]
    struct TestObject {
        data: i32,
        _extra: [u8; 256 - size_of::<i32>()],
    }

    zr::static_assert!(size_of::<TestObject>() == 256);

    impl TestObject {
        fn new(data: i32) -> Self {
            TEST_OBJECT_CONSTRUCTOR_COUNT.fetch_add(1, Ordering::SeqCst);
            Self { data, _extra: [0; 256 - size_of::<i32>()] }
        }

        fn reset_counts() {
            TEST_OBJECT_CONSTRUCTOR_COUNT.store(0, Ordering::SeqCst);
            TEST_OBJECT_DESTRUCTOR_COUNT.store(0, Ordering::SeqCst);
        }

        fn constructor_count() -> i32 {
            TEST_OBJECT_CONSTRUCTOR_COUNT.load(Ordering::SeqCst)
        }

        fn destructor_count() -> i32 {
            TEST_OBJECT_DESTRUCTOR_COUNT.load(Ordering::SeqCst)
        }
    }

    impl Drop for TestObject {
        fn drop(&mut self) {
            TEST_OBJECT_DESTRUCTOR_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    static TEST_PARENT_DESTRUCTOR_COUNT: AtomicI32 = AtomicI32::new(0);

    #[fbl::ref_counted]
    #[pin_data(PinnedDrop)]
    #[derive(fbl::Recyclable)]
    #[repr(C)]
    struct TestParent {
        #[pin]
        allocator: ObjectCache<Child, Single, TestGlobalAllocator>,
    }

    #[pinned_drop]
    impl PinnedDrop for TestParent {
        fn drop(self: Pin<&mut Self>) {
            TEST_PARENT_DESTRUCTOR_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[fbl::ref_counted]
    #[repr(C)]
    struct Child {
        parent: RefPtr<TestParent>,
    }

    impl_object_cache_deletable!(Child, TestGlobalAllocator);

    impl Child {
        fn new(parent: RefPtr<TestParent>) -> Self {
            Self { ref_count: RefCounted::new(), __fbl_ref_counted_guard: (), parent }
        }
    }

    impl TestParent {
        const OBJECTS_PER_SLAB: usize =
            ObjectCache::<Child, Single, TestGlobalAllocator>::OBJECTS_PER_SLAB;
        const RESERVE_SLABS: usize = 1;

        fn try_new() -> Result<RefPtr<Self>, Status> {
            fbl::pin_make_ref_counted!(TestParent {
                allocator <- ObjectCache::new(Self::RESERVE_SLABS),
            })
            .map_err(|_| Status::NO_MEMORY)
        }

        fn allocate(this: &RefPtr<Self>) -> Result<RefPtr<Child>, Status> {
            let child = this.allocator.allocate(Child::new(this.clone()))?;
            Ok(UniquePtr::into_ref_ptr(child))
        }

        fn reset_counts() {
            TEST_PARENT_DESTRUCTOR_COUNT.store(0, Ordering::SeqCst);
        }

        fn destructor_count() -> i32 {
            TEST_PARENT_DESTRUCTOR_COUNT.load(Ordering::SeqCst)
        }
    }

    struct AffinityGuard {
        thread: ThreadPtr,
        original_mask: cpu_mask_t,
    }

    impl AffinityGuard {
        // Stay on one CPU during the tests to verify numeric properties of a single per-CPU
        // cache. Accounting for CPU migration during the tests would make them overly complicated
        // for little value.
        fn pin_to_current_cpu() -> Self {
            // SAFETY: Tests run after threading is initialized on a live kernel thread.
            let thread = unsafe { ThreadPtr::current() };
            // SAFETY: `thread` is the live current thread.
            let original_mask = unsafe { thread.get_cpu_affinity() };
            {
                let _preempt_disable = AutoPreemptDisabler::new();
                let current_cpu = curr_cpu_num();
                // SAFETY: `thread` is the live current thread.
                unsafe {
                    thread.set_cpu_affinity(cpu_num_to_mask(current_cpu));
                }
            }
            Self { thread, original_mask }
        }
    }

    impl Drop for AffinityGuard {
        fn drop(&mut self) {
            // SAFETY: `self.thread` is the live current thread.
            unsafe {
                self.thread.set_cpu_affinity(self.original_mask);
            }
        }
    }

    trait TestAllocatorCounts: SlabAllocator {
        fn reset_counts();
        fn total_allocated_slabs() -> i32;
        fn total_freed_slabs() -> i32;
        fn make(a: i32, b: i32) -> Self;
        fn check_local_args(&self, _a: i32, _b: i32) -> bool {
            true
        }
        fn check_local_counts(&self, _allocated: i32, _freed: i32, _slab_count: usize) -> bool {
            true
        }
    }

    impl TestAllocatorCounts for TestGlobalAllocator {
        fn reset_counts() {
            Self::reset_counts();
        }
        fn total_allocated_slabs() -> i32 {
            Self::allocated_slabs()
        }
        fn total_freed_slabs() -> i32 {
            Self::freed_slabs()
        }
        fn make(a: i32, b: i32) -> Self {
            Self::new(a, b)
        }
    }

    impl TestAllocatorCounts for TestLocalAllocator {
        fn reset_counts() {
            Self::reset_counts();
        }
        fn total_allocated_slabs() -> i32 {
            Self::allocated_slabs()
        }
        fn total_freed_slabs() -> i32 {
            Self::freed_slabs()
        }
        fn make(a: i32, b: i32) -> Self {
            Self::new(a, b)
        }
        fn check_local_args(&self, a: i32, b: i32) -> bool {
            (unittest::subtest!(|this: &Self, a: i32, b: i32| {
                expect_eq!(a, this.local_state_a);
                expect_eq!(b, this.local_state_b);
            }))(self, a, b)
        }
        fn check_local_counts(&self, allocated: i32, freed: i32, slab_count: usize) -> bool {
            (unittest::subtest!(|this: &Self, allocated: i32, freed: i32, slab_count: usize| {
                let local_alloc = this.local_allocated_slabs.load(Ordering::SeqCst);
                let local_free = this.local_freed_slabs.load(Ordering::SeqCst);
                expect_eq!(allocated, local_alloc);
                expect_eq!(freed, local_free);
                expect_eq!(local_alloc - local_free, slab_count as i32);
            }))(self, allocated, freed, slab_count)
        }
    }

    trait TestCache<A: TestAllocatorCounts> {
        fn allocate(&self, value: TestObject) -> Result<UniquePtr<TestObject, A>, Status>;
        fn slab_count(&self) -> usize;
        fn check_local_args(&self, _a: i32, _b: i32) -> bool {
            true
        }
        fn check_local_counts(&self, _allocated: i32, _freed: i32, _slab_count: usize) -> bool {
            true
        }
    }

    impl<A: TestAllocatorCounts> TestCache<A> for Pin<Array<ObjectCache<TestObject, Single, A>>> {
        fn allocate(&self, value: TestObject) -> Result<UniquePtr<TestObject, A>, Status> {
            self[0].allocate(value)
        }

        fn slab_count(&self) -> usize {
            self[0].slab_count()
        }

        fn check_local_args(&self, a: i32, b: i32) -> bool {
            self[0].allocator().check_local_args(a, b)
        }

        fn check_local_counts(&self, allocated: i32, freed: i32, slab_count: usize) -> bool {
            self[0].allocator().check_local_counts(allocated, freed, slab_count)
        }
    }

    impl<A: TestAllocatorCounts> TestCache<A> for ObjectCache<TestObject, PerCpu, A> {
        fn allocate(&self, value: TestObject) -> Result<UniquePtr<TestObject, A>, Status> {
            ObjectCache::<TestObject, PerCpu, A>::allocate(self, value)
        }

        fn slab_count(&self) -> usize {
            ObjectCache::<TestObject, PerCpu, A>::slab_count(self)
        }
    }

    trait TestCacheMode<A: TestAllocatorCounts>: Sized {
        type Cache: TestCache<A>;

        fn create_cache(retain_slabs: usize) -> Option<Self::Cache>;
    }

    impl<A: TestAllocatorCounts> TestCacheMode<A> for Single {
        type Cache = Pin<Array<ObjectCache<TestObject, Single, A>>>;

        fn create_cache(retain_slabs: usize) -> Option<Self::Cache> {
            // Heap-pin in a 1-element `Array` since a pinned `ObjectCache<_, Single, _>` cannot be
            // returned by value.
            Array::try_new_pin_init(1, || {
                ObjectCache::<TestObject, Single, A>::new_with(retain_slabs, A::make(10, 20))
            })
            .ok()
        }
    }

    impl<A: TestAllocatorCounts> TestCacheMode<A> for PerCpu {
        type Cache = ObjectCache<TestObject, PerCpu, A>;

        fn create_cache(retain_slabs: usize) -> Option<Self::Cache> {
            ObjectCache::<TestObject, PerCpu, A>::try_new_with(retain_slabs, || A::make(10, 20))
                .ok()
        }
    }

    fn object_cache_tests<
        const RETAIN_SLABS: usize,
        const SLAB_COUNT: usize,
        M: TestCacheMode<A>,
        A: TestAllocatorCounts,
    >() -> bool {
        (unittest::subtest!(|retain_slabs: usize, slab_count: usize| {
            TestObject::reset_counts();
            assert_eq!(0, TestObject::constructor_count());
            assert_eq!(0, TestObject::destructor_count());

            A::reset_counts();
            assert_eq!(0, A::total_allocated_slabs());
            assert_eq!(0, A::total_freed_slabs());

            let objects_per_slab = ObjectCache::<TestObject>::OBJECTS_PER_SLAB;
            let object_count = objects_per_slab * slab_count;

            let mut objects = Vector::<Option<UniquePtr<TestObject, A>>>::new();
            assert_true!(objects.reserve(object_count).is_ok());

            {
                let object_cache = M::create_cache(retain_slabs);
                assert_true!(object_cache.is_some());
                let object_cache = object_cache.unwrap();
                expect_true!(object_cache.check_local_args(10, 20));

                // Stay on one CPU during the following tests to verify numeric properties of a
                // single per-CPU cache. Accounting for CPU migration during the tests would make
                // them overly complicated for little value.
                let _affinity = AffinityGuard::pin_to_current_cpu();

                expect_eq!(0usize, object_cache.slab_count());
                expect_eq!(0, TestObject::constructor_count());
                expect_eq!(0, TestObject::destructor_count());

                for i in 0..object_count {
                    let result = object_cache.allocate(TestObject::new(i as i32));
                    assert_true!(result.is_ok());
                    let ptr = result.unwrap();
                    expect_eq!((i + 1) as i32, TestObject::constructor_count());
                    expect_eq!(0, TestObject::destructor_count());
                    expect_eq!(i as i32, ptr.data);

                    assert_true!(objects.push_back(Some(ptr)).is_ok());
                }
                expect_eq!(slab_count, object_cache.slab_count());
                expect_eq!(slab_count as i32, A::total_allocated_slabs());
                expect_eq!(0, A::total_freed_slabs());
                expect_true!(object_cache.check_local_counts(
                    slab_count as i32,
                    0,
                    object_cache.slab_count(),
                ));

                // Release the first slab worth of objects.
                for i in 0..objects_per_slab {
                    objects[i] = None;
                    expect_eq!(object_count as i32, TestObject::constructor_count());
                    expect_eq!((i + 1) as i32, TestObject::destructor_count());
                }
                let expected_freed = if slab_count <= retain_slabs { 0 } else { 1 };
                expect_eq!(slab_count as i32, A::total_allocated_slabs());
                expect_eq!(expected_freed, A::total_freed_slabs());
                expect_eq!(
                    A::total_allocated_slabs() - A::total_freed_slabs(),
                    object_cache.slab_count() as i32
                );
                expect_true!(object_cache.check_local_counts(
                    slab_count as i32,
                    expected_freed,
                    object_cache.slab_count(),
                ));

                objects.clear();

                expect_eq!(TestObject::constructor_count(), TestObject::destructor_count());
                expect_eq!(object_count as i32, TestObject::destructor_count());
            }
            expect_eq!(TestObject::constructor_count(), TestObject::destructor_count());
            expect_eq!(A::total_allocated_slabs(), A::total_freed_slabs());
        }))(RETAIN_SLABS, SLAB_COUNT)
    }

    /// object_cache_tests<0, 2, Single, TestLocalAllocator>
    #[test]
    fn object_cache_single_0_2_local() {
        assert_true!(object_cache_tests::<0, 2, Single, TestLocalAllocator>());
    }

    /// object_cache_tests<1, 2, Single, TestLocalAllocator>
    #[test]
    fn object_cache_single_1_2_local() {
        assert_true!(object_cache_tests::<1, 2, Single, TestLocalAllocator>());
    }

    /// object_cache_tests<2, 2, Single, TestLocalAllocator>
    #[test]
    fn object_cache_single_2_2_local() {
        assert_true!(object_cache_tests::<2, 2, Single, TestLocalAllocator>());
    }

    /// object_cache_tests<0, 2, PerCpu, TestLocalAllocator>
    #[test]
    fn object_cache_per_cpu_0_2_local() {
        assert_true!(object_cache_tests::<0, 2, PerCpu, TestLocalAllocator>());
    }

    /// object_cache_tests<1, 2, PerCpu, TestLocalAllocator>
    #[test]
    fn object_cache_per_cpu_1_2_local() {
        assert_true!(object_cache_tests::<1, 2, PerCpu, TestLocalAllocator>());
    }

    /// object_cache_tests<2, 2, PerCpu, TestLocalAllocator>
    #[test]
    fn object_cache_per_cpu_2_2_local() {
        assert_true!(object_cache_tests::<2, 2, PerCpu, TestLocalAllocator>());
    }

    /// object_cache_tests<0, 2, Single, TestGlobalAllocator>
    #[test]
    fn object_cache_single_0_2_global() {
        assert_true!(object_cache_tests::<0, 2, Single, TestGlobalAllocator>());
    }

    /// object_cache_tests<1, 2, Single, TestGlobalAllocator>
    #[test]
    fn object_cache_single_1_2_global() {
        assert_true!(object_cache_tests::<1, 2, Single, TestGlobalAllocator>());
    }

    /// object_cache_tests<2, 2, Single, TestGlobalAllocator>
    #[test]
    fn object_cache_single_2_2_global() {
        assert_true!(object_cache_tests::<2, 2, Single, TestGlobalAllocator>());
    }

    /// object_cache_tests<0, 2, PerCpu, TestGlobalAllocator>
    #[test]
    fn object_cache_per_cpu_0_2_global() {
        assert_true!(object_cache_tests::<0, 2, PerCpu, TestGlobalAllocator>());
    }

    /// object_cache_tests<1, 2, PerCpu, TestGlobalAllocator>
    #[test]
    fn object_cache_per_cpu_1_2_global() {
        assert_true!(object_cache_tests::<1, 2, PerCpu, TestGlobalAllocator>());
    }

    /// object_cache_tests<2, 2, PerCpu, TestGlobalAllocator>
    #[test]
    fn object_cache_per_cpu_2_2_global() {
        assert_true!(object_cache_tests::<2, 2, PerCpu, TestGlobalAllocator>());
    }

    /// backreference_lifetime_tests
    #[test]
    fn backreference_lifetime_tests() {
        TestParent::reset_counts();
        expect_eq!(0, TestParent::destructor_count());

        TestGlobalAllocator::reset_counts();
        assert_eq!(0, TestGlobalAllocator::allocated_slabs());
        assert_eq!(0, TestGlobalAllocator::freed_slabs());

        let parent = TestParent::try_new();
        assert_true!(parent.is_ok());
        let parent = parent.unwrap();

        let result1 = TestParent::allocate(&parent);
        let result2 = TestParent::allocate(&parent);
        let result3 = TestParent::allocate(&parent);
        assert_true!(result1.is_ok());
        assert_true!(result2.is_ok());
        assert_true!(result3.is_ok());

        drop(parent);
        expect_eq!(0, TestParent::destructor_count());

        drop(result1);
        expect_eq!(0, TestParent::destructor_count());

        drop(result2);
        expect_eq!(0, TestParent::destructor_count());

        drop(result3);
        expect_eq!(1, TestParent::destructor_count());

        expect_eq!(TestGlobalAllocator::allocated_slabs(), TestGlobalAllocator::freed_slabs());
    }

    #[pin_data]
    struct StressControl {
        parent: AtomicPtr<TestParent>,
        #[pin]
        allocation_event: AutounsignalEvent,
        #[pin]
        deallocation_event: Event,
        #[pin]
        finished_event: AutounsignalEvent,
        failed: AtomicBool,
        count: AtomicI32,
    }

    extern "C" fn stress_worker_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a `StressControl` pinned on the caller's stack that outlives all
        // worker threads (which are joined before `StressControl` goes out of scope).
        let control = unsafe { &*(arg as *const StressControl) };
        let object_count = TestParent::OBJECTS_PER_SLAB;

        let mut objects = Vector::<Option<RefPtr<Child>>>::new();
        if objects.reserve(object_count).is_err() {
            control.failed.store(true, Ordering::SeqCst);
            return Status::NO_MEMORY.into_raw();
        }

        for _ in 0..object_count {
            let parent_ptr = control.parent.load(Ordering::SeqCst);
            // SAFETY: `parent_ptr` is non-null and kept alive by the main thread during the
            // allocation phase.
            let parent_ref = RefPtr::from_ref(unsafe { &*parent_ptr });
            let child = match TestParent::allocate(&parent_ref) {
                Ok(child) => child,
                Err(status) => {
                    control.failed.store(true, Ordering::SeqCst);
                    return status.into_raw();
                }
            };

            if objects.push_back(Some(child)).is_err() {
                control.failed.store(true, Ordering::SeqCst);
                return Status::NO_MEMORY.into_raw();
            }

            control.count.fetch_add(1, Ordering::SeqCst);
            thread::r#yield();
        }

        control.allocation_event.signal();
        let _ = control.deallocation_event.wait_infinite();

        for i in 0..object_count {
            objects[i] = None;
            control.count.fetch_sub(1, Ordering::SeqCst);
            thread::r#yield();
        }

        control.finished_event.signal();
        zx_status::sys::ZX_OK
    }

    /// backreference_lifetime_stress_tests
    #[test]
    fn backreference_lifetime_stress_tests() {
        const STRESS_TEST_ITERATIONS: usize = 1000;
        const THREAD_COUNT: usize = 8;

        for _ in 0..STRESS_TEST_ITERATIONS {
            TestParent::reset_counts();
            expect_eq!(0, TestParent::destructor_count());

            TestGlobalAllocator::reset_counts();
            assert_eq!(0, TestGlobalAllocator::allocated_slabs());
            assert_eq!(0, TestGlobalAllocator::freed_slabs());

            let parent = TestParent::try_new();
            assert_true!(parent.is_ok());
            let parent = parent.unwrap();

            let object_count = TestParent::OBJECTS_PER_SLAB;
            let parent_raw = RefPtr::as_ptr(&parent).cast_mut();
            stack_pin_init!(let control = pin_init!(StressControl {
                parent: AtomicPtr::new(parent_raw),
                allocation_event <- AutounsignalEvent::init_unsignaled(),
                deallocation_event <- Event::init_unsignaled(),
                finished_event <- AutounsignalEvent::init_unsignaled(),
                failed: AtomicBool::new(false),
                count: AtomicI32::new(0),
            }));

            let control_ptr = (&raw const *control).cast_mut().cast::<c_void>();
            let mut threads: [Option<ThreadPtr>; THREAD_COUNT] = [None; THREAD_COUNT];
            for slot in &mut threads {
                // SAFETY: `stress_worker_entry` only accesses `control`, which outlives every
                // worker thread because we join all threads before `control` drops.
                let created = unsafe {
                    thread::create(c"ObjectCacheRace".as_ptr(), stress_worker_entry, control_ptr)
                };
                assert_true!(created.is_ok());
                *slot = created.ok();
            }

            // Resume threads in quick succession to get maximum overlap in the allocation
            // phase.
            for slot in &threads {
                let thread = slot.expect("thread created");
                // SAFETY: `thread` was just created and has not been resumed or joined yet.
                unsafe {
                    thread.resume();
                }
            }

            // Wait for each worker to finish allocating children.
            let target_count = (THREAD_COUNT * object_count) as i32;
            while control.count.load(Ordering::SeqCst) != target_count
                && !control.failed.load(Ordering::SeqCst)
            {
                let _ = control.allocation_event.wait_infinite();
            }
            expect_false!(control.failed.load(Ordering::SeqCst));
            expect_eq!(THREAD_COUNT as i32, TestGlobalAllocator::allocated_slabs());
            expect_eq!(0, TestGlobalAllocator::freed_slabs());

            // Workers should not touch the parent object after they finish allocating
            // children.
            control.parent.store(core::ptr::null_mut(), Ordering::SeqCst);
            expect_eq!(0, TestParent::destructor_count());

            // Children should maintain the lifetime of the parent.
            drop(parent);
            expect_eq!(0, TestParent::destructor_count());

            control.deallocation_event.signal();

            // Wait for each worker to finish freeing children.
            while control.count.load(Ordering::SeqCst) != 0
                && !control.failed.load(Ordering::SeqCst)
            {
                let _ = control.finished_event.wait_infinite();
            }
            expect_false!(control.failed.load(Ordering::SeqCst));
            expect_eq!(1, TestParent::destructor_count());
            expect_eq!(THREAD_COUNT as i32, TestGlobalAllocator::allocated_slabs());
            expect_eq!(THREAD_COUNT as i32, TestGlobalAllocator::freed_slabs());

            for slot in threads {
                let thread = slot.expect("thread created");
                // SAFETY: Each thread was resumed once and has not been joined yet.
                let retcode = unsafe { thread.join(InstantMono::INFINITE) };
                assert_true!(retcode.is_ok());
                expect_eq!(zx_status::sys::ZX_OK, retcode.unwrap());
            }
        }
    }
}
