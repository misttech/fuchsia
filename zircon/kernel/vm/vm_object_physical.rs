// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr::NonNull;
use fbl::{IsOpaqueRefCounted, RefPtr};
use ksync::{KMutex, LockToken, RawCriticalMutex, guarded};
use pin_init::pin_data;
use zr::{Opaque, ToMutPtr};
use zx_status::Status;
use zx_types::{ZX_CACHE_POLICY_MASK, ZX_ERR_NEXT, ZX_ERR_STOP, zx_status_t};

use super::arch_vm_aspace::ArchMmuFlags;
use super::vm_object::VmObject;
use vm_constants_rs as constants;
use vm_object_bindings as vmo_bindings;
use vm_object_physical_bindings as phys_bindings;

use ::page as kernel_page;

// Assert size and alignment of VmObjectPhysicalState matches the generated C++ constants.
::zr::static_assert_size_and_align!(
    VmObjectPhysicalState,
    constants::kVmObjectPhysicalStateSize,
    constants::kVmObjectPhysicalStateAlign,
);

#[guarded]
#[repr(C)]
pub struct VmObjectPhysicalState {
    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    size: u64,
    base: PAddr,
    is_slice: bool,
    parent_user_id: u64,

    // parent is guarded by ChildListLock on the C++ side.
    parent: Option<RefPtr<VmObjectPhysical>>,
}

impl VmObjectPhysicalState {
    pub fn init(
        base: PAddr,
        size: u64,
        is_slice: bool,
        parent_user_id: u64,
    ) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            lock <- KMutex::init(),
            size,
            base,
            is_slice,
            parent_user_id,
            parent: None,
        })
    }
}

const PAGE_SIZE: u64 = kernel_page::SIZE as u64;

const fn in_range(offset: u64, len: u64, size: u64) -> bool {
    if let Some(end) = offset.checked_add(len) { end <= size } else { false }
}

#[repr(C)]
/// FFI-safe result structure returned by `rust_vm_object_physical_state_lookup_contiguous_locked`.
pub struct LookupContiguousResult {
    /// The status of the lookup operation.
    pub status: Result<(), Status>,
    /// The physical address retrieved, valid only if `status` is `Ok(())`.
    pub paddr: PAddr,
}

#[repr(C)]
/// FFI-safe result structure returned by `rust_vm_object_physical_validate_child_slice_args`.
pub struct ValidateChildSliceResult {
    /// The status of the validation operation.
    pub status: Result<(), Status>,
    /// The physical address retrieved, valid only if `status` is `Ok(())`.
    pub base: PAddr,
}

enum ChildListLockClass {}

/// RAII guard that manages the acquisition and release of the child list lock.
struct ChildListLockGuard {
    should_clear: bool,
    token: LockToken<'static, ChildListLockClass>,
}

impl ChildListLockGuard {
    #[inline(always)]
    fn new() -> Self {
        // TODO(https://fxbug.dev/539633525): Figure out lockdep validation for
        // ChildListLock instead of bypassing it via C++ FFI wrappers.
        // SAFETY: `cpp_vm_object_physical_child_list_lock_acquire` has no preconditions.
        let should_clear: bool =
            unsafe { phys_bindings::cpp_vm_object_physical_child_list_lock_acquire() };
        // SAFETY: The child list lock is held by the current thread until the guard drops, and
        // the token is only lent out for borrows of the guard.
        let token: LockToken<'static, ChildListLockClass> = unsafe { LockToken::new() };
        Self { should_clear, token }
    }

    #[inline(always)]
    fn token(&self) -> &LockToken<'_, ChildListLockClass> {
        &self.token
    }
}

impl Drop for ChildListLockGuard {
    #[inline(always)]
    fn drop(&mut self) {
        // SAFETY: The child list lock is held by the current thread, and `self.should_clear` is
        // the value its acquisition returned.
        unsafe {
            phys_bindings::cpp_vm_object_physical_child_list_lock_release(self.should_clear);
        }
    }
}

/// VMO representing a physical range of memory.
#[pin_data]
#[repr(C)]
pub struct VmObjectPhysical {
    #[pin]
    raw: Opaque<phys_bindings::VmObjectPhysical>,
}

// SAFETY: `VmObjectPhysical` inherits from `VmObject` at byte offset 0 and derefs to `VmObject`.
unsafe impl IsOpaqueRefCounted for VmObjectPhysical {
    type TargetBase = VmObject;
}

impl Deref for VmObjectPhysical {
    type Target = VmObject;
    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.as_raw()` points to a live `VmObjectPhysical`.
        let raw: *mut vmo_bindings::VmObject =
            unsafe { phys_bindings::cpp_vm_object_physical_as_vm_object(self.as_raw()) };
        let ptr: *mut VmObject = VmObject::ptr_from_raw(raw);
        // SAFETY: `ptr` points to a live `VmObject` for the lifetime of `self`.
        unsafe { ptr.as_ref_unchecked() }
    }
}

impl VmObjectPhysical {
    fn cast_raw(ptr: *mut VmObjectPhysical) -> *mut phys_bindings::VmObjectPhysical {
        ptr.cast()
    }

    fn as_raw(&self) -> *mut phys_bindings::VmObjectPhysical {
        self.raw.get()
    }

    /// # Safety
    ///
    /// `raw` is null or points to a live `VmObjectPhysical` and carries one reference, which
    /// passes to the returned `RefPtr`.
    unsafe fn from_raw(raw: *mut phys_bindings::VmObjectPhysical) -> Option<RefPtr<Self>> {
        // SAFETY: `raw` is null or points to a live `VmObjectPhysical` and carries one reference.
        unsafe { RefPtr::try_from_raw(raw.cast::<Self>()) }
    }

    /// Create a new physical VMO for the given physical region.
    pub fn create(base: PAddr, size: usize) -> Result<RefPtr<VmObjectPhysical>, Status> {
        let mut status: zx_status_t = 0;
        // SAFETY: The pointer derived from `&mut status` is valid.
        let raw: *mut phys_bindings::VmObjectPhysical =
            unsafe { phys_bindings::cpp_vm_object_physical_create(base.0, size, &mut status) };
        Status::ok(status)?;
        // SAFETY: `raw` is null or points to a live `VmObjectPhysical` and carries one reference.
        unsafe { Self::from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Converts a `RefPtr<VmObjectPhysical>` into a base `RefPtr<VmObject>`.
    pub fn into_vm_object(this: RefPtr<Self>) -> RefPtr<VmObject> {
        let this = ManuallyDrop::new(this);
        // SAFETY: `this.as_raw()` points to a live `VmObjectPhysical` with an active refcount.
        let raw_base = unsafe { phys_bindings::cpp_vm_object_physical_as_vm_object(this.as_raw()) };
        // SAFETY: `raw_base` points to a live `VmObject` whose reference count was owned by `this`.
        unsafe { VmObject::from_raw(raw_base) }.expect("RefPtr guarantees this is non-null")
    }

    /// Cast a pointer to a VmObjectPhysical to its base VmObject.
    ///
    /// # Safety
    ///
    /// `vmo` points to a live `VmObjectPhysical`.
    #[inline]
    pub unsafe fn cast(vmo: NonNull<VmObjectPhysical>) -> NonNull<VmObject> {
        let raw_this: *mut phys_bindings::VmObjectPhysical = Self::cast_raw(vmo.as_ptr());
        // SAFETY: `raw_this` points to a live `VmObjectPhysical`.
        let raw: *mut vmo_bindings::VmObject =
            unsafe { phys_bindings::cpp_vm_object_physical_as_vm_object(raw_this) };
        let ptr: *mut VmObject = VmObject::ptr_from_raw(raw);
        // SAFETY: `vmo` is non-null, and C++ `static_cast<VmObject*>` on a non-null
        // `VmObjectPhysical` pointer produces a non-null base pointer.
        unsafe { NonNull::new_unchecked(ptr) }
    }

    #[inline]
    fn get_mapping_cache_policy_locked(
        &self,
        _token: &LockToken<'_, VmObjectPhysicalStateLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: `self.as_raw()` points to a live `VmObjectPhysical` and the object lock is held.
        unsafe {
            phys_bindings::cpp_vm_object_physical_get_mapping_cache_policy_locked(self.as_raw())
        }
    }

    #[inline]
    fn num_mappings_locked(&self, _token: &LockToken<'_, VmObjectPhysicalStateLockClass>) -> u32 {
        // SAFETY: `self.as_raw()` points to a live `VmObjectPhysical` and the object lock is held.
        unsafe { phys_bindings::cpp_vm_object_physical_num_mappings_locked(self.as_raw()) }
    }

    #[inline]
    fn has_children_locked(&self, _token: &LockToken<'_, ChildListLockClass>) -> bool {
        // SAFETY: `self.as_raw()` points to a live `VmObjectPhysical` and the child list lock is
        // held.
        unsafe { phys_bindings::cpp_vm_object_physical_has_children_locked(self.as_raw()) }
    }

    #[inline]
    fn set_cache_policy_locked(
        &self,
        _token: &mut LockToken<'_, VmObjectPhysicalStateLockClass>,
        cache_policy: ArchMmuFlags,
    ) {
        // SAFETY: `self.as_raw()` points to a live `VmObjectPhysical` and the object lock is held.
        unsafe {
            phys_bindings::cpp_vm_object_physical_set_cache_policy_locked(
                self.as_raw(),
                cache_policy,
            )
        }
    }
}

// FFI trampolines for C++ calling into Rust VmObjectPhysicalState

/// # Safety
///
/// The caller must ensure `ptr` points to uninitialized memory of at least
/// `size_of::<VmObjectPhysicalState>()` bytes with proper alignment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_init(
    ptr: *mut VmObjectPhysicalState,
    base: PAddr,
    size: u64,
    is_slice: bool,
    parent_user_id: u64,
) {
    // SAFETY: The caller guarantees `ptr` points to uninitialized memory of at least
    // `size_of::<VmObjectPhysicalState>()` bytes with proper alignment.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(
            VmObjectPhysicalState::init(base, size, is_slice, parent_user_id),
            ptr,
        );
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_destroy(ptr: *mut VmObjectPhysicalState) {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    unsafe {
        core::ptr::drop_in_place(ptr);
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_get_lock(
    ptr: *const VmObjectPhysicalState,
) -> *mut KMutex<VmObjectPhysicalStateLockClass, RawCriticalMutex> {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    unsafe {
        let lock_ref = &(*ptr).lock;
        lock_ref.to_mut_ptr()
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_get_size(
    ptr: *const VmObjectPhysicalState,
) -> u64 {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    unsafe { (*ptr).size }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_get_is_slice(
    ptr: *const VmObjectPhysicalState,
) -> bool {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    unsafe { (*ptr).is_slice }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_get_parent_user_id(
    ptr: *const VmObjectPhysicalState,
) -> u64 {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    unsafe { (*ptr).parent_user_id }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
/// The caller must ensure that the `ChildListLock` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_get_parent_locked(
    ptr: *const VmObjectPhysicalState,
) -> *mut VmObjectPhysical {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`
    // and that the ChildListLock is held.
    unsafe {
        match &(*ptr).parent {
            Some(ref_ptr) => RefPtr::into_raw(ref_ptr.clone()).cast_mut(),
            None => core::ptr::null_mut(),
        }
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
/// The caller must ensure that the `ChildListLock` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_set_parent_locked(
    ptr: *mut VmObjectPhysicalState,
    parent: *mut VmObjectPhysical,
) {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`
    // and that the ChildListLock is held.
    unsafe {
        let parent_ref = if parent.is_null() { None } else { Some(RefPtr::from_raw(parent)) };
        (*ptr).parent = parent_ref;
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_lookup_contiguous_locked(
    ptr: *const VmObjectPhysicalState,
    offset: u64,
    len: u64,
) -> LookupContiguousResult {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    let state = unsafe { &*ptr };
    if len == 0 || !kernel_page::is_aligned(offset as usize) {
        return LookupContiguousResult { status: Err(Status::INVALID_ARGS), paddr: PAddr(0) };
    }
    if !in_range(offset, len, state.size) {
        return LookupContiguousResult { status: Err(Status::OUT_OF_RANGE), paddr: PAddr(0) };
    }
    LookupContiguousResult { status: Ok(()), paddr: PAddr(state.base.0 + (offset as usize)) }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_commit_range_pinned(
    ptr: *const VmObjectPhysicalState,
    offset: u64,
    len: u64,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    let state = unsafe { &*ptr };
    if len == 0 || !kernel_page::is_aligned(offset as usize) {
        return Err(Status::INVALID_ARGS);
    }
    ksync::lock!(let _guard = state.lock_lock());
    if !in_range(offset, len, state.size) {
        return Err(Status::OUT_OF_RANGE);
    }
    // Physical VMOs are always committed and so are always pinned.
    Ok(())
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_prefetch_range(
    ptr: *const VmObjectPhysicalState,
    offset: u64,
    len: u64,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    let state = unsafe { &*ptr };
    if !in_range(offset, len, state.size) {
        return Err(Status::OUT_OF_RANGE);
    }
    Ok(())
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `VmObjectPhysicalState`, and `lookup_fn`
/// points to a valid `VmObject::LookupFunction`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_state_lookup(
    ptr: *const VmObjectPhysicalState,
    offset: u64,
    len: u64,
    lookup_fn: *const vmo_bindings::VmObject_LookupFunction,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `ptr` points to an initialized `VmObjectPhysicalState`.
    let state = unsafe { &*ptr };
    if len == 0 {
        return Err(Status::INVALID_ARGS);
    }
    ksync::lock!(let _guard = state.lock_lock());
    if !in_range(offset, len, state.size) {
        return Err(Status::OUT_OF_RANGE);
    }
    let mut cur_offset = kernel_page::round_down(offset as usize) as u64;
    let end = offset + len;
    let end_page_offset = kernel_page::round_up(end as usize) as u64;
    let base_addr = state.base;
    while cur_offset < end_page_offset {
        let pa = PAddr(base_addr.0 + (cur_offset as usize));
        // SAFETY: `lookup_fn` points to a valid `VmObject::LookupFunction`.
        let status: zx_status_t =
            unsafe { vmo_bindings::cpp_vm_object_lookup_fn_invoke(lookup_fn, cur_offset, pa.0) };
        if status != ZX_ERR_NEXT {
            if status == ZX_ERR_STOP {
                return Ok(());
            }
            return Status::ok(status);
        }
        cur_offset += PAGE_SIZE;
    }
    Ok(())
}

/// # Safety
///
/// The caller must ensure `state_ptr` points to an initialized `VmObjectPhysicalState`.
/// The caller must ensure `vmo` points to a valid `VmObjectPhysical`.
/// The caller must ensure `state_ptr` is `vmo`'s state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_set_mapping_cache_policy(
    vmo: *mut VmObjectPhysical,
    state_ptr: *const VmObjectPhysicalState,
    cache_policy: ArchMmuFlags,
) -> Result<(), Status> {
    // SAFETY: The caller guarantees `state_ptr` points to an initialized `VmObjectPhysicalState`.
    let state = unsafe { &*state_ptr };
    if (cache_policy & !(ZX_CACHE_POLICY_MASK as ArchMmuFlags)) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    ksync::lock!(let mut guard = state.lock_lock());

    // SAFETY: The caller guarantees `vmo` points to a valid VmObjectPhysical.
    let vmo = unsafe { &*vmo };

    // If the cache policy is already configured on this VMO and matches
    // the requested policy then this is a no-op. This is a common practice
    // in the serialio and magma drivers, but may change.
    // TODO: revisit this when we shake out more of the future DDK protocol.
    if cache_policy == vmo.get_mapping_cache_policy_locked(guard.token()) {
        return Ok(());
    }

    let child_list_guard = ChildListLockGuard::new();

    // If this VMO is mapped already it is not safe to allow its caching policy to change.
    if vmo.num_mappings_locked(guard.token()) != 0
        || vmo.has_children_locked(child_list_guard.token())
        || state.parent.is_some()
    {
        return Err(Status::BAD_STATE);
    }

    vmo.set_cache_policy_locked(guard.as_mut().token_mut(), cache_policy);
    Ok(())
}

/// # Safety
///
/// The caller must ensure `state_ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_validate_child_slice_args(
    state_ptr: *const VmObjectPhysicalState,
    offset: u64,
    size: u64,
) -> ValidateChildSliceResult {
    // SAFETY: The caller guarantees `state_ptr` is valid.
    let state = unsafe { &*state_ptr };
    ksync::lock!(let _guard = state.lock_lock());

    // Slice must be wholly contained.
    // state.size is not an atomic variable and although it should not be changing, as we are not
    // allowing this operation on resizable VMOs, we should still be holding the lock to
    // correctly read state.size. We drop the lock when returning from this function before
    // performing the child VMO allocation on the C++ side.
    if !in_range(offset, size, state.size) {
        return ValidateChildSliceResult { status: Err(Status::INVALID_ARGS), base: PAddr(0) };
    }

    ValidateChildSliceResult { status: Ok(()), base: PAddr(state.base.0 + offset as usize) }
}

/// # Safety
///
/// The caller must ensure `state_ptr` points to an initialized `VmObjectPhysicalState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_physical_dump(
    state_ptr: *const VmObjectPhysicalState,
    depth: u32,
    cpp_vmo_addr: usize,
    ref_count: i32,
) {
    // SAFETY: The caller guarantees `state_ptr` is valid.
    let state = unsafe { &*state_ptr };
    ksync::lock!(let _guard = state.lock_lock());

    for _ in 0..depth {
        kprint::kprint!("  ");
    }

    kprint::kprintln!(
        "object {:p} base {:#x} size {:#x} ref {}",
        cpp_vmo_addr,
        state.base.0,
        state.size,
        ref_count,
    );
}
