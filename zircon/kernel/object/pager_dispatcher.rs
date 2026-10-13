// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::thread;
use crate::user_copy::UserOutPtr;
use crate::vm::page_source::PageSource;
use crate::vm::vm_object::VmObject;
use core::pin::Pin;
use fbl::{
    Canary, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, Name, RefPtr,
};
use ksync::{KMutex, RawCriticalMutex, guarded, kcell_init};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_PAGER, ZX_PAGER_OP_DIRTY, ZX_PAGER_OP_FAIL,
    ZX_PAGER_OP_WRITEBACK_BEGIN, ZX_PAGER_OP_WRITEBACK_END, ZX_PAGER_RESET_VMO_STATS,
    ZX_RIGHT_ATTACH_VMO, ZX_RIGHT_INSPECT, ZX_RIGHT_MANAGE_VMO, ZX_RIGHT_TRANSFER,
    ZX_VMO_DIRTY_RANGE_IS_ZERO, ZX_VMO_TRAP_DIRTY, zx_pager_vmo_stats_t, zx_rights_t, zx_status_t,
    zx_vaddr_t, zx_vmo_dirty_range_t,
};

use super::KernelHandle;
use super::pager_dispatcher_ffi::{
    cpp_pager_dispatcher_create, cpp_pager_proxy_create, cpp_pager_proxy_create_page_source,
    cpp_pager_proxy_free, cpp_pager_proxy_get_dll_node, cpp_pager_proxy_get_ref_counted,
    cpp_pager_proxy_on_dispatcher_close, cpp_pager_proxy_set_page_source_unchecked,
};
use super::port_dispatcher::PortDispatcher;

/// Default rights assigned to a `PagerDispatcher` handle (`ZX_DEFAULT_PAGER_RIGHTS`).
pub const DEFAULT_RIGHTS: zx_rights_t =
    ZX_RIGHT_INSPECT | ZX_RIGHT_TRANSFER | ZX_RIGHT_ATTACH_VMO | ZX_RIGHT_MANAGE_VMO;

zr::static_assert_size_and_align!(
    PagerDispatcherState,
    object_constants::kPagerDispatcherStateSize,
    object_constants::kPagerDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_PAGER_CREATE_COUNT, "dispatcher.pager.create", Sum);
define_kcounter!(DISPATCHER_PAGER_DESTROY_COUNT, "dispatcher.pager.destroy", Sum);

fbl::impl_opaque_ref_counted_facade!(
    /// Page provider implementation that talks to a userspace pager service.
    #[repr(align(8))]
    pub(crate) struct PagerProxy,
    cpp_pager_proxy_free,
    cpp_pager_proxy_get_ref_counted,
);

impl DoublyLinkedListContainable<PagerProxy> for PagerProxy {
    fn get_node(&self) -> &DoublyLinkedListNode<PagerProxy> {
        // SAFETY: `cpp_pager_proxy_get_dll_node` returns a valid pointer to the embedded
        // `DoublyLinkedListNodeState<fbl::RefPtr<PagerProxy>>` inside `self`, which has the
        // identical two-pointer layout and null/non-null invariants as `DoublyLinkedListNode`.
        unsafe { &*cpp_pager_proxy_get_dll_node(self) }
    }
}

impl PagerProxy {
    /// Option bit indicating that clean-to-dirty transitions should be trapped.
    const TRAP_DIRTY: u32 = object_constants::kPagerProxyTrapDirty;

    /// Creates a new `PagerProxy`.
    fn create(
        dispatcher: &PagerDispatcher,
        port: RefPtr<PortDispatcher>,
        key: u64,
        options: u32,
    ) -> Result<RefPtr<Self>, Status> {
        let port_raw = RefPtr::into_raw(port).cast_mut();
        let mut out_proxy: *mut PagerProxy = core::ptr::null_mut();
        // SAFETY: `dispatcher` is a valid reference, `port_raw` is an owned raw pointer from
        // `RefPtr::into_raw`, and `out_proxy` is a valid out reference.
        let status =
            unsafe { cpp_pager_proxy_create(dispatcher, port_raw, key, options, &mut out_proxy) };
        Status::ok(status)?;
        // SAFETY: `cpp_pager_proxy_create` succeeded and wrote a non-null owned `PagerProxy`
        // pointer to `out_proxy`.
        Ok(unsafe { RefPtr::from_raw(out_proxy) })
    }

    /// Creates a new `PageSource` backed by `proxy`.
    fn create_page_source(proxy: &Self) -> Result<RefPtr<PageSource>, Status> {
        let mut out_src: *mut PageSource = core::ptr::null_mut();
        // SAFETY: `proxy` and `out_src` are valid references.
        let status = unsafe { cpp_pager_proxy_create_page_source(proxy, &mut out_src) };
        Status::ok(status)?;
        // SAFETY: `cpp_pager_proxy_create_page_source` succeeded and wrote a non-null owned
        // `PageSource` pointer to `out_src`.
        Ok(unsafe { RefPtr::from_raw(out_src) })
    }

    /// Called by the pager dispatcher to set the `PageSource` reference. This is guaranteed to
    /// happen exactly once just after construction.
    ///
    /// # Safety
    ///
    /// Must be called at most once per `PagerProxy` instance, immediately after construction, and
    /// the caller must ensure `on_dispatcher_close` is later invoked to break the `RefPtr` cycle
    /// between `PagerProxy` and `PageSource`.
    unsafe fn set_page_source_unchecked(&self, src: RefPtr<PageSource>) {
        let src_raw = RefPtr::into_raw(src).cast_mut();
        // SAFETY: `self` is a valid `PagerProxy` reference, `src_raw` is an owned raw pointer from
        // `RefPtr::into_raw`, and caller upholds the single-initialization and cycle-breaking
        // invariants.
        unsafe { cpp_pager_proxy_set_page_source_unchecked(self, src_raw) }
    }

    /// Called by the pager dispatcher when it is about to go away. Handles cleaning up port's
    /// reference to any in flight packets.
    fn on_dispatcher_close(&self) {
        // SAFETY: `self` is a valid `PagerProxy` reference.
        unsafe { cpp_pager_proxy_on_dispatcher_close(self) }
    }
}

/// Internal state storage for `PagerDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PagerDispatcherState {
    canary: Canary<{ fbl::magic(b"PGRD") }>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    #[pin]
    #[guarded_by(lock)]
    proxies: DoublyLinkedList<RefPtr<PagerProxy>>,

    // Track whether zero handles has been triggered. This prevents race conditions where we might
    // create new sources after on_zero_handles has been called.
    #[guarded_by(lock)]
    triggered_zero_handles: bool,

    #[pin]
    debug_name: Name<ZX_MAX_NAME_LEN>,
}

// SAFETY: `PagerDispatcherState` contains `DoublyLinkedList<RefPtr<PagerProxy>>` which holds raw
// node pointers internally and is therefore `!Send` and `!Sync` by default. All access to `proxies`
// is synchronized by `lock`.
unsafe impl Send for PagerDispatcherState {}
unsafe impl Sync for PagerDispatcherState {}

impl PagerDispatcherState {
    /// Initializes the `PagerDispatcherState`.
    pub fn init(
        _dispatcher: *const PagerDispatcher,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_PAGER_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            lock <- KMutex::init(),
            proxies <- kcell_init(DoublyLinkedList::new()),
            triggered_zero_handles: false.into(),
            debug_name <- Name::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for PagerDispatcherState {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: We have exclusive access during drop and do not move any pinned fields.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(this.proxies.get_inner_mut().is_empty());
        DISPATCHER_PAGER_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher for a userspace pager service.
    pub struct PagerDispatcher,
    PagerDispatcherState,
    ZX_OBJ_TYPE_PAGER,
    object_constants::kPagerDispatcherStateOffset
);

impl PagerDispatcher {
    /// Returns the default rights for a `PagerDispatcher` handle.
    pub fn default_rights() -> zx_rights_t {
        DEFAULT_RIGHTS
    }

    /// Creates a new `PagerDispatcher` and returns its kernel handle and default rights.
    pub fn create() -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `cpp_pager_dispatcher_create` initializes `out` on `ZX_OK`.
        let handle = unsafe { KernelHandle::create(|out| cpp_pager_dispatcher_create(out)) }?;
        Ok((handle, DEFAULT_RIGHTS))
    }

    /// Creates a new `PageSource` linked to this pager dispatcher and `port`.
    pub fn create_source(
        &self,
        port: RefPtr<PortDispatcher>,
        key: u64,
        mut options: u32,
    ) -> Result<RefPtr<PageSource>, Status> {
        ksync::lock!(let mut guard = self.state().lock_lock());
        // Make sure on_zero_handles has not been called. This could happen if a call to
        // pager_create_vmo races with closing the last handle, as pager_create_vmo does not hold
        // the handle table lock over this operation.
        if *guard.fields().triggered_zero_handles {
            return Err(Status::BAD_STATE);
        }

        // Process any options relevant to creation of the PagerProxy.
        let mut proxy_options = 0u32;
        if (options & ZX_VMO_TRAP_DIRTY) != 0 {
            proxy_options = PagerProxy::TRAP_DIRTY;
            options &= !ZX_VMO_TRAP_DIRTY;
        }
        if options != 0 {
            return Err(Status::INVALID_ARGS);
        }

        // We are going to setup two objects that both need to point to each other. As such one of
        // the pointers must be bound 'late' and not in the constructor.
        let proxy = PagerProxy::create(self, port, key, proxy_options)?;
        let src = PagerProxy::create_page_source(&proxy)?;
        // Now that PageSource has been created and has a reference to proxy we must setup expected
        // backlink in proxy. As such there must never be an early return added between here and
        // set_page_source_unchecked.

        // Setting this creates a RefPtr cycle between the PagerProxy and PageSource, however we
        // guarantee we will call proxy.on_dispatcher_close at some point to break the cycle.
        // SAFETY: Called once immediately after construction, and `proxy` is pushed onto `proxies`
        // below so `on_zero_handles` is guaranteed to call `on_dispatcher_close` to break the
        // cycle.
        unsafe { proxy.set_page_source_unchecked(src.clone()) };

        guard.as_mut().fields_mut().proxies.push_front(proxy);
        Ok(src)
    }

    /// Drop and return this object's reference to `proxy`. Must be called under
    /// `proxy`'s lock to prevent races with dispatcher teardown.
    pub(crate) fn release_proxy(&self, proxy: &PagerProxy) -> Option<RefPtr<PagerProxy>> {
        ksync::lock!(let mut guard = self.state().lock_lock());
        let in_container = proxy.get_node().in_container();
        let triggered_zero_handles = *guard.fields().triggered_zero_handles;
        // proxy might not be in the container since we could be racing with a call to
        // on_zero_handles, but that should only happen if we have triggered_zero_handles. Note that
        // it is possible for the proxy to still be in the container even if triggered_zero_handles
        // is true, as we drop the lock between on_dispatcher_close calls for each proxy in the
        // list, so we might not have gotten to this proxy yet.
        debug_assert!(
            in_container || triggered_zero_handles,
            "triggered_zero_handles is {} and proxy is {}in container\n",
            i32::from(triggered_zero_handles),
            if in_container { "" } else { "not " }
        );
        if in_container {
            // SAFETY: `proxy` is in `proxies`.
            unsafe { guard.as_mut().fields_mut().proxies.erase(proxy) }
        } else {
            None
        }
    }

    /// Callback invoked when all handles to this dispatcher are closed.
    pub fn on_zero_handles(&self) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        debug_assert!(!*guard.fields().triggered_zero_handles);
        // Set triggered_zero_handles to true before starting to release proxies, so that a racy
        // call to PagerDispatcher::release_proxy knows it's not incorrect to not find the proxy in
        // the list.
        *guard.as_mut().fields_mut().triggered_zero_handles = true;
        while let Some(proxy) = guard.as_mut().fields_mut().proxies.pop_front() {
            // Call unlocked to prevent a double-lock if PagerDispatcher::release_proxy is called,
            // and to preserve the lock order that PagerProxy locks are acquired before the
            // list lock.
            guard.as_mut().call_unlocked(|| {
                proxy.on_dispatcher_close();
            });
        }
    }

    /// Performs a pager operation `op` on `vmo` across `[offset, offset + length)`.
    pub fn range_op(
        &self,
        op: u32,
        vmo: &VmObject,
        offset: u64,
        length: u64,
        data: u64,
    ) -> Result<(), Status> {
        match op {
            ZX_PAGER_OP_FAIL => {
                let signed_data = data as i64;
                if signed_data < i64::from(i32::MIN) || signed_data > i64::from(i32::MAX) {
                    return Err(Status::INVALID_ARGS);
                }
                let error_status = Status::err_from_raw(data as zx_status_t);
                if !PageSource::is_valid_external_failure_code(error_status) {
                    return Err(Status::INVALID_ARGS);
                }
                vmo.fail_page_requests(offset, length, error_status)
            }
            ZX_PAGER_OP_DIRTY => {
                if data != 0 {
                    return Err(Status::INVALID_ARGS);
                }
                vmo.dirty_pages(offset, length)
            }
            ZX_PAGER_OP_WRITEBACK_BEGIN => {
                if data != 0 && data != ZX_VMO_DIRTY_RANGE_IS_ZERO {
                    return Err(Status::INVALID_ARGS);
                }
                vmo.writeback_begin(offset, length, data == ZX_VMO_DIRTY_RANGE_IS_ZERO)
            }
            ZX_PAGER_OP_WRITEBACK_END => {
                if data != 0 {
                    return Err(Status::INVALID_ARGS);
                }
                vmo.writeback_end(offset, length)
            }
            _ => Err(Status::NOT_SUPPORTED),
        }
    }

    /// Queries dirty ranges in `vmo` across `[offset, offset + length)`.
    ///
    /// May block on page requests and must be called without locks held.
    #[allow(clippy::too_many_arguments)]
    pub fn query_dirty_ranges(
        &self,
        vmo: &VmObject,
        offset: u64,
        length: u64,
        buffer: UserOutPtr<u8>,
        buffer_size: usize,
        actual: UserOutPtr<usize>,
        avail: UserOutPtr<usize>,
    ) -> Result<(), Status> {
        // As we may need to perform fault resolution later, ensure our caller is not holding any
        // locks.
        lockdep::assert_no_locks_held();

        // State captured by |copy_to_buffer| below.
        struct CopyToBufferInfo {
            // Index into |buffer|, used to populate its entries.
            index: usize,
            // Total number of dirty ranges discovered.
            total: usize,
            // Whether the total number of ranges need to be computed, depending on whether |avail|
            // is supplied.
            compute_total: bool,
            // The range that enumeration runs over. Might get updated when enumeration ends early
            // due to a page fault.
            offset: u64,
            length: u64,
            // The buffer to copy out ranges to.
            buffer: UserOutPtr<zx_vmo_dirty_range_t>,
            buffer_size: usize,
            // State that will get populated if the user copy in the dirty_range_fn encounters
            // a page fault.
            pf_va: zx_vaddr_t,
            pf_flags: u32,
            captured_fault_info: bool,
        }

        let mut info = CopyToBufferInfo {
            index: 0,
            total: 0,
            compute_total: !avail.is_null(),
            offset,
            length,
            buffer: buffer.reinterpret::<zx_vmo_dirty_range_t>(),
            buffer_size,
            pf_va: 0,
            pf_flags: 0,
            captured_fault_info: false,
        };

        // Enumerate dirty ranges with |copy_to_buffer|. If page faults are captured, resolve them
        // and retry enumeration.
        loop {
            let (cur_offset, cur_length) = (info.offset, info.length);
            // Enumeration function that will be invoked on each dirty range found.
            let copy_to_buffer =
                |range_offset: u64, range_len: u64, range_is_zero: bool| -> Result<(), Status> {
                    let buffer_full = |index: usize, buffer_size: usize| {
                        (index + 1) * core::mem::size_of::<zx_vmo_dirty_range_t>() > buffer_size
                    };
                    // No more space in the buffer.
                    if buffer_full(info.index, info.buffer_size) {
                        // If we were not asked to compute the total, we can end termination early
                        // as there is nothing more to copy out. Although we would have terminated
                        // at the bottom of this loop if out of space, this could be our first
                        // iteration and so this check is still needed.
                        if !info.compute_total {
                            debug_assert!(info.index == 0);
                            return Err(Status::STOP);
                        }
                        // As there is no more space in the |buffer|, only update the total without
                        // trying to copy out any more ranges.
                        info.total += 1;
                        return Err(Status::NEXT);
                    }

                    let dirty_range = zx_vmo_dirty_range_t {
                        offset: range_offset,
                        length: range_len,
                        options: if range_is_zero { ZX_VMO_DIRTY_RANGE_IS_ZERO } else { 0 },
                    };

                    let copy_result = info
                        .buffer
                        .element_offset(info.index)
                        .copy_to_user_capture_faults(&dirty_range);
                    // Stash fault information if a fault is encountered. Return early from
                    // enumeration with Status::SHOULD_WAIT so that the page fault can be resolved.
                    if let Err(copy_err) = copy_result {
                        let Some(fault_info) = copy_err.fault_info else {
                            return Err(copy_err.status);
                        };
                        info.captured_fault_info = true;
                        info.pf_va = fault_info.pf_va;
                        info.pf_flags = fault_info.pf_flags;

                        // Update the offset and length to skip over the range that we've already
                        // processed dirty ranges for, to allow forward progress of the syscall.
                        let processed = range_offset - info.offset;
                        info.offset += processed;
                        info.length -= processed;

                        return Err(Status::SHOULD_WAIT);
                    }
                    // We were able to successfully copy out this dirty range. Advance the index and
                    // continue with the enumeration if we need to consider more ranges.
                    info.index += 1;
                    info.total += 1;
                    if !info.compute_total && buffer_full(info.index, info.buffer_size) {
                        // No need to compute the total and the buffer is full, so can cease
                        // considering additional ranges. This is equivalent to the start the loop,
                        // but by doing it here we save the need to calculate the next range before
                        // noticing the buffer is full and terminating.
                        return Err(Status::STOP);
                    }
                    Err(Status::NEXT)
                };

            let status = vmo.enumerate_dirty_ranges(cur_offset, cur_length, copy_to_buffer);
            // Per |copy_to_buffer|, enumeration will terminate early with Status::SHOULD_WAIT if a
            // fault is captured. Resolve the fault and then attempt the enumeration again.
            match status {
                Ok(()) => break,
                Err(Status::SHOULD_WAIT) => {
                    debug_assert!(info.captured_fault_info);
                    if thread::soft_fault(info.pf_va, info.pf_flags).is_err() {
                        return Err(Status::INVALID_ARGS);
                    }
                    // Reset |captured_fault_info| so that a future page fault can set it again.
                    info.captured_fault_info = false;
                }
                Err(e) => {
                    // Another error was encountered. Return.
                    return Err(e);
                }
            }
        }

        // Now try to copy out the total and actual number of ranges we populated in |buffer|. We
        // don't need to use copy_to_user_capture_faults() here; we don't hold any locks that need
        // to be dropped before handling a fault.
        if !actual.is_null() {
            actual.copy_to_user(&info.index)?;
        }
        if !avail.is_null() {
            debug_assert!(info.total >= info.index);
            avail.copy_to_user(&info.total)?;
        }
        Ok(())
    }

    /// Queries pager VMO statistics for `vmo`.
    ///
    /// May block on page requests and must be called without locks held.
    pub fn query_pager_vmo_stats(
        &self,
        vmo: &VmObject,
        mut options: u32,
        buffer: UserOutPtr<u8>,
        buffer_size: usize,
    ) -> Result<(), Status> {
        // As we may need to perform fault resolution later, ensure our caller is not holding any
        // locks.
        lockdep::assert_no_locks_held();

        if buffer_size < core::mem::size_of::<zx_pager_vmo_stats_t>() {
            return Err(Status::BUFFER_TOO_SMALL);
        }

        let reset = (options & ZX_PAGER_RESET_VMO_STATS) != 0;
        options &= !ZX_PAGER_RESET_VMO_STATS;
        if options != 0 {
            return Err(Status::INVALID_ARGS);
        }

        let stats = vmo.query_pager_vmo_stats(reset)?;

        loop {
            let copy_result =
                buffer.reinterpret::<zx_pager_vmo_stats_t>().copy_to_user_capture_faults(&stats);
            let Err(copy_err) = copy_result else {
                break;
            };
            let Some(fault_info) = copy_err.fault_info else {
                return Err(copy_err.status);
            };
            if thread::soft_fault(fault_info.pf_va, fault_info.pf_flags).is_err() {
                return Err(Status::INVALID_ARGS);
            }
        }

        Ok(())
    }

    /// Sets the debug name of this pager dispatcher.
    pub fn set_debug_name(&self, name: &[u8]) {
        self.state().debug_name.set(name);
    }

    /// Copies the debug name of this pager dispatcher into `out_name`.
    pub fn get_debug_name(&self, out_name: &mut [u8]) {
        self.state().debug_name.get(out_name);
    }
}

/// Kernel unit tests for `PagerDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "pager_dispatcher_tests")]
mod tests {
    use super::{
        DEFAULT_RIGHTS, PagerDispatcher, Status, UserOutPtr, ZX_MAX_NAME_LEN, ZX_PAGER_OP_DIRTY,
        ZX_PAGER_OP_FAIL, ZX_PAGER_OP_WRITEBACK_BEGIN, ZX_PAGER_OP_WRITEBACK_END,
        ZX_PAGER_RESET_VMO_STATS, ZX_VMO_DIRTY_RANGE_IS_ZERO, ZX_VMO_TRAP_DIRTY,
        zx_pager_vmo_stats_t, zx_vmo_dirty_range_t,
    };
    use crate::object::PortDispatcher;
    use crate::user_memory::UserMemory;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use page::SIZE as PAGE_SIZE_USIZE;
    use zx_types::{
        ZX_ERR_BAD_STATE, ZX_ERR_BUFFER_TOO_SMALL, ZX_ERR_IO, ZX_ERR_IO_DATA_INTEGRITY,
        ZX_ERR_NO_SPACE, ZX_ERR_STOP, ZX_OK,
    };

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    /// Tests creating a `PagerDispatcher` and getting/setting its debug name.
    #[test]
    fn test_pager_dispatcher_create_and_debug_name() {
        let (handle, rights) = PagerDispatcher::create().expect("create pager");
        unittest::expect_eq!(rights, DEFAULT_RIGHTS);
        handle.dispatcher().set_debug_name(b"test-pager");
        let mut buf = [0u8; ZX_MAX_NAME_LEN];
        handle.dispatcher().get_debug_name(&mut buf);
        unittest::expect_true!(&buf[..10] == b"test-pager");
        unittest::expect_eq!(buf[10], 0);
    }

    /// Tests creating a `PageSource` from a `PagerDispatcher` and handling `on_zero_handles`.
    #[test]
    fn test_pager_dispatcher_create_source() {
        let (pager, _) = PagerDispatcher::create().expect("create pager");
        let (port, _) = PortDispatcher::create(0).expect("create port");
        let port_ref = port.dispatcher().clone();

        // Invalid options should fail.
        unittest::expect_true!(
            pager.dispatcher().create_source(port_ref.clone(), 1, u32::MAX).err()
                == Some(Status::INVALID_ARGS)
        );

        // Valid source creation should succeed.
        let _src =
            pager.dispatcher().create_source(port_ref.clone(), 42, 0).expect("create source");

        // After dropping the pager handle (triggering on_zero_handles), creating a source should
        // fail with BAD_STATE.
        let pager_ref = pager.dispatcher().clone();
        drop(pager);
        unittest::expect_true!(
            pager_ref.create_source(port_ref, 43, 0).err() == Some(Status::BAD_STATE)
        );
    }

    /// Tests `PagerDispatcher::range_op` validation and operations.
    #[test]
    fn test_pager_dispatcher_range_op() {
        let (pager, _) = PagerDispatcher::create().expect("create pager");
        let (port, _) = PortDispatcher::create(0).expect("create port");
        let src = pager
            .dispatcher()
            .create_source(port.dispatcher().clone(), 1, ZX_VMO_TRAP_DIRTY)
            .expect("create source");
        let vmo = VmObjectPaged::create_external(src, 0, PAGE_SIZE * 4).expect("create vmo");

        // Unsupported op should return NOT_SUPPORTED.
        unittest::expect_true!(
            pager.dispatcher().range_op(u32::MAX, &vmo, 0, PAGE_SIZE, 0)
                == Err(Status::NOT_SUPPORTED)
        );

        // ZX_PAGER_OP_FAIL validation: 0, positive, or non-whitelisted error codes fail with
        // INVALID_ARGS.
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_FAIL, &vmo, 0, PAGE_SIZE, ZX_OK as u64)
                == Err(Status::INVALID_ARGS)
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_FAIL, &vmo, 0, PAGE_SIZE, 1)
                == Err(Status::INVALID_ARGS)
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(
                ZX_PAGER_OP_FAIL,
                &vmo,
                0,
                PAGE_SIZE,
                ZX_ERR_STOP as i64 as u64
            ) == Err(Status::INVALID_ARGS)
        );

        // Allowed error statuses for ZX_PAGER_OP_FAIL should succeed.
        for err in [
            ZX_ERR_IO,
            ZX_ERR_IO_DATA_INTEGRITY,
            ZX_ERR_BAD_STATE,
            ZX_ERR_NO_SPACE,
            ZX_ERR_BUFFER_TOO_SMALL,
        ] {
            unittest::expect_true!(
                pager.dispatcher().range_op(
                    ZX_PAGER_OP_FAIL,
                    &vmo,
                    0,
                    PAGE_SIZE,
                    err as i64 as u64
                ) == Ok(())
            );
        }

        // ZX_PAGER_OP_DIRTY and ZX_PAGER_OP_WRITEBACK_END require data == 0.
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_DIRTY, &vmo, 0, PAGE_SIZE, 1)
                == Err(Status::INVALID_ARGS)
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_WRITEBACK_END, &vmo, 0, PAGE_SIZE, 1)
                == Err(Status::INVALID_ARGS)
        );

        // ZX_PAGER_OP_WRITEBACK_BEGIN only accepts 0 or ZX_VMO_DIRTY_RANGE_IS_ZERO.
        unittest::expect_true!(
            pager.dispatcher().range_op(
                ZX_PAGER_OP_WRITEBACK_BEGIN,
                &vmo,
                0,
                PAGE_SIZE,
                !ZX_VMO_DIRTY_RANGE_IS_ZERO
            ) == Err(Status::INVALID_ARGS)
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_WRITEBACK_BEGIN, &vmo, 0, PAGE_SIZE, 0)
                == Ok(())
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(
                ZX_PAGER_OP_WRITEBACK_BEGIN,
                &vmo,
                0,
                PAGE_SIZE,
                ZX_VMO_DIRTY_RANGE_IS_ZERO
            ) == Ok(())
        );
        unittest::expect_true!(
            pager.dispatcher().range_op(ZX_PAGER_OP_WRITEBACK_END, &vmo, 0, PAGE_SIZE, 0) == Ok(())
        );
    }

    /// Tests `PagerDispatcher::query_pager_vmo_stats`.
    #[test]
    fn test_pager_dispatcher_query_pager_vmo_stats() {
        let (pager, _) = PagerDispatcher::create().expect("create pager");
        let (port, _) = PortDispatcher::create(0).expect("create port");
        let src = pager
            .dispatcher()
            .create_source(port.dispatcher().clone(), 1, 0)
            .expect("create source");
        let vmo = VmObjectPaged::create_external(src, 0, PAGE_SIZE * 2).expect("create vmo");

        let mem = UserMemory::create(PAGE_SIZE_USIZE).expect("create user memory");
        mem.commit_and_map(0..PAGE_SIZE_USIZE).expect("commit and map");

        let stats_size = size_of::<zx_pager_vmo_stats_t>();

        // Invalid options should return INVALID_ARGS.
        unittest::expect_true!(
            pager.dispatcher().query_pager_vmo_stats(
                &vmo,
                u32::MAX,
                mem.user_out::<u8>(),
                stats_size
            ) == Err(Status::INVALID_ARGS)
        );

        // Insufficient buffer_size should return BUFFER_TOO_SMALL.
        unittest::expect_true!(
            pager.dispatcher().query_pager_vmo_stats(&vmo, 0, mem.user_out::<u8>(), 0)
                == Err(Status::BUFFER_TOO_SMALL)
        );
        unittest::expect_true!(
            pager.dispatcher().query_pager_vmo_stats(&vmo, 0, mem.user_out::<u8>(), stats_size - 1)
                == Err(Status::BUFFER_TOO_SMALL)
        );

        // Pre-populate user memory with non-zero bytes to verify the stats output is written.
        mem.put::<u32>(0xdead_beef, 0).expect("put u32");
        unittest::expect_true!(
            pager.dispatcher().query_pager_vmo_stats(
                &vmo,
                ZX_PAGER_RESET_VMO_STATS,
                mem.user_out::<u8>(),
                stats_size
            ) == Ok(())
        );
        let modified = mem.get::<u32>(0).expect("get modified field");
        unittest::expect_eq!(modified, 0);
    }

    /// Tests `PagerDispatcher::query_dirty_ranges`.
    #[test]
    fn test_pager_dispatcher_query_dirty_ranges() {
        let (pager, _) = PagerDispatcher::create().expect("create pager");
        let (port, _) = PortDispatcher::create(0).expect("create port");
        let src = pager
            .dispatcher()
            .create_source(port.dispatcher().clone(), 1, ZX_VMO_TRAP_DIRTY)
            .expect("create source");
        let vmo = VmObjectPaged::create_external(src, 0, PAGE_SIZE * 4).expect("create vmo");

        // Null output pointers with zero buffer size should succeed on a clean VMO.
        unittest::expect_true!(
            pager.dispatcher().query_dirty_ranges(
                &vmo,
                0,
                PAGE_SIZE * 4,
                UserOutPtr::new(core::ptr::null_mut()),
                0,
                UserOutPtr::new(core::ptr::null_mut()),
                UserOutPtr::new(core::ptr::null_mut())
            ) == Ok(())
        );

        // Query with valid user buffers for ranges, actual, and avail.
        let mem = UserMemory::create(PAGE_SIZE_USIZE).expect("create user memory");
        mem.commit_and_map(0..PAGE_SIZE_USIZE).expect("commit and map");

        // Lay out [zx_vmo_dirty_range_t; 2] at offset 0, `actual: usize` at offset 64, and
        // `avail: usize` at offset 72.
        let actual_offset = 64 / size_of::<usize>();
        let avail_offset = 72 / size_of::<usize>();
        mem.put::<usize>(999, actual_offset).expect("init actual");
        mem.put::<usize>(999, avail_offset).expect("init avail");

        let buffer_ptr = mem.user_out::<u8>();
        let actual_ptr = mem.user_out::<usize>().element_offset(actual_offset);
        let avail_ptr = mem.user_out::<usize>().element_offset(avail_offset);

        unittest::expect_true!(
            pager.dispatcher().query_dirty_ranges(
                &vmo,
                0,
                PAGE_SIZE * 4,
                buffer_ptr,
                2 * size_of::<zx_vmo_dirty_range_t>(),
                actual_ptr,
                avail_ptr
            ) == Ok(())
        );

        unittest::expect_eq!(mem.get::<usize>(actual_offset).expect("read actual"), 0);
        unittest::expect_eq!(mem.get::<usize>(avail_offset).expect("read avail"), 0);
    }
}
