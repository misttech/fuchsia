// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod wake_vector_ffi;

use crate::arch_rs::{blocking_disallowed, ints_disabled, set_blocking_disallowed};
use crate::kernel::idle_power_thread::IdlePowerThread;
use crate::kernel::thread::AutoPreemptDisabler;
use crate::platform_rs::timer::current_boot_time;
use crate::user_copy::UserOutPtr;
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use ksync::{KCell, LockToken};
use zx_status::Status;
use zx_types::{
    ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_PREVIOUSLY_REPORTED,
    ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED, ZX_TIME_INFINITE, zx_instant_boot_t, zx_koid_t,
    zx_wake_source_report_entry_t, zx_wake_source_report_header_t,
};

use self::lists::{
    GlobalListLock, ListNodes, PendingCursor, PendingGuard, PendingListLock, global_list,
    pending_list,
};

/// Diagnostic information about the wake vector managed by the implementor of [`WakeVector`].
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Diagnostics {
    /// Indicates that the given wake vector is enabled and can generate wake events. Disabled wake
    /// vectors are not listed in diagnostic logs.
    pub enabled: bool,

    /// The koid of the object implementing this interface, if any.
    pub koid: zx_koid_t,

    /// Extra information specific to the wake vector that can aid in determining the source of the
    /// wake event and potentially its state.
    pub extra: [u8; ZX_MAX_NAME_LEN],
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self { enabled: false, koid: ZX_KOID_INVALID, extra: [0; ZX_MAX_NAME_LEN] }
    }
}

zr::static_assert!(size_of::<Diagnostics>() == 48);
zr::static_assert!(align_of::<Diagnostics>() == 8);
zr::static_assert!(offset_of!(Diagnostics, enabled) == 0);
zr::static_assert!(offset_of!(Diagnostics, koid) == 8);
zr::static_assert!(offset_of!(Diagnostics, extra) == 16);

/// `WakeVector` is an interface implemented by objects that will generate system wake events using
/// the [`WakeEvent`] type. This interface provides diagnostic information about the wake vector to
/// the suspend subsystem.
///
/// The C++ base class `wake_vector::WakeVector` is represented as an opaque type in Rust and its
/// virtual `GetDiagnostics` method is invoked via an FFI shim.
#[repr(C)]
pub struct WakeVector(zr::Opaque<[u8; 0]>);

impl WakeVector {
    /// Provides diagnostic information about the wake vector object implementing this interface.
    pub fn get_diagnostics(&self, diagnostics_out: &mut Diagnostics) {
        // SAFETY: `self` is a valid reference to a C++ `wake_vector::WakeVector` instance, and
        // `diagnostics_out` is a valid mutable reference to `Diagnostics`.
        unsafe {
            wake_vector_ffi::cpp_wake_vector_get_diagnostics(self, diagnostics_out);
        }
    }
}

/// The result of a request to wake up the system.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WakeResult {
    /// The system was not suspended at the time of the event.
    Active = 0,

    /// The system was suspended at the time of the wake event.
    Resumed = 1,

    /// The system was in the process of suspending at the time of the wake event.
    SuspendAborted = 2,

    /// An already pending wake event was triggered again.
    BadState = 3,
}

/// Behavior when acknowledging a [`WakeEvent`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AckBehavior {
    ClearSignaled = 0,
    RemainSignaled = 1,
}

fn limit_size_t_u32(size: usize) -> u32 {
    u32::try_from(size).unwrap_or(u32::MAX)
}

/// Encapsulates the global and pending intrusive `WakeEvent` lists, their singleton locks, and
/// `PendingCursor` so the rest of this module cannot violate list or lock invariants in safe Rust.
///
/// ## Lock Hierarchy and Guarded State
///
/// When both locks are acquired, `GlobalListLock` is always acquired before `PendingListLock`.
///
/// - **`GlobalListLock` (mutex)**: Guards `GLOBAL_LIST` and each `WakeEvent`'s `global_node`.
///   - Operations requiring only `GlobalListLock`: [`GlobalListRef::push_back`],
///     [`GlobalListRef::erase`], [`GlobalListRef::iter`], [`GlobalListRef::len`], and
///     [`ListNodes::in_global_list`].
/// - **`PendingListLock` (IRQ-safe spinlock)**: Guards `PENDING_LIST`, each `WakeEvent`'s
///   `pending_node`, and `WakeEvent::report_info`.
///   - Operations requiring only `PendingListLock`: reading/mutating `report_info`,
///     [`ListNodes::in_pending_list`], [`PendingListRef::is_empty`], [`PendingListRef::len`], and
///     [`PendingListRef::push_front`].
/// - **Both `GlobalListLock` (exclusive token) and `PendingListLock`**: Required to remove nodes
///   from `PENDING_LIST` ([`PendingListRef::erase`]) or to iterate `PENDING_LIST` across unlocked
///   windows ([`PendingCursor::new`], [`PendingCursor::next`], [`PendingCursor::erase_current`]).
///   Holding `GlobalListLock` for the lifetime of [`PendingCursor`] prevents any `WakeEvent` from
///   being destroyed or removed from `PENDING_LIST` while `PendingListLock` is temporarily dropped
///   via `call_unlocked`.
mod lists {
    use super::WakeEvent;
    use core::marker::PhantomPinned;
    use core::pin::Pin;
    use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, TrackingSize};
    use ksync::{KMutexGuard, KOnceCell, LockToken, RawMutex, RawSpinlock};
    use pin_init::{pin_data, pin_init};

    struct GlobalListTag;
    struct PendingListTag;

    /// Intrusive list nodes embedded in [`WakeEvent`]. Fields are private to `lists` so only this
    /// module can link or unlink a `WakeEvent`.
    #[repr(C)]
    pub struct ListNodes {
        global_node: DoublyLinkedListNode<WakeEvent>,
        pending_node: DoublyLinkedListNode<WakeEvent>,
        _pin: PhantomPinned,
    }

    impl ListNodes {
        /// Creates unlinked intrusive list nodes for a [`WakeEvent`].
        ///
        /// # Safety
        ///
        /// The caller must ensure that the containing [`WakeEvent`] is not moved while linked into
        /// either list, that it is only linked into [`PENDING_LIST`] while also linked in
        /// [`GLOBAL_LIST`], and that it is removed from both lists before being dropped.
        pub const unsafe fn new() -> Self {
            Self {
                global_node: DoublyLinkedListNode::new(),
                pending_node: DoublyLinkedListNode::new(),
                _pin: PhantomPinned,
            }
        }

        #[inline]
        pub fn in_global_list(&self, _token: &LockToken<'_, GlobalListLock>) -> bool {
            self.global_node.in_container()
        }

        #[inline]
        pub fn in_pending_list(&self, _token: &LockToken<'_, PendingListLock>) -> bool {
            self.pending_node.in_container()
        }

        #[inline]
        pub fn in_global_list_exclusive(&mut self) -> bool {
            self.global_node.in_container()
        }

        #[inline]
        pub fn in_pending_list_exclusive(&mut self) -> bool {
            self.pending_node.in_container()
        }
    }

    impl DoublyLinkedListContainable<WakeEvent, GlobalListTag> for WakeEvent {
        fn get_node(&self) -> &DoublyLinkedListNode<WakeEvent> {
            &self.list_nodes.global_node
        }
    }

    impl DoublyLinkedListContainable<WakeEvent, PendingListTag> for WakeEvent {
        fn get_node(&self) -> &DoublyLinkedListNode<WakeEvent> {
            &self.list_nodes.pending_node
        }
    }

    type GlobalList = DoublyLinkedList<*mut WakeEvent, GlobalListTag, TrackingSize>;
    type PendingList = DoublyLinkedList<*mut WakeEvent, PendingListTag, TrackingSize>;

    // `fbl::DoublyLinkedList` uses a self-referential sentinel pointer and must be pin-initialized
    // in place inside `KOnceCell`.
    #[pin_data]
    struct GlobalListHolder {
        #[pin]
        list: GlobalList,
    }

    // SAFETY: Access to `GlobalListHolder` is serialized by `GlobalListLock`.
    unsafe impl Send for GlobalListHolder {}

    #[pin_data]
    struct PendingListHolder {
        #[pin]
        list: PendingList,
    }

    // SAFETY: Access to `PendingListHolder` is serialized by `PendingListLock`.
    unsafe impl Send for PendingListHolder {}

    ksync::declare_singleton_mutex!(pub GlobalListLock);
    ksync::declare_singleton_spinlock!(pub PendingListLock);

    pub type GlobalGuard<'a> = Pin<&'a mut KMutexGuard<'static, GlobalListLock, RawMutex>>;
    pub type PendingGuard<'a> = Pin<&'a mut KMutexGuard<'static, PendingListLock, RawSpinlock>>;

    static GLOBAL_LIST: KOnceCell<GlobalListHolder, GlobalListLock> = KOnceCell::new();
    static PENDING_LIST: KOnceCell<PendingListHolder, PendingListLock> = KOnceCell::new();

    pub fn global_list(guard: GlobalGuard<'_>) -> GlobalListRef<'_> {
        // SAFETY: `guard.token_mut()` belongs to the `GlobalListLock` singleton that protects
        // `GLOBAL_LIST`.
        let holder = unsafe {
            GLOBAL_LIST
                .get_or_pin_init(
                    pin_init!(GlobalListHolder { list <- GlobalList::new() }),
                    guard.token_mut(),
                )
                .unwrap()
        };
        // SAFETY: `GLOBAL_LIST` is a `static` cell, so `holder.list` is pinned in static memory.
        GlobalListRef { list: unsafe { Pin::new_unchecked(&mut holder.list) } }
    }

    pub fn pending_list(guard: PendingGuard<'_>) -> PendingListRef<'_> {
        // SAFETY: `guard.token_mut()` belongs to the `PendingListLock` singleton that protects
        // `PENDING_LIST`.
        let holder = unsafe {
            PENDING_LIST
                .get_or_pin_init(
                    pin_init!(PendingListHolder { list <- PendingList::new() }),
                    guard.token_mut(),
                )
                .unwrap()
        };
        // SAFETY: `PENDING_LIST` is a `static` cell, so `holder.list` is pinned in static memory.
        PendingListRef { list: unsafe { Pin::new_unchecked(&mut holder.list) } }
    }

    /// Safe handle to [`GLOBAL_LIST`] obtained while holding [`GlobalListLock`].
    pub struct GlobalListRef<'a> {
        list: Pin<&'a mut GlobalList>,
    }

    impl<'a> GlobalListRef<'a> {
        pub fn push_back(&mut self, event: Pin<&WakeEvent>) {
            assert!(!event.list_nodes.global_node.in_container());
            // SAFETY: `!event.list_nodes.global_node.in_container()` is asserted above while
            // holding `GlobalListLock`, `event` is pinned, and `WakeEvent::new`'s safety contract
            // guarantees `event` remains valid until `destroy` removes it while holding
            // `GlobalListLock`.
            unsafe {
                self.list.as_mut().push_back_raw(core::ptr::from_ref(event.get_ref()).cast_mut());
            }
        }

        pub fn erase(&mut self, event: &WakeEvent) {
            // SAFETY: `GlobalListTag` is private to `lists` and only ever used with the singleton
            // `GLOBAL_LIST`. `DoublyLinkedList::erase` is a no-op if the node is not in the list.
            unsafe {
                self.list.as_mut().erase(event);
            }
        }

        pub fn iter(&self) -> impl Iterator<Item = &WakeEvent> {
            self.list.iter()
        }

        pub fn len(&self) -> usize {
            self.list.len()
        }
    }

    /// Safe handle to [`PENDING_LIST`] obtained while holding [`PendingListLock`].
    pub struct PendingListRef<'a> {
        list: Pin<&'a mut PendingList>,
    }

    impl<'a> PendingListRef<'a> {
        pub fn is_empty(&self) -> bool {
            self.list.is_empty()
        }

        pub fn len(&self) -> usize {
            self.list.len()
        }

        /// Inserts `event` at the front of [`PENDING_LIST`] if it is not already in the list.
        /// Pushing to the front ensures newly triggered events are not visited by an already
        /// in-progress [`PendingCursor`] report pass.
        pub fn push_front(&mut self, event: &WakeEvent) {
            if !event.list_nodes.pending_node.in_container() {
                // SAFETY: `!event.list_nodes.pending_node.in_container()` is checked above while
                // holding `PendingListLock`, and `WakeEvent::new`'s safety contract guarantees
                // `event` remains valid until `destroy` removes it.
                unsafe {
                    self.list.as_mut().push_front_raw(core::ptr::from_ref(event).cast_mut());
                }
            }
        }

        /// Removes `event` from [`PENDING_LIST`]. Requires an exclusive `_global_guard` in
        /// addition to `&mut self` ([`PendingListLock`]) so removal cannot occur on another thread
        /// or on the same thread while a [`PendingCursor`] is active.
        pub fn erase(&mut self, _global_guard: GlobalGuard<'_>, event: &WakeEvent) {
            // SAFETY: `PendingListTag` is private to `lists` and only ever used with the singleton
            // `PENDING_LIST`, and `_global_guard` proves exclusive access to `GlobalListLock` with
            // no active `PendingCursor`. `DoublyLinkedList::erase` is a no-op if `event` is not in
            // the list.
            unsafe {
                self.list.as_mut().erase(event);
            }
        }
    }

    /// Cursor over [`PENDING_LIST`] whose item lifetime `'g` is tied to an exclusive borrow of
    /// [`GlobalListLock`].
    ///
    /// Exclusively borrowing [`GlobalGuard`] for `'g` prevents both other threads and the current
    /// thread from calling [`WakeEvent::destroy`] or [`PendingListRef::erase`] while the cursor is
    /// alive, so `self.next` remains valid and linked in [`PENDING_LIST`] even if
    /// [`PendingListLock`] is temporarily dropped between iterations. The current element can be
    /// removed via [`PendingCursor::erase_current`], because `self.next` has already advanced past
    /// `self.curr`.
    pub struct PendingCursor<'g> {
        curr: Option<&'g WakeEvent>,
        next: Option<&'g WakeEvent>,
    }

    impl<'g> PendingCursor<'g> {
        /// Creates a cursor positioned at the front of [`PENDING_LIST`].
        pub fn new(_global_guard: GlobalGuard<'g>, pending_guard: PendingGuard<'_>) -> Self {
            let list = pending_list(pending_guard).list;
            // SAFETY: Any node in `PENDING_LIST` is also registered in `GLOBAL_LIST` and cannot be
            // destroyed while `_global_guard` (`GlobalListLock`) is held.
            let next = list.front().map(|front| unsafe { &*core::ptr::from_ref(front) });
            Self { curr: None, next }
        }

        /// Returns the current [`WakeEvent`] and advances the cursor to the next element in
        /// [`PENDING_LIST`].
        pub fn next(&mut self, pending_guard: PendingGuard<'_>) -> Option<&'g WakeEvent> {
            self.curr = self.next.take();
            let curr = self.curr?;
            let list = pending_list(pending_guard).list;
            // SAFETY: `curr` is still in `PENDING_LIST` because `_global_guard` (`GlobalGuard<'g>`)
            // has been exclusively borrowed since `curr` was observed in `PENDING_LIST`, preventing
            // `PendingListRef::erase` and `WakeEvent::destroy`, and `erase_current` only ever
            // removes `self.curr` (which is distinct from `self.next`).
            let mut cursor = unsafe { list.cursor_at(curr) };
            cursor.move_next();
            // SAFETY: Any node in `PENDING_LIST` remains valid for `'g` while `GlobalListLock` is
            // held.
            self.next = cursor.get().map(|next| unsafe { &*core::ptr::from_ref(next) });
            Some(curr)
        }

        /// Removes the element most recently yielded by [`PendingCursor::next`] from
        /// [`PENDING_LIST`].
        ///
        /// Safe because `self.next` has already advanced past `self.curr`, and `self.curr.take()`
        /// ensures the element can only be removed once even if it is re-triggered after
        /// [`PendingListLock`] is dropped.
        pub fn erase_current(&mut self, pending_guard: PendingGuard<'_>) {
            if let Some(curr) = self.curr.take() {
                let list = pending_list(pending_guard).list;
                // SAFETY: `PendingListTag` is private to `lists` and only ever used with the
                // singleton `PENDING_LIST`. `self.next` has already moved past `curr`.
                unsafe {
                    list.erase(curr);
                }
            }
        }
    }
}

/// `WakeEvent` manages the lifecycle of wake events triggered by wake vectors.
///
/// A system wake event may be triggered in response to an appropriately configured interrupt,
/// exception, timer, or other future wake source that should resume the system from a suspended
/// state. When a wake event is triggered, it enters the pending state and will prevent the system
/// from entering suspend until it has been acknowledged. A pending wake event is automatically
/// acknowledged when the `WakeEvent` instance is destroyed to prevent missing an acknowledgement
/// that would render the system unable to suspend.
///
/// `WakeEvent` maintains a global list of all instances for diagnostic purposes (i.e. logging wake
/// events that are pending before, during, and after suspend). `WakeEvent`s are added to and
/// removed from the global list using [`WakeEvent::initialize`] and [`WakeEvent::destroy`],
/// respectively. Because diagnostics access each `WakeEvent`, and its containing [`WakeVector`],
/// from the global list, care must be taken to avoid potential use-after-free hazards.
///
/// Users of `WakeEvent`s MUST adhere to the following rules:
/// 1. A `WakeEvent` object MUST be instantiated as a member of a type that implements the
///    [`WakeVector`] interface, such that the lifetime of the containing type encloses the
///    lifetime of the `WakeEvent`. DO NOT heap allocate `WakeEvent` instances separately from the
///    referenced `WakeVector`.
/// 2. The container of a `WakeEvent` instance SHOULD call [`WakeEvent::initialize`] during
///    construction / initialization to register the `WakeEvent` on the global list. The container
///    MAY skip the call to `WakeEvent::initialize` if initialization of the containing type fails.
/// 3. The container of a `WakeEvent` MUST call [`WakeEvent::destroy`] IFF
///    `WakeEvent::initialize` has been called previously on the same instance of `WakeEvent` AND
///    `WakeEvent::destroy` MUST be called BEFORE the containing type itself is destructed.
///
/// Calls to `WakeEvent::initialize` and `WakeEvent::destroy` must always be balanced. However, a
/// `WakeEvent` MAY be initialized and destroyed more than once, as long as it is destroyed before
/// its destructor is invoked.
///
/// ## Notes on the pending list, locks, and concurrency
///
/// It is a requirement that every wake source in the system which has become signaled since last
/// being reported be present in any wake source report generated for a caller of
/// `zx_system_suspend_enter`. They will continue to be reported to users until they have been
/// *both* acknowledged, and reported at least once.
///
/// The `PENDING_LIST` holds the current list of wake events waiting to be reported. Members of
/// the list should remain on the list provided that:
///
/// 1) They have been signaled at some point in the past, at least once.
/// 2) They are either not-yet-acknowledged, or not-yet-reported, or both.
///
/// The `PendingListLock` is a spinlock used to protect the integrity of `PENDING_LIST`, however
/// due to another requirement, it alone is not sufficient. Specifically, it is a requirement that
/// generating a report can never hold off interrupt processing for O(n) time. This requirement
/// would be violated if we had to hold `PendingListLock` for the duration of a report-generation
/// operation. To avoid violating this requirement, we drop `PendingListLock` each time through the
/// loop while iterating through the pending list during report generation.
///
/// The need to drop `PendingListLock` during iteration while generating a report leads to two
/// other potential bad behaviors which we need to protect against:
///
/// 1) During report generation, we are holding an iterator to an element in the list. This
///    iterator cannot become invalidated during the period where we don't hold the lock.
/// 2) We must never "double report" a wake source in a single report. IOW - if KOID X shows up
///    once in the report generated for the user, it must not show up any more times *in that
///    specific report*.
///
/// ### Avoiding iterator invalidation
///
/// Report generation holds `GlobalListLock` (a mutex) for the duration of a report generation
/// operation. Construction/Destruction operations must also hold `GlobalListLock` meaning that
/// they cannot invalidate a report operation's iterator as they cannot run concurrently with the
/// report generation. Trigger operations only add items, and therefore cannot invalidate an
/// intrusive list iterator.
///
/// Ack operations will never remove an element from the list. Instead, they merely mark the
/// element as ack'ed and depend on report generation to handle the removal for them, avoiding the
/// invalidation issue in the process.
///
/// ### Avoiding double reports
///
/// When an event becomes triggered, it is added to the *front* of the list instead of the back.
/// This ensures that once report generation has started and the initial iterator has been
/// computed, no newly triggered events can show up in this report. They will have to wait to ride
/// the next report-train.
#[repr(C)]
pub struct WakeEvent {
    list_nodes: ListNodes,
    wake_vector: NonNull<WakeVector>,
    report_info: KCell<zx_wake_source_report_entry_t, PendingListLock>,
}

zr::static_assert!(size_of::<WakeEvent>() == 112);
zr::static_assert!(align_of::<WakeEvent>() == 8);

// SAFETY: `WakeEvent` synchronizes all mutable access to its intrusive nodes and `report_info`
// through `GlobalListLock` and `PendingListLock`.
unsafe impl Send for WakeEvent {}
// SAFETY: Shared references to `WakeEvent` only mutate state while holding `GlobalListLock` or
// `PendingListLock`.
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    pub fn has_pending_wake_events() -> bool {
        ksync::lock!(let mut guard = PendingListLock::get().lock());
        !pending_list(guard.as_mut()).is_empty()
    }

    /// Construct a WakeEvent referencing the given wake_vector.
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    /// - `wake_vector` points to a valid, thread-safe `WakeVector` that outlives all calls to
    ///   `initialize` on this `WakeEvent`.
    /// - `Self` is not moved while initialized (between `initialize` and `destroy`).
    /// - `trigger`, `acknowledge`, and `strobe` are only called while `Self` is initialized.
    /// - If `initialize` was called, `destroy` is called before `Self` is dropped.
    pub const unsafe fn new(wake_vector: NonNull<WakeVector>) -> Self {
        Self {
            // SAFETY: Upheld by the caller of `WakeEvent::new`.
            list_nodes: unsafe { ListNodes::new() },
            wake_vector,
            report_info: KCell::new(zx_wake_source_report_entry_t {
                koid: 0,
                name: [0; ZX_MAX_NAME_LEN],
                initial_signal_time: 0,
                last_signal_time: 0,
                last_ack_time: 0,
                signal_count: 0,
                flags: 0,
            }),
        }
    }

    #[inline]
    fn in_global_list(&self, token: &LockToken<'_, GlobalListLock>) -> bool {
        self.list_nodes.in_global_list(token)
    }

    #[inline]
    fn in_pending_list(&self, token: &LockToken<'_, PendingListLock>) -> bool {
        self.list_nodes.in_pending_list(token)
    }

    #[inline]
    fn report_info<'a>(
        &'a self,
        token: &'a LockToken<'_, PendingListLock>,
    ) -> &'a zx_wake_source_report_entry_t {
        // SAFETY: `PendingListLock` is a singleton lock, so `token` proves `PendingListLock` is
        // held.
        unsafe { self.report_info.get(token) }
    }

    #[inline]
    fn report_info_mut<'a>(
        &'a self,
        token: &'a mut LockToken<'_, PendingListLock>,
    ) -> &'a mut zx_wake_source_report_entry_t {
        // SAFETY: `PendingListLock` is a singleton lock, so `token` proves exclusive ownership of
        // `PendingListLock`.
        unsafe { self.report_info.get_mut(token) }
    }

    #[inline]
    fn is_signaled(&self, token: &LockToken<'_, PendingListLock>) -> bool {
        (self.report_info(token).flags & ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED) != 0
    }

    #[inline]
    fn has_been_reported(&self, token: &LockToken<'_, PendingListLock>) -> bool {
        (self.report_info(token).flags & ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_PREVIOUSLY_REPORTED) != 0
    }

    #[inline]
    fn assign_flag(&self, token: &mut LockToken<'_, PendingListLock>, value: bool, flag: u32) {
        let report_info = self.report_info_mut(token);
        if value {
            report_info.flags |= flag;
        } else {
            report_info.flags &= !flag;
        }
    }

    #[inline]
    fn assign_signaled(&self, token: &mut LockToken<'_, PendingListLock>, value: bool) {
        self.assign_flag(token, value, ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED);
    }

    #[inline]
    fn assign_has_been_reported(&self, token: &mut LockToken<'_, PendingListLock>, value: bool) {
        self.assign_flag(token, value, ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_PREVIOUSLY_REPORTED);
    }

    pub fn initialize(self: Pin<&Self>) {
        // Stash our WakeVector constants in our report_info. These will never change over the
        // lifetime of the WakeEvent, so we might as well just stash it all now.
        let mut diagnostic_info = Diagnostics::default();
        // SAFETY: `self.wake_vector` was provided at construction and remains valid whenever
        // `initialize` is called.
        unsafe { self.wake_vector.as_ref() }.get_diagnostics(&mut diagnostic_info);

        ksync::lock!(let mut guard = GlobalListLock::get().lock());
        debug_assert!(!self.in_global_list(guard.token()));

        {
            ksync::lock!(let mut pending_guard = PendingListLock::get().lock());
            debug_assert!(!self.in_pending_list(pending_guard.token()));

            let report_info = self.report_info_mut(pending_guard.as_mut().token_mut());
            *report_info = zx_wake_source_report_entry_t::default();
            report_info.koid = diagnostic_info.koid;
            zr::static_assert!(size_of::<[u8; ZX_MAX_NAME_LEN]>() == ZX_MAX_NAME_LEN);
            report_info.name.copy_from_slice(&diagnostic_info.extra);

            // Initially flag our report entry as having been reported. This sets up
            // our bookkeeping properly so the first time it becomes signaled, it will
            // initialize all of the fields instead of simply bumping the signal count
            // and updating the last signal time.
            self.assign_has_been_reported(pending_guard.as_mut().token_mut(), true);
        }

        global_list(guard.as_mut()).push_back(self);
    }

    pub fn destroy(&self) {
        ksync::lock!(let mut guard = GlobalListLock::get().lock());
        debug_assert!(self.in_global_list(guard.token()));

        {
            // If we are in pending list right now, we need to remove ourselves, making
            // certain to ack at the IdlePowerThread if needed.
            ksync::lock!(let mut pending_guard = PendingListLock::get().lock());

            if self.in_pending_list(pending_guard.token()) {
                if self.is_signaled(pending_guard.token()) {
                    IdlePowerThread::acknowledge_system_wake_event();
                    self.assign_signaled(pending_guard.as_mut().token_mut(), false);
                }
                pending_list(pending_guard.as_mut()).erase(guard.as_mut(), self);
            } else {
                // It should not be possible to have a pending ack, but not be on the
                // pending list.
                debug_assert!(!self.is_signaled(pending_guard.token()));
            }
        }

        global_list(guard.as_mut()).erase(self);
    }

    /// Triggers a wakeup that resumes the system, or aborts an incomplete suspend sequence, and
    /// prevents the system from starting a new suspend sequence.
    ///
    /// Must be called with interrupts and preempt disabled.
    ///
    /// Returns:
    ///  - [`WakeResult::Active`] if this wake trigger occurred when the system was active.
    ///  - [`WakeResult::Resumed`] if this or another wake trigger resumed the system.
    ///  - [`WakeResult::SuspendAborted`] if this wake trigger occurred before suspend completed.
    ///  - [`WakeResult::BadState`] if this wake event is already pending.
    ///
    /// Calls to `trigger` and `acknowledge` must be synchronized by the caller to guarantee that
    /// updates are performed by a single actor at a time.
    pub fn trigger(&self, trigger_time: zx_instant_boot_t) -> WakeResult {
        let _preempt_disabler = AutoPreemptDisabler::new();
        ksync::lock!(let mut pending_guard = PendingListLock::get().lock());
        self.trigger_locked(pending_guard.as_mut(), trigger_time)
    }

    /// Acknowledges a pending wake event, allowing the system to enter suspend when all other
    /// suspend conditions are met.
    ///
    /// Calls to `trigger` and `acknowledge` must be synchronized by the caller to guarantee that
    /// updates are performed by a single actor at a time.
    pub fn acknowledge(&self, ack_behavior: AckBehavior) {
        let _preempt_disabler = AutoPreemptDisabler::new();
        ksync::lock!(let mut pending_guard = PendingListLock::get().lock());
        self.acknowledge_locked(pending_guard.as_mut(), current_boot_time().0, ack_behavior);
    }

    /// WARNING : This is not the method you are looking for `<jedimindtrick/>`
    ///
    /// `strobe` is an operation used only in a very specific situation; when a suspend operation
    /// times out and the `ResumeTimerWakeVector` becomes signaled as a result. This object is
    /// (currently) the only non-interrupt wake source/vector defined in the system, and it is not
    /// directly exposed to user-mode as an object which becomes acknowledged by user-mode actions.
    /// Instead, it is the synthetic wake source used to report suspend-operation timeouts, and is
    /// (logically speaking) *always* immediately acked after being signaled.
    ///
    /// `strobe` handles this operation, without needing to expose any locks to make it possible to
    /// atomically `trigger`/`acknowledge` the object. For all other wake source objects in the
    /// system, explicit calls to `trigger` and `acknowledge` are what should be used.
    pub fn strobe(&self, trigger_time: zx_instant_boot_t) -> WakeResult {
        let _preempt_disabler = AutoPreemptDisabler::new();
        ksync::lock!(let mut pending_guard = PendingListLock::get().lock());
        let result = self.trigger_locked(pending_guard.as_mut(), trigger_time);
        self.acknowledge_locked(pending_guard.as_mut(), trigger_time, AckBehavior::ClearSignaled);
        result
    }

    fn trigger_locked(
        &self,
        mut pending_guard: PendingGuard<'_>,
        trigger_time: zx_instant_boot_t,
    ) -> WakeResult {
        // There are three cases to consider here:
        //
        // 1) We are already signaled. This is an unusual state to be in as we are
        //    being triggered again, even though we have never received an ack.
        //    Regardless we should just bump our signal count, and record a new last
        //    triggered time.
        // 2) We are not signaled, but we have not been reported yet. We must have
        //    been acknowledged previously, and are becoming triggered once
        //    again. Just like case #1, bump the signal count and record a new last
        //    triggered time.
        // 3) We are not signaled, and have been reported (note; this is also the
        //    initial state of a WakeEvent). It is time to re-initialize our report
        //    entry state to record the first trigger event.
        if !self.is_signaled(pending_guard.token()) && self.has_been_reported(pending_guard.token())
        {
            let report_info = self.report_info_mut(pending_guard.as_mut().token_mut());
            report_info.initial_signal_time = trigger_time;
            report_info.last_ack_time = ZX_TIME_INFINITE;
            report_info.signal_count = 1;
            self.assign_has_been_reported(pending_guard.as_mut().token_mut(), false);
        } else {
            let report_info = self.report_info_mut(pending_guard.as_mut().token_mut());
            report_info.signal_count = report_info.signal_count.wrapping_add(1);
        }

        // Regardless of whether this was the first trigger, or a subsequent trigger,
        // record a new last trigger time.
        self.report_info_mut(pending_guard.as_mut().token_mut()).last_signal_time = trigger_time;

        // If we are not in the pending list, add ourself now. Now, we add ourself to
        // the front of the list instead of the back in order to prevent this one wake
        // source from being double-reported in the case that it has been added to a
        // report, and becomes re-triggered before report generation has finished.
        // See the extensive comment on the WakeEvent struct for more details.
        if !self.in_pending_list(pending_guard.token()) {
            pending_list(pending_guard.as_mut()).push_front(self);
        }

        // If we are triggered multiple times without being ack'ed, make sure to
        // report only one trigger to the IdlePowerThread level.
        if !self.is_signaled(pending_guard.token()) {
            self.assign_signaled(pending_guard.as_mut().token_mut(), true);

            // WakeEvent triggering can happen starting from one of two places.
            //
            // 1) In a hard IRQ handler because a physical interrupt wake source was triggered.
            // 2) During a user's call to zx_interrupt_trigger for a virtual interrupt.
            //
            // When we call TriggerSystemWakeEvent, if it needs to wake the boot CPU, it
            // is going to eventually call PreemptSetPending for the boot CPU in order
            // to make sure that it gets scheduled instead of simply returning to the
            // idle thread. This function is going to demand that the per-cpu "blocking
            // disallowed" flag has been set.
            //
            // In the case of #1, this will always be the case. We are in an IRQ
            // handler, and the flag is going to be automatically set/cleared at the
            // start of the handler (you are not allowed to block during an IRQ
            // handler).
            //
            // In the case of #2, this is _effectively_ true, but the per-cpu flag will
            // not have been set. By the time we make it to this point (in
            // trigger_locked) we will have interrupts off, and will be holding the wake
            // event pending list spinlock, both of which are Very Good Reasons to
            // disallow blocking. This said, disabling interrupts and obtaining a
            // spinlock does _not_ automatically set the blocking-disallowed flag. Why?
            // Because when a thread actually blocks, it needs to hold (among other
            // things) its scheduler's spinlock as it re-schedules after joining its
            // WaitQueue.
            //
            // We already have interrupts off, are holding a spinlock, and are not going
            // to call Scheduler::Block here. So go ahead and set the blocking
            // disallowed flag while we call TriggerSystemWakeEvent, restoring the state
            // to its previous value at the end of the sequence.
            debug_assert!(ints_disabled());
            let was_blocking_disallowed = blocking_disallowed();
            set_blocking_disallowed(true);
            let _cleanup = zr::defer(|| {
                set_blocking_disallowed(was_blocking_disallowed);
            });

            return IdlePowerThread::trigger_system_wake_event();
        }

        WakeResult::BadState
    }

    fn acknowledge_locked(
        &self,
        mut pending_guard: PendingGuard<'_>,
        trigger_time: zx_instant_boot_t,
        ack_behavior: AckBehavior,
    ) {
        // In order for an event to be waiting to be acknowledged, it must be in the
        // pending list, and it needs to be signaled (we should not be getting spurious
        // double acks).
        debug_assert!(self.in_pending_list(pending_guard.token()));
        debug_assert!(self.is_signaled(pending_guard.token()));

        // Always record the last ack time.
        self.report_info_mut(pending_guard.as_mut().token_mut()).last_ack_time = trigger_time;

        if ack_behavior == AckBehavior::ClearSignaled {
            // If our AckBehavior tells us to clear our signaled, then ack at the
            // IdlePowerThread level, and clear the signaled flag.
            IdlePowerThread::acknowledge_system_wake_event();
            self.assign_signaled(pending_guard.as_mut().token_mut(), false);
        } else {
            // We have been told that we should remain signaled. Clear our "has been
            // reported" flag. We are in a situation where the interrupt was triggered
            // at least once more after the first time. The first trigger may have been
            // reported, but the second and subsequent triggers have not been reported
            // yet.
            self.assign_has_been_reported(pending_guard.as_mut().token_mut(), false);
        }

        // Do NOT remove this event from the pending list, even if it has been
        // reported already. If we remove the event from the list, we run the risk of
        // invalidating the iterator for a report which is currently being generated.
        // Instead, just leave the event in place, and let report generation handle
        // removing the event from the list when the time comes.
        //
        // See the extensive comment on `WakeEvent` for a more comprehensive
        // explanation of the pending list and the various ways that operations
        // interact with it, and the things they need to do in order to remain safe.
    }

    /// Walks the global list of all instances and dumps diagnostic information to `f`. All events
    /// that are currently pending OR that were triggered after `log_triggered_after_boot_time` are
    /// logged.
    ///
    /// Safe to call concurrently with any and all methods, including ctors and dtors.
    pub fn dump(f: &mut impl core::fmt::Write, log_triggered_after_boot_time: zx_instant_boot_t) {
        ksync::lock!(let mut guard = GlobalListLock::get().lock());
        ksync::lock!(let mut pending_guard = PendingListLock::get().lock());

        for event in global_list(guard.as_mut()).iter() {
            if event.is_signaled(pending_guard.token())
                || event.report_info(pending_guard.token()).last_signal_time
                    >= log_triggered_after_boot_time
            {
                let entry = *event.report_info(pending_guard.token());
                pending_guard.as_mut().call_unlocked(|| {
                    let name_len =
                        entry.name.iter().position(|&b| b == 0).unwrap_or(entry.name.len());
                    let extra = core::str::from_utf8(&entry.name[..name_len]).unwrap_or("");
                    let _ = write!(
                        f,
                        "  koid            : {}\n\
                         \x20 pending         : {}\n\
                         \x20 prev reported   : {}\n\
                         \x20 signal count    : {}\n\
                         \x20 initial trigger : {}\n\
                         \x20 last trigger    : {}\n\
                         \x20 last ack        : {}\n\
                         \x20 extra           : {}\n\n",
                        entry.koid,
                        if (entry.flags & ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_SIGNALED) != 0 {
                            "yes"
                        } else {
                            "no"
                        },
                        if (entry.flags & ZX_SYSTEM_WAKE_REPORT_ENTRY_FLAG_PREVIOUSLY_REPORTED) != 0
                        {
                            "yes"
                        } else {
                            "no"
                        },
                        entry.signal_count,
                        entry.initial_signal_time,
                        entry.last_signal_time,
                        entry.last_ack_time,
                        extra,
                    );
                });
            }
        }
    }

    pub fn generate_wake_event_report(
        suspend_start_time: zx_instant_boot_t,
        out_header: UserOutPtr<zx_wake_source_report_header_t>,
        out_entries: UserOutPtr<zx_wake_source_report_entry_t>,
        num_entries: u32,
        actual_entries: UserOutPtr<u32>,
    ) -> Result<(), Status> {
        // These should have been verified for us already at the syscall layer.
        debug_assert!(!out_header.is_null());

        let do_report_entries = !out_entries.is_null();
        debug_assert_eq!(do_report_entries, num_entries != 0);
        debug_assert_eq!(do_report_entries, !actual_entries.is_null());

        let mut hdr = zx_wake_source_report_header_t::default();
        let mut reported_entries: u32 = 0;
        hdr.suspend_start_time = suspend_start_time;

        // We are going to need to heap allocate a side buffer to copy our results into. We cannot
        // copy our results back out to user mode while holding any of our locks, and we cannot
        // allocate an arbitrary amount of entries on our stack.
        let mut entry_side_buffer = fbl::Array::<zx_wake_source_report_entry_t>::new();
        let mut entry_side_buffer_len: usize = 0;

        {
            // Hold the global list lock for the entire O(n) time it takes to create the
            // report. This prevents any wake sources from being created or destroyed
            // while we iterate the pending list. This guarantees that our total wake
            // source count cannot change while we generate the report, and also means
            // that wake source destruction cannot invalidate our iterator out from
            // under us when we drop the pending lock in order to copy data out to
            // user-mode.
            //
            // See the extensive comment on `WakeEvent` for a more comprehensive
            // explanation of the pending list and the various ways that operations
            // interact with it, and the things they need to do in order to remain safe.
            ksync::lock!(let mut guard = GlobalListLock::get().lock());
            {
                ksync::lock!(let mut pending_guard = PendingListLock::get().lock());

                // Record the number of entries which are waiting to be reported at the
                // start of report generation. We'll adjust this number later on to
                // reflect the actual number of entries which were waiting at the start of
                // the report which didn't actually fit into the user's buffer.
                hdr.unreported_wake_report_entries =
                    limit_size_t_u32(pending_list(pending_guard.as_mut()).len());

                if do_report_entries {
                    let mut cursor = PendingCursor::new(guard.as_mut(), pending_guard.as_mut());
                    entry_side_buffer_len =
                        (num_entries as usize).min(pending_list(pending_guard.as_mut()).len());

                    // Now that we have captured our initial iterator, and computed the
                    // maximum number of entries we might attempt to report, allocate a side
                    // buffer we can copy the information into. We cannot simply copy into
                    // a stack allocated single entry which we then copy to user mode after
                    // dropping the pending lock. We are not allowed to hold _any_ locks
                    // when we copy into user mode, so we will need to be able to buffer
                    // _all_ of our results before starting to do so.
                    //
                    // TODO(johngro): We don't necessarily have to do this every time we
                    // generate a report. We could also keep the allocation around as a
                    // static member of WakeEvent protected by the global lock, and growing
                    // it only when we needed to. At the end of the op (when we drop the
                    // global lock to copy out to user mode), we could move the buffer out,
                    // perform the copy, then lock and move the buffer back in again. We'd
                    // never have a second buffer sitting around unless there was a
                    // concurrent attempt to generate a report, something which is
                    // technically possible but should never happen in practice.
                    let mut alloc_result = Ok(());
                    pending_guard.as_mut().call_unlocked(|| {
                        if entry_side_buffer_len > 0 {
                            if let Ok(buf) =
                                fbl::Array::<zx_wake_source_report_entry_t>::try_new_zeroed_slice(
                                    entry_side_buffer_len,
                                )
                            {
                                entry_side_buffer = buf;
                            } else {
                                alloc_result = Err(Status::NO_MEMORY);
                            }
                        }
                    });

                    // Bail out if we could not allocate a side buffer.
                    alloc_result?;

                    while (reported_entries as usize) < entry_side_buffer_len
                        && let Some(curr) = cursor.next(pending_guard.as_mut())
                    {
                        let mut skip_reporting = false;

                        if !curr.is_signaled(pending_guard.token()) {
                            // The wake source is not currently signaled. If it has been
                            // reported already, then skip adding it to the user's report.
                            // Either way, remove it from the pending list.
                            if curr.has_been_reported(pending_guard.token()) {
                                skip_reporting = true;
                            }
                            cursor.erase_current(pending_guard.as_mut());
                        }

                        if !skip_reporting {
                            // Copy our result into our side buffer and mark it as having been
                            // reported. Then drop our lock briefly allowing pending IRQs
                            // to run, and wake sources to become signaled/acked.
                            entry_side_buffer[reported_entries as usize] =
                                *curr.report_info(pending_guard.token());
                            reported_entries += 1;
                            curr.assign_has_been_reported(pending_guard.as_mut().token_mut(), true);
                            pending_guard.as_mut().call_unlocked(|| {
                                libarch::r#yield();
                            });
                        }

                        // Whether we added the entry to the user's report, or skipped it
                        // because the entry was just waiting around to be cleaned up by us,
                        // we should drop the unreported count.
                        debug_assert!(hdr.unreported_wake_report_entries > 0);
                        hdr.unreported_wake_report_entries -= 1;
                    }
                }
            }

            // Record the total number of wake sources in the system as well as our
            // report time before dropping the global lock.
            hdr.total_wake_sources = limit_size_t_u32(global_list(guard.as_mut()).len());
            hdr.report_time = current_boot_time().0;
        }

        // We're done. Finish up by trying to copy everything back out to user-mode.
        if do_report_entries {
            debug_assert!((reported_entries as usize) <= entry_side_buffer_len);

            out_entries.copy_slice_to_user(&entry_side_buffer[..reported_entries as usize])?;
            actual_entries.copy_to_user(&reported_entries)?;
        }

        out_header.copy_to_user(&hdr)
    }

    pub fn discard_wake_event_report() {
        ksync::lock!(let mut guard = GlobalListLock::get().lock());
        ksync::lock!(let mut pending_guard = PendingListLock::get().lock());

        let mut cursor = PendingCursor::new(guard.as_mut(), pending_guard.as_mut());
        while let Some(curr) = cursor.next(pending_guard.as_mut()) {
            // Remove the event from the pending list iff it has been acknowledged.
            // Make sure the reported flag is set when we remove it from the list.
            if !curr.is_signaled(pending_guard.token()) {
                curr.assign_has_been_reported(pending_guard.as_mut().token_mut(), true);
                cursor.erase_current(pending_guard.as_mut());
            }

            // Drop the lock and libarch::r#yield to give any pending triggers or acks a chance to
            // run.
            pending_guard.as_mut().call_unlocked(|| {
                libarch::r#yield();
            });
        }
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // By the time that we destruct, we should have been Destroyed, meaning that
        // we are no longer in any lists.
        debug_assert!(!self.list_nodes.in_global_list_exclusive());
        debug_assert!(!self.list_nodes.in_pending_list_exclusive());
    }
}
