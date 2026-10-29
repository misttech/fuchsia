// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::DispatcherOps;
use super::channel_dispatcher::ChannelDispatcher;
use super::dispatcher::Dispatcher;
use super::process_dispatcher::ProcessDispatcher;
use crate::counters::define_kcounter;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use fbl::{DoublyLinkedListNode, HasRefCount, Recyclable, RefPtr};
use relaxed_atomic::{RelaxedAtomicU32, RelaxedAtomicU64};
use zx_status::Status;
use zx_types::{ZX_KOID_INVALID, zx_handle_t, zx_koid_t, zx_rights_t, zx_status_t};

/// Number of bits reserved in handle values for userspace/fixed tags.
pub(crate) const HANDLE_RESERVED_BITS: u32 = 2;

// The number of outstanding (live) handles in the arena.
const MAX_HANDLE_COUNT: usize = 256 * 1024;

// Warning level: When the number of handles exceeds this value, we start to emit
// warnings to the kernel's debug log.
const HIGH_HANDLE_COUNT: usize = (MAX_HANDLE_COUNT * 7) / 8;

define_kcounter!(HANDLE_COUNT_MADE, "handles.made", Sum);
define_kcounter!(HANDLE_COUNT_DUPED, "handles.duped", Sum);
define_kcounter!(HANDLE_COUNT_LIVE, "handles.live", Sum);
define_kcounter!(HANDLE_COUNT_ALLOC_FAILED, "handles.alloc.failed", Sum);

// Masks for building a Handle's base_value, which ProcessDispatcher
// uses to create zx_handle_t values.
//
// base_value bit fields:
//   [31..(32 - HANDLE_RESERVED_BITS)]                     : Must be zero
//   [(31 - HANDLE_RESERVED_BITS)..HANDLE_GENERATION_SHIFT] : Generation number
//                                                          Masked by HANDLE_GENERATION_MASK
//   [HANDLE_GENERATION_SHIFT-1..0]                        : Index into handle_arena
//                                                          Masked by HANDLE_INDEX_MASK
const HANDLE_INDEX_MASK: u32 = (MAX_HANDLE_COUNT as u32) - 1;

const HANDLE_RESERVED_BITS_MASK: u32 =
    ((1 << HANDLE_RESERVED_BITS) - 1) << (32 - HANDLE_RESERVED_BITS);

const HANDLE_GENERATION_MASK: u32 = !HANDLE_INDEX_MASK & !HANDLE_RESERVED_BITS_MASK;

const HANDLE_GENERATION_SHIFT: u32 = 18;

pub(crate) const HANDLE_MUST_BE_ONE_MASK: u32 = (1 << HANDLE_RESERVED_BITS) - 1;

pub(crate) const BASE_VALUE_MUST_BE_ZERO_MASK: u32 =
    HANDLE_MUST_BE_ONE_MASK << (32 - HANDLE_RESERVED_BITS);

// The leading members of `Handle` (`handle_table_id`, `dispatcher`, `rights` and `base_value`)
// need to be preserved when handles are free'd to the arena. HANDLE_PRESERVE_SIZE is an
// 'approximation' of how large all those members are; the layout assertions below `Handle`
// statically validate that the chosen HANDLE_PRESERVE_SIZE is correct. Any incorrect size will
// result in a compilation error! This must match `Handle::PreserveSize` in handle.h, which
// parameterizes the C++ `fbl::GPArena` backing the handles.
const HANDLE_PRESERVE_SIZE: usize = 24;

zr::static_assert!((HANDLE_INDEX_MASK & (MAX_HANDLE_COUNT as u32)) == 0);
zr::static_assert!(
    ((3 << (HANDLE_GENERATION_SHIFT - 1)) & HANDLE_GENERATION_MASK)
        == (1 << HANDLE_GENERATION_SHIFT)
);
zr::static_assert!((HANDLE_GENERATION_MASK >> HANDLE_GENERATION_SHIFT) >= 255);
zr::static_assert!(HANDLE_RESERVED_BITS_MASK & HANDLE_GENERATION_MASK == 0);
zr::static_assert!(HANDLE_RESERVED_BITS_MASK & HANDLE_INDEX_MASK == 0);
zr::static_assert!(HANDLE_GENERATION_MASK & HANDLE_INDEX_MASK == 0);
zr::static_assert!(
    (HANDLE_RESERVED_BITS_MASK | HANDLE_GENERATION_MASK | HANDLE_INDEX_MASK) == 0xffff_ffff
);

/// Calculates a new base value for an arena slot given its index and previous base value.
///
/// `index` is the literal index into the table. `old_value` is the
/// index mixed with the per-handle-lifetime state.
fn new_handle_value(index: u32, old_value: u32) -> u32 {
    // `index` is the literal index into the table. `old_value` is the index mixed with the
    // per-handle-lifetime state.
    debug_assert!((index & !HANDLE_INDEX_MASK) == 0);

    let mut old_gen = 0;
    if old_value != 0 {
        // This slot has been used before.
        debug_assert!((old_value & HANDLE_INDEX_MASK) == index);
        old_gen = (old_value & HANDLE_GENERATION_MASK) >> HANDLE_GENERATION_SHIFT;
    }
    let new_gen = ((old_gen + 1) << HANDLE_GENERATION_SHIFT) & HANDLE_GENERATION_MASK;
    index | new_gen
}

/// Converts a base value to its arena slot index.
fn handle_value_to_index(value: u32) -> u32 {
    value & HANDLE_INDEX_MASK
}

/// A wrapper around a handle value received from userspace.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct HandleValue {
    value: zx_handle_t,
}

impl HandleValue {
    /// Constructs a new `HandleValue` from a raw handle value.
    pub const fn new(value: zx_handle_t) -> Self {
        Self { value }
    }

    /// Returns the underlying raw handle value.
    #[inline]
    pub fn raw_value(&self) -> zx_handle_t {
        self.value
    }
}

impl From<zx_handle_t> for HandleValue {
    #[inline]
    fn from(value: zx_handle_t) -> Self {
        Self::new(value)
    }
}

impl From<HandleValue> for zx_handle_t {
    #[inline]
    fn from(val: HandleValue) -> Self {
        val.raw_value()
    }
}

/// A `Handle` is how a specific process refers to a specific `Dispatcher`.
#[repr(C)]
#[derive(fbl::DoublyLinkedListContainable)]
pub struct Handle {
    // handle_table_id is atomic because threads from different processes can
    // access it concurrently, while holding different instances of
    // lock.
    handle_table_id: RelaxedAtomicU64,
    dispatcher: RefPtr<Dispatcher>,
    rights: zx_rights_t,
    base_value: RelaxedAtomicU32,
    #[dll_node]
    node_state: DoublyLinkedListNode<Handle>,
}

impl core::fmt::Debug for Handle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handle")
            .field("handle_table_id", &self.handle_table_id())
            .field("rights", &self.rights)
            .field("base_value", &self.base_value())
            .finish_non_exhaustive()
    }
}

// This layout is mirrored by `HandleLayoutCheck` in handle.cc, which pins the C++ view of the same
// bytes so the two definitions can never drift apart silently.
zr::static_assert!(core::mem::size_of::<Handle>() == 40);
zr::static_assert!(core::mem::align_of::<Handle>() == 8);
zr::static_assert!(core::mem::offset_of!(Handle, handle_table_id) == 0);
zr::static_assert!(core::mem::offset_of!(Handle, dispatcher) == 8);
zr::static_assert!(core::mem::offset_of!(Handle, rights) == 16);
zr::static_assert!(core::mem::offset_of!(Handle, base_value) == 20);
zr::static_assert!(core::mem::offset_of!(Handle, node_state) == 24);

// Validate that all the fields we need to preserve fit within the preservation window.
zr::static_assert!(
    core::mem::offset_of!(Handle, handle_table_id) + core::mem::size_of::<RelaxedAtomicU64>()
        <= HANDLE_PRESERVE_SIZE
);
zr::static_assert!(
    core::mem::offset_of!(Handle, base_value) + core::mem::size_of::<RelaxedAtomicU32>()
        <= HANDLE_PRESERVE_SIZE
);
zr::static_assert!(
    core::mem::offset_of!(Handle, dispatcher) + core::mem::size_of::<RefPtr<Dispatcher>>()
        <= HANDLE_PRESERVE_SIZE
);

impl Handle {
    /// Returns the `Dispatcher` to which this instance points.
    #[inline]
    pub fn dispatcher(&self) -> &RefPtr<Dispatcher> {
        &self.dispatcher
    }

    /// Returns the KOID of the handle table that owns this handle.
    #[inline]
    pub fn handle_table_id(&self) -> zx_koid_t {
        self.handle_table_id.load()
    }

    /// Reads `handle_table_id` from a committed arena slot without materializing `&Handle`.
    ///
    /// # Safety
    /// `ptr` must point to a committed slot in the handle arena (such as one returned by
    /// [`Handle::from_u32`]).
    #[inline]
    pub(crate) unsafe fn raw_handle_table_id(ptr: NonNull<Self>) -> zx_koid_t {
        // SAFETY: `ptr` points to a committed arena slot whose `handle_table_id` field lies
        // within `HANDLE_PRESERVE_SIZE` and is always valid as a `RelaxedAtomicU64`. We must
        // not materialize `&Handle` here because a freed slot has a null `dispatcher`.
        unsafe { (&*core::ptr::addr_of!((*ptr.as_ptr()).handle_table_id)).load() }
    }

    /// Reads `base_value` from a committed arena slot without materializing `&Handle`.
    ///
    /// # Safety
    /// `ptr` must point to a committed slot in the handle arena. The slot may be free: the arena
    /// preserves `base_value` across allocations so the next allocation can derive a fresh value
    /// from it.
    #[inline]
    unsafe fn raw_base_value(ptr: NonNull<Self>) -> u32 {
        // SAFETY: `ptr` points to a committed arena slot whose `base_value` field lies within
        // `HANDLE_PRESERVE_SIZE` and is always valid as a `RelaxedAtomicU32`. We must not
        // materialize `&Handle` here because a freed slot has a null `dispatcher`.
        unsafe { (&*core::ptr::addr_of!((*ptr.as_ptr()).base_value)).load() }
    }

    /// Sets the owning handle table KOID and propagates it to the dispatcher if applicable.
    #[inline]
    pub fn set_handle_table_id(&self, pid: zx_koid_t) {
        self.handle_table_id.store(pid);
        if let Some(channel) = self.dispatcher.downcast::<ChannelDispatcher>() {
            channel.set_owner(pid);
        }
    }

    /// Returns the rights mask associated with this handle.
    #[inline]
    pub fn rights(&self) -> zx_rights_t {
        self.rights
    }

    /// Returns true if this handle possesses all of the desired rights bits.
    #[inline]
    pub fn has_rights(&self, desired: zx_rights_t) -> bool {
        (self.rights & desired) == desired
    }

    /// Returns the internal base value for this handle.
    #[inline]
    pub fn base_value(&self) -> u32 {
        self.base_value.load()
    }

    /// Initializes the global handle table arena.
    ///
    /// To be called once during bring up before any handles are allocated.
    pub(super) fn init() {
        static INITIALIZED: AtomicBool = AtomicBool::new(false);
        assert!(!INITIALIZED.swap(true, Ordering::Relaxed), "Handle::init called more than once");
        HandleTableArena::init().expect("failed to initialize handle arena");
    }

    /// Maps a 32-bit handle base value back to a pointer to a `Handle`.
    pub(crate) fn from_u32(value: u32) -> Option<NonNull<Handle>> {
        HandleTableArena::from_u32(value)
    }

    /// Returns the number of outstanding handles pointing to the given dispatcher.
    pub fn count(dispatcher: &Dispatcher) -> u32 {
        dispatcher.current_handle_count()
    }

    /// Initializes an allocated arena slot field by field. The atomic `handle_table_id` and
    /// `base_value` fields are written with atomic stores because lockless lookups may read them
    /// concurrently; a whole-struct `write` would race with those readers.
    ///
    /// # Safety
    /// `addr` must point to an allocated, writable `Handle` slot in the handle arena.
    unsafe fn init_slot(
        addr: NonNull<u8>,
        dispatcher: RefPtr<Dispatcher>,
        rights: zx_rights_t,
        base_value: u32,
    ) -> NonNull<Handle> {
        let ptr = addr.cast::<Handle>();
        let raw = ptr.as_ptr();
        // SAFETY: `addr` points to a writable `Handle` slot in the arena. `handle_table_id` and
        // `base_value` lie within `HANDLE_PRESERVE_SIZE` and may be read concurrently by lockless
        // lookups, so they are updated via atomic stores; the remaining fields are initialized
        // with `write`.
        unsafe {
            (&*core::ptr::addr_of_mut!((*raw).handle_table_id)).store(ZX_KOID_INVALID);
            core::ptr::addr_of_mut!((*raw).dispatcher).write(dispatcher);
            core::ptr::addr_of_mut!((*raw).rights).write(rights);
            (&*core::ptr::addr_of_mut!((*raw).base_value)).store(base_value);
            core::ptr::addr_of_mut!((*raw).node_state).write(DoublyLinkedListNode::new());
        }
        ptr
    }

    /// Creates a newly allocated `Handle` pointing to `dispatcher` with `rights`.
    pub fn make(dispatcher: RefPtr<Dispatcher>, rights: zx_rights_t) -> Option<HandleOwner> {
        let (addr, base_value) = HandleTableArena::alloc(&dispatcher, "new")?;
        HANDLE_COUNT_MADE.add(1);
        HANDLE_COUNT_LIVE.add(1);
        // SAFETY: `addr` is an allocated slot from `HandleTableArena`, and `init_slot` initializes
        // it into a uniquely owned `Handle`.
        unsafe {
            let ptr = Self::init_slot(addr, dispatcher, rights, base_value);
            Some(HandleOwner::from_non_null(ptr))
        }
    }

    /// Creates a newly allocated `Handle` by taking ownership of a `KernelHandle`.
    pub fn make_from_kernel_handle(
        kernel_handle: KernelHandle<Dispatcher>,
        rights: zx_rights_t,
    ) -> Option<HandleOwner> {
        let (addr, base_value) = HandleTableArena::alloc(kernel_handle.dispatcher(), "new")?;
        let dispatcher = kernel_handle.release();
        HANDLE_COUNT_MADE.add(1);
        HANDLE_COUNT_LIVE.add(1);
        // SAFETY: `addr` is an allocated slot from `HandleTableArena`, and `init_slot` initializes
        // it into a uniquely owned `Handle`.
        unsafe {
            let ptr = Self::init_slot(addr, dispatcher, rights, base_value);
            Some(HandleOwner::from_non_null(ptr))
        }
    }

    /// Duplicates an existing handle with specified rights.
    pub fn dup(source: &Handle, rights: zx_rights_t) -> Option<HandleOwner> {
        let (addr, base_value) = HandleTableArena::alloc(source.dispatcher(), "duplicate")?;
        HANDLE_COUNT_DUPED.add(1);
        HANDLE_COUNT_LIVE.add(1);
        // SAFETY: `addr` is an allocated slot from `HandleTableArena`, and `init_slot` initializes
        // it into a uniquely owned `Handle`.
        unsafe {
            // Although this handle is intended to become owned by source.handle_table_id at the
            // point of creation it is stack owned and may be destroyed without actually being
            // assigned to the handle table. If this happens the assert in Handle::delete would get
            // triggered.
            let ptr = Self::init_slot(addr, source.dispatcher.clone(), rights, base_value);
            Some(HandleOwner::from_non_null(ptr))
        }
    }

    /// Deletes a handle, running its destructor and returning its memory slot to the arena.
    fn delete(handle: NonNull<Handle>) {
        let raw = handle.as_ptr();

        // SAFETY: `handle` points to an allocated `Handle` slot in the arena. `handle_table_id`
        // and `base_value` are read atomically because concurrent lockless lookups may inspect
        // them, while `dispatcher` and `node_state` are moved out of the uniquely owned slot.
        let (handle_table_id, dispatcher, base_value, node_state) = unsafe {
            (
                Self::raw_handle_table_id(handle),
                core::ptr::addr_of!((*raw).dispatcher).read(),
                Self::raw_base_value(handle),
                core::ptr::addr_of!((*raw).node_state).read(),
            )
        };

        // There may be stale pointers to this slot and they will look at handle_table_id. We
        // expect handle_table_id to already have been cleared by the process dispatcher before
        // the handle got to this point.
        debug_assert!(handle_table_id == ZX_KOID_INVALID);

        if dispatcher.is_waitable() {
            dispatcher.cancel_raw(raw.cast());
        }

        // The destructor should not do anything interesting but call it for completeness.
        drop(node_state);
        // Make sure the base value was not altered by the destructor.
        // SAFETY: `handle` points to the arena slot whose `base_value` was preserved.
        debug_assert!(unsafe { Self::raw_base_value(handle) } == base_value);

        // SAFETY: Write null into the slot's preserved `dispatcher` field so the debug assertion in
        // `alloc()` can verify that recycled slots have a null dispatcher pointer.
        unsafe {
            core::ptr::addr_of_mut!((*raw).dispatcher)
                .cast::<*mut ()>()
                .write(core::ptr::null_mut());
        }

        let zero_handles = dispatcher.decrement_handle_count();

        // SAFETY: `handle` was allocated by `cpp_handle_arena_alloc` and its contents have been
        // destructured.
        unsafe { cpp_handle_arena_free(raw.cast()) };

        if zero_handles {
            dispatcher.on_zero_handles();
        }

        // If this was the last reference to the dispatcher, it gets dropped here
        // after the handle slot has been returned to the arena.
        drop(dispatcher);
        HANDLE_COUNT_LIVE.add(-1);
    }
}

// An EventLimiter allows an action to only be taken every K seconds in a thread-safe manner.
struct EventLimiter<const DURATION: i64> {
    last_event: core::sync::atomic::AtomicI64,
}

impl<const DURATION: i64> EventLimiter<DURATION> {
    const fn new() -> Self {
        Self { last_event: core::sync::atomic::AtomicI64::new(0) }
    }

    fn ready(&self) -> bool {
        let now = crate::platform_rs::timer::current_mono_time().0;
        let last_event = self.last_event.load(core::sync::atomic::Ordering::Relaxed);
        if last_event != 0 && now < last_event.saturating_add(DURATION) {
            return false;
        }
        if self
            .last_event
            .compare_exchange(
                last_event,
                now,
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        true
    }
}

static HANDLE_COUNT_HIGH_LOG: EventLimiter<1_000_000_000> = EventLimiter::new();

/// Arena allocator managing memory slots for all `Handle` objects in the kernel.
pub(crate) struct HandleTableArena;

impl HandleTableArena {
    /// Initializes the handle table arena.
    fn init() -> Result<(), Status> {
        // SAFETY: `cpp_handle_arena_init` initializes the C++ backing allocator for the arena.
        let status = unsafe { cpp_handle_arena_init(c"handles".as_ptr(), MAX_HANDLE_COUNT) };
        Status::ok(status)
    }

    /// Allocates space for a `Handle` from the arena, but don't instantiate the
    /// object. `base_value` gets the value for `Handle::base_value`. `what`
    /// says whether this is allocation or duplication, for the error message.
    fn alloc(dispatcher: &Dispatcher, what: &str) -> Option<(NonNull<u8>, u32)> {
        // Attempt to allocate a handle.
        // SAFETY: `cpp_handle_arena_alloc` allocates a raw slot in the global handle arena.
        let raw_addr = unsafe { cpp_handle_arena_alloc() };
        // SAFETY: Retrieves the diagnostic count from the global arena.
        let outstanding_handles = unsafe { cpp_handle_arena_diagnostic_count() };
        let Some(addr) = NonNull::new(raw_addr.cast::<u8>()) else {
            HANDLE_COUNT_ALLOC_FAILED.add(1);
            kprint::kprintln!(
                "WARNING: Could not allocate {what:s} handle ({outstanding_handles} outstanding)"
            );
            return None;
        };

        // Emit a warning if too many handles have been created and we haven't recently logged
        if outstanding_handles > HIGH_HANDLE_COUNT && HANDLE_COUNT_HIGH_LOG.ready() {
            kprint::kprintln!(
                "WARNING: High handle count: {outstanding_handles} / {HIGH_HANDLE_COUNT} handles"
            );
        }

        dispatcher.increment_handle_count();
        let handle_ptr = addr.cast::<Handle>();
        // checking the handle_table_id and dispatcher is really about trying to catch cases
        // where this Handle might somehow already be in use.
        // SAFETY: `handle_ptr` points to the newly allocated uninitialized arena slot.
        unsafe {
            debug_assert!(Handle::raw_handle_table_id(handle_ptr) == ZX_KOID_INVALID);
            let disp_ptr =
                core::ptr::addr_of!((*handle_ptr.as_ptr()).dispatcher).cast::<*const ()>().read();
            debug_assert!(disp_ptr.is_null());
        }
        let base_value = Self::get_new_base_value(handle_ptr);
        Some((addr, base_value))
    }

    /// Returns a new base_value based on the value stored in the free arena slot pointed to by
    /// `handle`. The new value will be different from the last base_value used by this slot.
    fn get_new_base_value(handle: NonNull<Handle>) -> u32 {
        // Get the index of this slot within the arena.
        let handle_index = Self::handle_to_index(handle.as_ptr());
        // Check the free memory for a stashed base_value.
        // SAFETY: `handle` points to an arena slot whose previous base value is preserved across
        // allocations.
        let old_base_value = unsafe { Handle::raw_base_value(handle) };
        new_handle_value(handle_index, old_base_value)
    }

    /// Converts a handle pointer to its index in the arena.
    fn handle_to_index(handle: *const Handle) -> u32 {
        // SAFETY: Returns the base address of the global handle arena.
        let base = unsafe { cpp_handle_arena_base() } as usize;
        ((handle as usize - base) / core::mem::size_of::<Handle>()) as u32
    }

    /// Converts an index in the arena to a memory address.
    fn index_to_handle(index: u32) -> usize {
        // SAFETY: Returns the base address of the global handle arena.
        let base = unsafe { cpp_handle_arena_base() } as usize;
        base + (index as usize) * core::mem::size_of::<Handle>()
    }

    /// Maps a 32-bit handle value back to a pointer to a `Handle`.
    fn from_u32(value: u32) -> Option<NonNull<Handle>> {
        let index = handle_value_to_index(value);
        let handle_addr = Self::index_to_handle(index);
        // SAFETY: Checks if `handle_addr` is committed in the arena.
        if unsafe {
            !cpp_handle_arena_committed(core::ptr::with_exposed_provenance::<core::ffi::c_void>(
                handle_addr,
            ))
        } {
            return None;
        }
        // SAFETY: Confines `handle_addr` to ensure it is within the arena boundary without
        // speculative bypass.
        let handle_addr = unsafe { cpp_handle_arena_confine(handle_addr) };
        let handle = NonNull::new(core::ptr::with_exposed_provenance_mut::<Handle>(handle_addr))?;
        // SAFETY: `handle` points to committed, confined arena memory.
        let actual_base = unsafe { Handle::raw_base_value(handle) };
        let safe_addr =
            fbl::conditional_select_nospec_eq(actual_base as usize, value as usize, handle_addr, 0);
        NonNull::new(core::ptr::with_exposed_provenance_mut::<Handle>(safe_addr))
    }

    /// Returns the number of currently allocated handles in the arena.
    #[cfg(console_enabled)]
    pub(super) fn diagnostic_count() -> usize {
        // SAFETY: Safe to query diagnostic handle count from global arena.
        unsafe { cpp_handle_arena_diagnostic_count() }
    }

    /// Dumps arena diagnostic information.
    #[cfg(console_enabled)]
    pub(super) fn dump() {
        // SAFETY: Safe to dump arena diagnostic info.
        unsafe { cpp_handle_arena_dump() };
    }

    /// Returns the number of handle allocations that have failed since boot.
    pub(crate) fn get_alloc_failed_count() -> i64 {
        HANDLE_COUNT_ALLOC_FAILED.sum_across_all_cpus()
    }
}

/// Safe RAII wrapper for an uniquely owned `Handle`.
#[derive(Debug)]
#[repr(transparent)]
pub struct HandleOwner {
    ptr: NonNull<Handle>,
}

impl HandleOwner {
    /// Creates a new `HandleOwner` from a non-null `Handle` pointer.
    ///
    /// # Safety
    /// Caller must ensure `ptr` is a valid, uniquely owned pointer to an allocated `Handle`.
    #[inline]
    pub unsafe fn from_non_null(ptr: NonNull<Handle>) -> Self {
        Self { ptr }
    }

    /// Creates a new `HandleOwner` from a raw `Handle` pointer if non-null.
    ///
    /// # Safety
    /// Caller must ensure `ptr` is null or a valid, uniquely owned pointer to an allocated
    /// `Handle`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut Handle) -> Option<Self> {
        // SAFETY: The caller guarantees `ptr`, if non-null, is a valid uniquely owned `Handle`.
        NonNull::new(ptr).map(|ptr| unsafe { Self::from_non_null(ptr) })
    }

    /// Releases ownership of the pointer without running its destructor.
    #[inline]
    pub fn release(self) -> NonNull<Handle> {
        let ptr = self.ptr;
        core::mem::forget(self);
        ptr
    }

    /// Releases ownership of an optional `HandleOwner`, returning null if `None`.
    #[inline]
    pub fn release_opt(owner: Option<Self>) -> *mut Handle {
        owner.map_or(core::ptr::null_mut(), |o| o.release().as_ptr())
    }

    /// Duplicates this handle with the given rights.
    #[inline]
    pub fn dup(&self, rights: zx_rights_t) -> Result<Self, Status> {
        Handle::dup(self, rights).ok_or(Status::NO_MEMORY)
    }

    /// Returns the raw pointer backing this handle owner.
    #[inline]
    pub fn as_raw(&self) -> *mut Handle {
        self.ptr.as_ptr()
    }

    /// Returns the non-null pointer backing this handle owner.
    pub fn as_non_null(&self) -> NonNull<Handle> {
        self.ptr
    }

    /// Returns a borrowed `HandleRef` referencing this handle.
    #[inline]
    pub fn as_ref(&self) -> HandleRef<'_> {
        HandleRef::new(self)
    }

    /// Returns a borrowed reference to the handle's dispatcher without incrementing its reference
    /// count.
    #[inline]
    pub fn dispatcher_ref(&self) -> &Dispatcher {
        self.dispatcher()
    }

    /// Allocates a new `HandleOwner` wrapping `handle` with `rights`.
    pub fn make<T>(handle: KernelHandle<T>, rights: zx_rights_t) -> Option<Self>
    where
        T: HasRefCount + Recyclable + DispatcherOps,
    {
        Handle::make_from_kernel_handle(handle.cast(), rights)
    }

    /// Allocates a new `HandleOwner` wrapping `dispatcher` with `rights`.
    pub fn make_from_ref<T>(dispatcher: RefPtr<T>, rights: zx_rights_t) -> Option<Self>
    where
        T: HasRefCount + Recyclable + DispatcherOps,
    {
        // SAFETY: `T` implements `DispatcherOps` and is layout-compatible with `Dispatcher`.
        let generic = unsafe { dispatcher.cast::<Dispatcher>() };
        Handle::make(generic, rights)
    }
}

impl core::ops::Deref for HandleOwner {
    type Target = Handle;
    fn deref(&self) -> &Handle {
        // SAFETY: `self.ptr` is non-null and points to a valid `Handle` owned by `HandleOwner`.
        unsafe { self.ptr.as_ref() }
    }
}

impl core::borrow::Borrow<Handle> for HandleOwner {
    #[inline]
    fn borrow(&self) -> &Handle {
        self
    }
}

impl core::borrow::Borrow<Handle> for &HandleOwner {
    #[inline]
    fn borrow(&self) -> &Handle {
        self
    }
}

impl Drop for HandleOwner {
    #[inline]
    fn drop(&mut self) {
        Handle::delete(self.ptr);
    }
}

// SAFETY: `HandleOwner` uniquely owns a `Handle`, which is thread-safe.
unsafe impl Send for HandleOwner {}

/// A minimal wrapper around a `Dispatcher` which is owned by the kernel.
///
/// Intended usage when creating a new Dispatcher object is:
///   1. Create a KernelHandle on the stack (cannot fail)
///   2. Move the RefPtr<Dispatcher> into the KernelHandle (cannot fail)
///   3. When ready to give the handle to a process, upgrade the KernelHandle
///      to a full HandleOwner via Handle::make() / Handle::make_from_kernel_handle() (can fail)
///
/// This sequence ensures that the Dispatcher's on_zero_handles() method is
/// called even if errors occur during or before HandleOwner creation, which
/// is necessary to break circular references for some Dispatcher types.
///
/// This class is thread-unsafe and must be externally synchronized if used
/// across multiple threads.
#[repr(transparent)]
pub struct KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    dispatcher: Option<RefPtr<T>>,
}

zr::static_assert!(core::mem::size_of::<KernelHandle<Dispatcher>>() == 8);
zr::static_assert!(core::mem::align_of::<KernelHandle<Dispatcher>>() == 8);

impl<T> Default for KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> core::fmt::Debug for KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KernelHandle").field("is_valid", &self.is_valid()).finish()
    }
}

impl<T> KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    /// Creates a `KernelHandle<T>` using an initializer closure (typically an FFI call) that
    /// writes to an uninitialized handle pointer and returns a `zx_status_t`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `init` initializes the memory at `*out` with a valid
    /// `KernelHandle<T>` whenever it returns `ZX_OK` (0).
    pub unsafe fn create(
        init: impl FnOnce(&mut MaybeUninit<Self>) -> zx_status_t,
    ) -> Result<Self, Status> {
        let mut handle = MaybeUninit::<Self>::uninit();
        let status = init(&mut handle);
        Status::ok(status)?;
        // SAFETY: `init` succeeded and initialized `handle`.
        unsafe { Ok(handle.assume_init()) }
    }

    /// Creates a `KernelHandle<T>` and rights using an initializer closure that writes
    /// to uninitialized handle and rights pointers.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `init` initializes the memory at `*handle_out` and
    /// `*rights_out` whenever it returns `ZX_OK` (0).
    pub unsafe fn create_with_rights(
        init: impl FnOnce(&mut MaybeUninit<Self>, &mut MaybeUninit<zx_rights_t>) -> zx_status_t,
    ) -> Result<(Self, zx_rights_t), Status> {
        let mut handle = MaybeUninit::<Self>::uninit();
        let mut rights = MaybeUninit::<zx_rights_t>::uninit();
        let status = init(&mut handle, &mut rights);
        Status::ok(status)?;
        // SAFETY: `init` succeeded and initialized `handle` and `rights`.
        unsafe { Ok((handle.assume_init(), rights.assume_init())) }
    }

    /// Creates a new `KernelHandle` owning `dispatcher`.
    pub fn new(dispatcher: RefPtr<T>) -> Self {
        debug_assert!(dispatcher.current_handle_count() == 0);
        Self { dispatcher: Some(dispatcher) }
    }

    /// Constructs an empty `KernelHandle`.
    pub const fn empty() -> Self {
        Self { dispatcher: None }
    }

    /// Casts this handle to a generic Dispatcher handle.
    pub fn cast(mut self) -> KernelHandle<Dispatcher> {
        let disp = self.dispatcher.take().expect("KernelHandle was empty");
        // SAFETY: `T` implements `DispatcherOps` and is layout-compatible with `Dispatcher`.
        let cast_disp = unsafe { disp.cast::<Dispatcher>() };
        KernelHandle { dispatcher: Some(cast_disp) }
    }

    /// Releases the underlying `RefPtr<T>` without invoking `on_zero_handles`.
    pub fn release(mut self) -> RefPtr<T> {
        self.dispatcher.take().expect("KernelHandle was empty")
    }

    /// Returns a reference to the underlying `RefPtr<T>`.
    pub fn dispatcher(&self) -> &RefPtr<T> {
        self.dispatcher.as_ref().expect("KernelHandle was empty")
    }

    /// Returns true if this `KernelHandle` holds a dispatcher.
    pub fn is_valid(&self) -> bool {
        self.dispatcher.is_some()
    }

    /// Resets the `KernelHandle` to empty, triggering `on_zero_handles` on the previous dispatcher.
    pub fn reset(&mut self) {
        self.reset_to(None);
    }

    /// Resets the `KernelHandle` with a new dispatcher.
    pub fn reset_to(&mut self, new_disp: Option<RefPtr<T>>) {
        let prev = core::mem::replace(&mut self.dispatcher, new_disp);
        if let Some(disp) = prev {
            disp.on_zero_handles();
        }
    }

    /// Adds this kernel handle to the current process's handle table.
    pub fn make_and_add_handle(self, rights: zx_rights_t) -> Result<HandleValue, Status> {
        ProcessDispatcher::with_current(|up| up.make_and_add_handle(self, rights))
    }
}

impl<T> Drop for KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    fn drop(&mut self) {
        if let Some(disp) = self.dispatcher.take() {
            disp.on_zero_handles();
        }
    }
}

/// A borrowed reference to a [`Handle`].
///
/// The referenced `Handle` is guaranteed to remain valid for lifetime `'a`, either because the
/// owning process's handle table lock is held for `'a` or because it borrows an owning
/// [`HandleOwner`] (such as the `'static` root job handle).
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct HandleRef<'a> {
    handle: &'a Handle,
}

impl<'a> HandleRef<'a> {
    /// Creates a new `HandleRef` wrapping `handle`.
    #[inline]
    pub(crate) const fn new(handle: &'a Handle) -> Self {
        Self { handle }
    }

    /// Creates a new `HandleRef` from a non-null raw handle pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` points to a valid `Handle` that remains alive and is not
    /// mutably aliased for the lifetime `'a` (for example, by holding the owning handle table lock
    /// or borrowing a live `HandleOwner`).
    #[inline]
    pub unsafe fn from_raw(ptr: NonNull<Handle>) -> Self {
        // SAFETY: The caller guarantees `ptr` points to a valid `Handle` for `'a`.
        Self { handle: unsafe { ptr.as_ref() } }
    }

    /// Returns whether this handle possesses the requested rights.
    #[inline]
    pub fn has_rights(&self, rights: zx_rights_t) -> bool {
        self.handle.has_rights(rights)
    }

    /// Returns the rights mask of the handle.
    #[inline]
    pub fn rights(&self) -> zx_rights_t {
        self.handle.rights()
    }

    /// Duplicates this handle with the given rights.
    #[inline]
    pub fn dup(&self, rights: zx_rights_t) -> Result<HandleOwner, Status> {
        Handle::dup(self.handle, rights).ok_or(Status::NO_MEMORY)
    }

    /// Returns the base value of the handle.
    #[inline]
    pub fn base_value(&self) -> u32 {
        self.handle.base_value()
    }

    /// Returns the handle table KOID owning this handle.
    #[inline]
    pub fn handle_table_id(&self) -> zx_koid_t {
        self.handle.handle_table_id()
    }

    /// Returns a cloned `RefPtr` to the handle's dispatcher.
    #[inline]
    pub fn dispatcher(&self) -> RefPtr<Dispatcher> {
        self.handle.dispatcher().clone()
    }

    /// Returns a shared reference to the handle's dispatcher.
    #[inline]
    pub fn dispatcher_ref(&self) -> &Dispatcher {
        self.handle.dispatcher()
    }

    /// Returns a shared reference to the underlying `Handle`.
    #[inline]
    pub fn as_handle(&self) -> &'a Handle {
        self.handle
    }

    /// Returns a raw pointer to the handle.
    #[inline]
    pub fn as_ptr(&self) -> *const Handle {
        core::ptr::from_ref(self.handle)
    }
}

impl<'a> core::ops::Deref for HandleRef<'a> {
    type Target = Handle;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_handle()
    }
}

impl<'a> core::borrow::Borrow<Handle> for HandleRef<'a> {
    #[inline]
    fn borrow(&self) -> &Handle {
        self.as_handle()
    }
}

impl<'a> core::borrow::Borrow<Handle> for &HandleRef<'a> {
    #[inline]
    fn borrow(&self) -> &Handle {
        self.as_handle()
    }
}

unsafe extern "C" {
    fn cpp_handle_arena_init(
        name: *const core::ffi::c_char,
        max_count: usize,
    ) -> zx_types::zx_status_t;
    fn cpp_handle_arena_alloc() -> *mut core::ffi::c_void;
    fn cpp_handle_arena_free(ptr: *mut core::ffi::c_void);
    fn cpp_handle_arena_committed(ptr: *const core::ffi::c_void) -> bool;
    fn cpp_handle_arena_confine(addr: usize) -> usize;
    fn cpp_handle_arena_base() -> *mut core::ffi::c_void;
    fn cpp_handle_arena_diagnostic_count() -> usize;
    #[cfg(console_enabled)]
    fn cpp_handle_arena_dump();
}

// C-compatible FFI exported symbols.

/// Allocates and creates a new `Handle` pointing to `dispatcher` with the specified `rights`.
///
/// # Safety
/// `dispatcher` must be a valid, non-null pointer to an initialized `Dispatcher` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_make(
    dispatcher: *const Dispatcher,
    rights: zx_rights_t,
) -> *mut Handle {
    if dispatcher.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: dispatcher is a valid non-null raw pointer.
    let disp_ref = unsafe { &*dispatcher };
    let Some((addr, base_value)) = HandleTableArena::alloc(disp_ref, "new") else {
        return core::ptr::null_mut();
    };
    HANDLE_COUNT_MADE.add(1);
    HANDLE_COUNT_LIVE.add(1);
    // SAFETY: Ownership of the dispatcher reference count is transferred by C++ caller upon
    // non-null return.
    let ref_ptr = unsafe { RefPtr::from_raw(dispatcher) };
    // SAFETY: `addr` is an allocated slot from `HandleTableArena`.
    unsafe { Handle::init_slot(addr, ref_ptr, rights, base_value).as_ptr() }
}

/// Destroys a handle and returns its memory to the handle table arena.
///
/// # Safety
/// `handle` must be a valid pointer to an allocated `Handle` or null. Must not be called
/// more than once for the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_destroy(handle: *mut Handle) {
    // SAFETY: `handle` is either null or a valid pointer to an allocated `Handle`.
    if let Some(owner) = unsafe { HandleOwner::from_raw(handle) } {
        drop(owner);
    }
}

/// Tests for Handle and KernelHandle.
#[cfg(ktest)]
#[unittest::suite(name = "handle_rust")]
mod tests {
    use super::{Handle, KernelHandle};
    use crate::object::{Dispatcher, EventPairDispatcher};
    use unittest::{expect_eq, expect_err, expect_ok, expect_true};
    use zx_status::Status;

    /// Tests creating and dropping a KernelHandle.
    #[test]
    fn test_kernel_handle_create() {
        let (ep0, ep1, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let disp = ep0.release();
            let handle = KernelHandle::new(disp);
            expect_true!(handle.is_valid());
            expect_eq!(handle.dispatcher().get_koid(), ep1.dispatcher().get_related_koid());
            expect_ok!(ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        }
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests upcasting a KernelHandle<EventPairDispatcher> to KernelHandle<Dispatcher>.
    #[test]
    fn test_kernel_handle_cast() {
        let (ep0, ep1, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let handle: KernelHandle<Dispatcher> = ep0.cast();
            expect_true!(handle.is_valid());
            expect_ok!(ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
            drop(handle);
        }
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests resetting a KernelHandle to another dispatcher.
    #[test]
    fn test_kernel_handle_reset() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let (ep1, ep1_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let mut handle = ep0;
            handle.reset_to(Some(ep1.release()));
            expect_true!(handle.is_valid());
            // ep0 peer should now be closed.
            expect_err!(
                ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
                Status::PEER_CLOSED
            );
            // ep1 peer should still be open.
            expect_ok!(ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        }
        // ep1 peer should now be closed when handle drops.
        expect_err!(
            ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests resetting a base KernelHandle<Dispatcher> to another derived dispatcher via upcast.
    #[test]
    fn test_kernel_handle_reset_upcast() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let (ep1, ep1_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let mut handle: KernelHandle<Dispatcher> = ep0.cast();
            handle.reset_to(Some(ep1.cast().release()));
            expect_true!(handle.is_valid());
            // ep0 peer should now be closed.
            expect_err!(
                ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
                Status::PEER_CLOSED
            );
            // ep1 peer should still be open.
            expect_ok!(ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        }
        // ep1 peer should now be closed when handle drops.
        expect_err!(
            ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests resetting a KernelHandle to null.
    #[test]
    fn test_kernel_handle_reset_to_null() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let mut handle = ep0;
        handle.reset();
        expect_true!(!handle.is_valid());
        expect_err!(
            ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests releasing the dispatcher from a KernelHandle.
    #[test]
    fn test_kernel_handle_release() {
        let (ep0, ep1, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let koid = ep0.dispatcher().get_koid();
        let disp = ep0.release();
        // ep0 is consumed and dropped without its dispatcher; peer should still be open.
        expect_ok!(ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        expect_eq!(disp.get_koid(), koid);
        expect_eq!(disp.get_koid(), ep1.dispatcher().get_related_koid());
        // Putting it in a new KernelHandle and dropping it triggers on_zero_handles.
        let handle = KernelHandle::new(disp);
        drop(handle);
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests moving a KernelHandle between variables.
    #[test]
    fn test_kernel_handle_move() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let handle = ep0;
        {
            let new_handle = handle;
            expect_true!(new_handle.is_valid());
            expect_ok!(ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
            drop(new_handle);
        }
        expect_err!(
            ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests move assignment over an existing, active KernelHandle.
    #[test]
    fn test_kernel_handle_move_assignment() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let (ep1, ep1_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let mut handle = ep0;
            expect_true!(handle.is_valid());
            expect_ok!(ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
            // Move assigning ep1 over handle drops ep0, immediately closing ep0_peer.
            handle = ep1;
            expect_true!(handle.is_valid());
            expect_err!(
                ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
                Status::PEER_CLOSED
            );
            // ep1_peer should still be open while handle is alive.
            expect_ok!(ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        }
        // ep1_peer should now be closed when handle drops.
        expect_err!(
            ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests move assignment of an upcasted derived KernelHandle.
    #[test]
    fn test_kernel_handle_move_assignment_upcast() {
        let (ep0, ep0_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let (ep1, ep1_peer, _rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let mut handle: KernelHandle<Dispatcher> = ep0.cast();
            expect_true!(handle.is_valid());
            expect_ok!(ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
            // Move assigning ep1.cast() over handle drops ep0, immediately closing ep0_peer.
            handle = ep1.cast();
            expect_true!(handle.is_valid());
            expect_err!(
                ep0_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
                Status::PEER_CLOSED
            );
            // ep1_peer should still be open while handle is alive.
            expect_ok!(ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
        }
        // ep1_peer should now be closed when handle drops.
        expect_err!(
            ep1_peer.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests upgrading a KernelHandle to a HandleOwner.
    #[test]
    fn test_kernel_handle_upgrade() {
        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        {
            let handle_owner = Handle::make_from_kernel_handle(ep0.cast(), rights);
            expect_true!(handle_owner.is_some());
            let handle_owner = handle_owner.unwrap();
            expect_eq!(handle_owner.rights(), rights);
            expect_ok!(ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0));
            drop(handle_owner);
        }
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests duplicating a HandleOwner with same and reduced rights.
    #[test]
    fn test_handle_dup() {
        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();

        let ho_dup = ho.dup(rights).expect("dup");
        expect_eq!(ho_dup.rights(), rights);

        let reduced = rights & !zx_types::ZX_RIGHT_SIGNAL;
        let ho_dup2 = ho.dup(reduced).expect("dup reduced");
        expect_eq!(ho_dup2.rights(), reduced);

        drop(ho_dup);
        drop(ho_dup2);
        drop(ho);
        expect_err!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0),
            Status::PEER_CLOSED
        );
    }

    /// Tests looking up a Handle from its 32-bit base value.
    #[test]
    fn test_handle_from_u32() {
        let (ep0, _ep1, rights) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();
        let base_val = ho.base_value();

        let ptr = Handle::from_u32(base_val);
        expect_true!(ptr.is_some());
        expect_eq!(ptr.unwrap().as_ptr() as *const Handle, ho.as_raw());

        expect_true!(Handle::from_u32(base_val ^ 0x1234_5678).is_none());
        drop(ho);
    }
}
