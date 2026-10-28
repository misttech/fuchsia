// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UTP transfer request submission and completion (UFSHCI 3.0/4.0 section 7.2) on the
//! single-doorbell UTRL (`UTRLDBR`).
//!
//! Two submission models share the slot pool:
//!
//! - Synchronous I/O ([`TransferRequestProcessor::submit_io`]) for block requests: the calling
//!   thread pins the buffer and builds the PRDT, reserves a slot (blocking for pushback when none
//!   is free), writes the UCD/UTRD (DMA descriptors), rings the doorbell and returns. The
//!   completion path is interrupt-driven: the interrupt handler decodes the response, frees the
//!   slot and delivers the outcome through an [`IoCompletion`]. No future, channel or executor is
//!   involved per request. Because nobody awaits these requests, a one-shot timeout task
//!   ([`TransferRequestProcessor::run_io_timeout_task`]) aborts the ones the controller never
//!   finishes within [`COMMAND_TIMEOUT`].
//! - Async UPIUs (`send_scsi_command`, `send_query_request_upiu`, `send_nop_out_upiu`) for
//!   administrative and driver-internal commands, which await the completion interrupt on the
//!   executor, racing it against their own one-shot [`COMMAND_TIMEOUT`] timer.
//!
//! Slot ownership: a submitter owns a slot through a [`SlotGuard`] while it is `Reserved` or
//! `Completed`. While `Scheduled` the controller owns it; if the submitter goes away (or never
//! waits, as for synchronous I/O) the completion path frees it. Once `Timeout`, the completion path
//! frees it after the doorbell bit clears. Client pages are unpinned only through explicit
//! `unsafe` calls at points where the controller is known to be done with them, never from a
//! `Drop` impl.
//!
//! Lock order: `request_list` -> `controller`.

use crate::UfsError;
use crate::constants::{
    ADMIN_COMMAND_SLOT_NUMBER, COMMAND_TIMEOUT, INLINE_DATA_OFFSET, INLINE_DATA_SIZE,
    MAX_PRDT_ENTRIES, MAX_REQUEST_LIST_SIZE, OCS_RETRIES, SCSI_RETRIES, SCSI_RETRY_DELAY,
    SDB_SLOTS, SENSE_DESCRIPTOR_FORMAT_CURRENT, SENSE_DESCRIPTOR_FORMAT_DEFERRED,
    SENSE_RESPONSE_CODE_MASK, SMALL_TRANSFER_PAGES, TIMEOUT_RECLAIM_POLL_INTERVAL,
    TIMEOUT_RECLAIM_WINDOW,
};
use crate::controller::UfsController;
use crate::dma_buffer::ContiguousDmaBuffer;
use crate::query::QueryUpiu;
use crate::registers::{
    InterruptStatusReg, UtrListBaseAddressReg, UtrListBaseAddressUpperReg, UtrListClearReg,
    UtrListDoorBellReg, UtrListRunStopReg,
};
use crate::request_list::{
    self, DataDirection, OverallCommandStatus, RequestList, SlotCompletion, SlotState,
    TransferRequestDescriptor,
};
use crate::scsi::{ScsiStatus, SenseKey};
use crate::transfer::{self, CommandDescriptorLayout, PrdtEntry};
use crate::upiu::{
    CommandUpiu, NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode,
};
use block_server::RequestId;
use fuchsia_sync::{Condvar, Mutex, MutexGuard};
use futures::FutureExt;
use futures::channel::oneshot;
use futures::task::AtomicWaker;
use log::{error, info, warn};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

/// Bitmask of all slots in the request list (`0..SDB_SLOTS`).
const ALL_SLOTS_MASK: u64 = request_list::slot_mask(SDB_SLOTS as u8);
/// Bitmask of usable I/O slots (`0..ADMIN_COMMAND_SLOT_NUMBER`).
const IO_SLOT_MASK: u64 = request_list::slot_mask(ADMIN_COMMAND_SLOT_NUMBER);

/// Client data buffer for one transfer.
#[derive(Debug, Clone, Copy)]
pub struct DataTransfer<'a> {
    vmo: &'a zx::Vmo,
    offset: u64,
    length: u32,
    direction: DataDirection,
}

impl<'a> DataTransfer<'a> {
    /// `length` bytes read from the device into `vmo` at `offset`.
    pub fn read(vmo: &'a zx::Vmo, offset: u64, length: u32) -> Self {
        Self { vmo, offset, length, direction: DataDirection::DeviceToHost }
    }

    /// `length` bytes written to the device from `vmo` at `offset`.
    pub fn write(vmo: &'a zx::Vmo, offset: u64, length: u32) -> Self {
        Self { vmo, offset, length, direction: DataDirection::HostToDevice }
    }

    /// Returns the page-aligned `(offset, length)` covering the transfer.
    fn pin_range(&self, page_size: u64) -> Result<(u64, u64), UfsError> {
        let start = self.offset & !(page_size - 1);
        let end = self
            .offset
            .checked_add(self.length as u64)
            .and_then(|end| end.checked_next_multiple_of(page_size))
            .ok_or(UfsError::InvalidParameter)?;
        Ok((start, end - start))
    }

    fn pin_options(&self) -> zx::BtiOptions {
        match self.direction {
            DataDirection::HostToDevice => zx::BtiOptions::PERM_READ,
            _ => zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE,
        }
    }

    /// Cleans and invalidates the cache after a `DeviceToHost` DMA read so that the next CPU read
    /// picks up data written to main memory by the controller (discarding any speculative CPU
    /// prefetches during DMA).
    fn invalidate_read_cache(&self) -> Result<(), zx::Status> {
        self.vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, self.offset, self.length as u64)
    }
}

/// Backing storage for an [`IoTransfer`].
#[derive(Clone, Debug)]
enum IoBuffer<'a> {
    /// Shared VMO (survives until completion for read cache invalidation).
    Vmo(Arc<zx::Vmo>),
    /// Small `HostToDevice` payload copied into the slot's UCD inline data tail on submission.
    Inline(&'a [u8]),
}

/// Client data buffer of a synchronous I/O request ([`TransferRequestProcessor::submit_io`]).
/// Unlike [`DataTransfer`], a VMO transfer holds an `Arc<zx::Vmo>` so it can be kept in
/// [`SlotCompletion::Io`] for post-DMA read cache invalidation. Small write payloads up to
/// [`INLINE_DATA_SIZE`] bytes can instead be passed via [`Self::inline_write`] and are copied
/// into the slot's pre-pinned UCD buffer without pinning a separate VMO.
#[derive(Clone, Debug)]
pub struct IoTransfer<'a> {
    buffer: IoBuffer<'a>,
    offset: u64,
    length: u32,
    direction: DataDirection,
}

impl IoTransfer<'static> {
    /// `length` bytes read from the device into `vmo` at `offset`.
    pub fn read(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self { buffer: IoBuffer::Vmo(vmo), offset, length, direction: DataDirection::DeviceToHost }
    }

    /// `length` bytes written to the device from `vmo` at `offset`.
    pub fn write(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self { buffer: IoBuffer::Vmo(vmo), offset, length, direction: DataDirection::HostToDevice }
    }
}

impl<'a> IoTransfer<'a> {
    /// Small `HostToDevice` payload (`<= INLINE_DATA_SIZE`, dword-aligned length) copied into the
    /// slot's UCD inline data region on submission without pinning a VMO.
    pub fn inline_write(bytes: &'a [u8]) -> Self {
        Self {
            buffer: IoBuffer::Inline(bytes),
            offset: 0,
            length: bytes.len() as u32,
            direction: DataDirection::HostToDevice,
        }
    }

    fn as_data_transfer(&self) -> Option<DataTransfer<'_>> {
        match &self.buffer {
            IoBuffer::Vmo(vmo) => Some(DataTransfer {
                vmo,
                offset: self.offset,
                length: self.length,
                direction: self.direction,
            }),
            IoBuffer::Inline(_) => None,
        }
    }

    fn inline_bytes(&self) -> Option<&'a [u8]> {
        match self.buffer {
            IoBuffer::Inline(bytes) => Some(bytes),
            IoBuffer::Vmo(_) => None,
        }
    }
}

/// How the outcome of a synchronous I/O request ([`TransferRequestProcessor::submit_io`]) is
/// delivered.
pub enum IoCompletion {
    /// A block server request, reported to the [`IoCompleter`] installed with
    /// [`TransferRequestProcessor::set_io_completer`]. Carries no per-request allocation.
    Request(RequestId),
    /// Reported by invoking the callback.
    #[cfg(test)]
    Callback(Box<dyn FnOnce(Result<(), zx::Status>) + Send>),
}

impl std::fmt::Debug for IoCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(id) => f.debug_tuple("Request").field(id).finish(),
            #[cfg(test)]
            Self::Callback(_) => f.write_str("Callback"),
        }
    }
}

/// Receives the outcomes of [`IoCompletion::Request`] submissions.
pub trait IoCompleter: Send + Sync {
    /// Reports the outcome of `request_id`, which was submitted for `lun`.
    fn complete(&self, lun: u8, request_id: RequestId, status: Result<(), zx::Status>);
}

/// Outcome of a synchronous I/O request, delivered after `request_list` is released.
struct IoOutcome {
    lun: u8,
    completion: IoCompletion,
    read_back: Option<IoTransfer<'static>>,
    status: Result<(), zx::Status>,
}

/// Work a slot leaves for after `request_list` is released: unpinning its client buffer and
/// delivering its outcome.
struct DeferredSlot {
    /// `None` while the controller may still access the buffer (a timed-out slot keeps its PMT
    /// until the doorbell bit clears).
    pmt: Option<zx::Pmt>,
    outcome: Option<IoOutcome>,
}

/// Deferred work collected during one completion pass. Inline storage covers every slot up to
/// [`MAX_REQUEST_LIST_SIZE`] so the completion path never allocates.
///
/// Unpinning requires the explicit `unsafe` [`TransferRequestProcessor::run_deferred`], whose
/// caller vouches that the controller is done with the buffers. If a `Deferred` is dropped
/// without running, its outcomes are still delivered (so clients are not stuck) but the PMTs are
/// leaked, since nothing has established that the pages are safe to unpin.
struct Deferred<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    inline: [Option<DeferredSlot>; MAX_REQUEST_LIST_SIZE as usize],
    len: usize,
}

impl<'a, M: Mmio> Deferred<'a, M> {
    fn new(processor: &'a TransferRequestProcessor<M>) -> Self {
        Self { processor, inline: [const { None }; MAX_REQUEST_LIST_SIZE as usize], len: 0 }
    }

    fn push(&mut self, pmt: Option<zx::Pmt>, outcome: Option<IoOutcome>) {
        if pmt.is_none() && outcome.is_none() {
            return;
        }
        self.inline[self.len] = Some(DeferredSlot { pmt, outcome });
        self.len += 1;
    }

    fn drain(&mut self) -> impl Iterator<Item = DeferredSlot> + '_ {
        let len = self.len;
        self.len = 0;
        self.inline[..len].iter_mut().filter_map(Option::take)
    }
}

impl<M: Mmio> Drop for Deferred<'_, M> {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        warn!("Deferred completions dropped without running; pinned buffers will be leaked");
        let processor = self.processor;
        for DeferredSlot { pmt, outcome } in self.drain() {
            // Dropping the `zx::Pmt` handle without unpinning leaves its pages quarantined by
            // the kernel, which is the safe failure mode.
            drop(pmt);
            if let Some(outcome) = outcome {
                processor.deliver_io_outcome(outcome);
            }
        }
    }
}

/// Unpins a PMT once the controller can no longer access its pages.
///
/// # Safety
///
/// The controller must no longer access the pinned pages: their doorbell bit reads 0 after
/// scheduling, the request was never submitted, or `HCE` reads 0.
unsafe fn unpin_pmt(pmt: Option<zx::Pmt>) {
    if let Some(pmt) = pmt {
        // SAFETY: Guaranteed by the caller.
        if let Err(status) = unsafe { pmt.unpin() } {
            error!(status:%; "Failed to unpin transfer buffer");
        }
    }
}

/// Converts a UCD byte offset or length into the dword count stored in a UTRD field.
///
/// # Panics
///
/// Panics if `bytes` is not a multiple of 4 or does not fit a 16-bit dword count. Callers pass
/// [`CommandDescriptorLayout`] values, which are bounded at compile time.
fn dwords(bytes: usize) -> u16 {
    assert_eq!(bytes % 4, 0, "UCD offset or length {bytes} is not dword aligned");
    u16::try_from(bytes / 4).expect("UCD offset or length exceeds a 16-bit dword count")
}

/// Owns a `Reserved` or `Completed` slot and returns it on drop, making async request futures
/// cancellation-safe at every `.await` point:
///
/// - Dropped while `Reserved` (before [`TransferRequestProcessor::submit`] rings the doorbell): no
///   PMT is stored in the slot yet; `Drop` frees the slot and wakes waiting reservers.
/// - Dropped while `Scheduled` (inside `wait_for_slot`): hardware DMA is still active, so `Drop`
///   clears `completion = SlotCompletion::None` and leaves the slot `Scheduled` with its `pmt`
///   intact until hardware clears the doorbell (or [`TransferRequestProcessor::quiesce`] disables
///   `HCE`), at which point `finish_slot_locked` frees the slot and unpins the `pmt` via
///   [`Deferred`].
/// - Dropped while `Completed` (after ISR, before `execute_with_reader` takes the `pmt`): `Drop`
///   frees the slot and drops the `zx::Pmt` handle without calling `unpin()`, relying on Zircon's
///   BTI quarantine rather than unpinning in `Drop`.
/// - If the driver shuts down ([`TransferRequestProcessor::quiesce`]), `HCE` is disabled (`0`)
///   before slots are aborted and PMTs are unpinned.
pub(crate) struct SlotGuard<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    slot: u8,
    /// Cleared when ownership passes to the completion path (timeout or synchronous I/O) or when
    /// `execute` releases the slot directly.
    armed: bool,
}

impl<M: Mmio> SlotGuard<'_, M> {
    /// Returns the slot index.
    pub(crate) fn slot(&self) -> u8 {
        self.slot
    }
}

impl<M: Mmio> Drop for SlotGuard<'_, M> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = self.slot;
        let mut list = self.processor.request_list.lock();
        let Ok(request_slot) = list.slot_mut(slot) else {
            return;
        };
        match request_slot.state {
            // Pinning happens before reservation and the PMT moves into the slot only when the
            // doorbell is rung, so a `Reserved` slot never holds one.
            SlotState::Reserved => {
                debug_assert!(request_slot.pmt.is_none(), "Reserved slot {slot} holds a PMT");
                self.processor.take_and_release_slot(&mut list, slot);
            }
            // The async submitter was dropped before it resumed. Only release the slot index:
            // nothing here has established that the pages are safe to unpin, so a PMT still
            // present is leaked (the kernel quarantines its pages) rather than unpinned.
            SlotState::Completed => {
                if let Some(pmt) = self.processor.take_and_release_slot(&mut list, slot) {
                    warn!(slot; "Slot released without unpinning; leaking pinned pages");
                    drop(pmt);
                }
            }
            // The completion path frees the slot once the controller is done with it.
            SlotState::Scheduled => request_slot.completion = SlotCompletion::None,
            SlotState::Timeout | SlotState::Free => {}
        }
    }
}

/// Holds the admin lock and the admin slot.
struct AdminSlotGuard<'a, M: Mmio> {
    // Declared first so the slot is released before the lock.
    slot: SlotGuard<'a, M>,
    _lock: futures::lock::MutexGuard<'a, ()>,
}

/// Transfer buffer source passed into [`TransferRequestProcessor::with_prepared_transfer`].
enum TransferSource<'a> {
    None,
    Borrowed(DataTransfer<'a>),
    Owned(IoTransfer<'a>),
}

/// Pinned or inline transfer state prepared before acquiring `request_list`.
struct PreparedTransfer<'p, 'a> {
    pmt: Option<zx::Pmt>,
    prdt: &'p [PrdtEntry],
    inline_data: Option<&'a [u8]>,
    direction: DataDirection,
}

/// Submits UTP transfer requests and processes their completions.
pub struct TransferRequestProcessor<M: Mmio> {
    /// BTI used to pin client data buffers.
    pub(crate) bti: zx::Bti,
    controller: Arc<UfsController<M>>,
    pub(crate) request_list: Mutex<RequestList>,
    /// System page size cached at creation time.
    page_size: u64,
    /// Serializes Query/NOP requests on the admin slot.
    admin_lock: futures::lock::Mutex<()>,
    /// Signalled (with `request_list` held) when an I/O slot is released or the processor is
    /// stopped, waking threads blocked in [`Self::reserve_io_slot_locked`].
    slot_available: Condvar,
    /// Sink for [`IoCompletion::Request`] outcomes.
    io_completer: OnceLock<Arc<dyn IoCompleter>>,
    /// Wakes [`Self::run_io_timeout_task`] when the first synchronous I/O request is scheduled.
    timeout_waker: AtomicWaker,
    /// Wakes [`Self::wait_for_link_recovery`] when link recovery is requested or the processor is
    /// stopped.
    recovery_waker: AtomicWaker,
}

impl<M: Mmio> TransferRequestProcessor<M> {
    /// Creates a processor with [`SDB_SLOTS`] UCD slots; [`ADMIN_COMMAND_SLOT_NUMBER`] is reserved
    /// as the admin slot.
    ///
    /// # Errors
    ///
    /// Returns the DMA allocation or handle duplication error.
    pub fn new(bti: &zx::Bti, controller: Arc<UfsController<M>>) -> Result<Self, zx::Status> {
        let request_list = RequestList::new(bti, SDB_SLOTS as u8)?;

        Ok(Self {
            bti: bti.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
            controller,
            request_list: Mutex::new(request_list),
            page_size: zx::system_get_page_size() as u64,
            admin_lock: futures::lock::Mutex::new(()),
            slot_available: Condvar::new(),
            io_completer: OnceLock::new(),
            timeout_waker: AtomicWaker::new(),
            recovery_waker: AtomicWaker::new(),
        })
    }

    /// Returns the shared controller.
    pub fn controller(&self) -> &Arc<UfsController<M>> {
        &self.controller
    }

    /// Marks the controller as needing link recovery and wakes any task awaiting
    /// [`Self::wait_for_link_recovery`].
    pub fn request_link_recovery(&self) {
        self.controller.request_link_recovery();
        self.recovery_waker.wake();
    }

    /// Waits until link recovery is requested (returning `true`) or the processor is stopped by
    /// [`Self::quiesce`] (returning `false`).
    pub async fn wait_for_link_recovery(&self) -> bool {
        std::future::poll_fn(|cx| {
            self.recovery_waker.register(cx.waker());
            if self.is_stopped() {
                return Poll::Ready(false);
            }
            if self.controller.take_recovery_needed() {
                return Poll::Ready(true);
            }
            Poll::Pending
        })
        .await
    }

    /// Returns whether the processor has been stopped by [`Self::quiesce`].
    pub fn is_stopped(&self) -> bool {
        self.request_list.lock().stopped
    }

    /// Installs the sink for [`IoCompletion::Request`] outcomes. Must be called before the first
    /// such submission; later calls are ignored.
    pub fn set_io_completer(&self, completer: Arc<dyn IoCompleter>) {
        if self.io_completer.set(completer).is_err() {
            warn!("I/O completer already installed; ignoring replacement");
        }
    }

    /// Programs the UTRL base address and sets `UTRLRSR`.
    ///
    /// # Errors
    ///
    /// Returns `IoError` after [`Self::quiesce`].
    pub fn init(&self) -> Result<(), UfsError> {
        let list = self.request_list.lock();
        if list.stopped {
            return Err(UfsError::IoError);
        }
        let paddr = list.io_buffer.phys_address();
        self.controller.with_mmio(|mmio| {
            let mut addr_low = UtrListBaseAddressReg::from_raw(0);
            addr_low.set_address(paddr as u32);
            addr_low.write(mmio);

            let mut addr_high = UtrListBaseAddressUpperReg::from_raw(0);
            addr_high.set_address_upper((paddr >> 32) as u32);
            addr_high.write(mmio);

            let mut run_stop = UtrListRunStopReg::from_raw(0);
            run_stop.set_value(true);
            run_stop.write(mmio);
        });
        Ok(())
    }

    /// Wakes every blocked and async I/O slot reserver so it re-examines the slot pool (or
    /// `stopped`).
    fn wake_all_slot_waiters(&self, list: &mut RequestList) {
        if list.blocked_reservers > 0 {
            self.slot_available.notify_all();
        }
        for waker in list.slot_wakers.drain(..) {
            waker.wake();
        }
    }

    /// Returns `slot` to `Free` without waking waiters and returns its pinned [`zx::Pmt`] so the
    /// caller can unpin it outside `request_list.lock()`.
    fn release_slot_locked(&self, list: &mut RequestList, slot: u8) -> Option<zx::Pmt> {
        let request_slot = list.slot_mut(slot).ok()?;
        let sync_io = request_slot.sync_io;
        let pmt = request_slot.release();
        if sync_io {
            list.inflight_io -= 1;
        }
        list.set_slot_state(slot, SlotState::Free);
        pmt
    }

    /// Returns `slot` to `Free`, wakes waiting I/O slot reservers, and returns its pinned
    /// [`zx::Pmt`] so the caller can unpin it outside `request_list.lock()`.
    fn take_and_release_slot(&self, list: &mut RequestList, slot: u8) -> Option<zx::Pmt> {
        let pmt = self.release_slot_locked(list, slot);
        if slot != ADMIN_COMMAND_SLOT_NUMBER {
            self.wake_all_slot_waiters(list);
        }
        pmt
    }

    /// Marks the lowest `Free` I/O slot `Reserved` and returns it, if any.
    fn take_free_io_slot_locked(&self, list: &mut RequestList) -> Option<u8> {
        let free_io = list.free_mask & IO_SLOT_MASK;
        if free_io == 0 {
            return None;
        }
        let slot = free_io.trailing_zeros() as u8;
        list.set_slot_state(slot, SlotState::Reserved);
        Some(slot)
    }

    /// Reserves an I/O slot for a synchronous request while holding `request_list`, blocking the
    /// calling thread on [`Self::slot_available`] while all are busy (pushback) until a slot is
    /// released by the completion path or the processor is stopped.
    ///
    /// # Errors
    ///
    /// Returns `IoError` after [`Self::quiesce`].
    fn reserve_io_slot_locked(
        &self,
        list: &mut MutexGuard<'_, RequestList>,
    ) -> Result<u8, UfsError> {
        loop {
            if list.stopped {
                return Err(UfsError::IoError);
            }
            if let Some(slot) = self.take_free_io_slot_locked(list) {
                return Ok(slot);
            }
            list.blocked_reservers += 1;
            self.slot_available.wait(list);
            list.blocked_reservers -= 1;
        }
    }

    /// Reserves an I/O slot for an async request, waiting until one is released by the completion
    /// path.
    ///
    /// # Errors
    ///
    /// Returns `IoError` after [`Self::quiesce`].
    pub(crate) async fn reserve_io_slot(&self) -> Result<SlotGuard<'_, M>, UfsError> {
        std::future::poll_fn(|cx| {
            let mut list = self.request_list.lock();
            if list.stopped {
                return Poll::Ready(Err(UfsError::IoError));
            }
            if let Some(slot) = self.take_free_io_slot_locked(&mut list) {
                return Poll::Ready(Ok(SlotGuard { processor: self, slot, armed: true }));
            }
            if !list.slot_wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                list.slot_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }

    /// Takes the admin lock and reserves the admin slot. If the previous admin request timed out
    /// or was cancelled, the slot is still owned by the controller until its doorbell bit clears;
    /// since `UTRLCLR` raises no interrupt, the bit is polled every
    /// [`TIMEOUT_RECLAIM_POLL_INTERVAL`] until the slot is reclaimed.
    async fn reserve_admin_slot(&self) -> Result<AdminSlotGuard<'_, M>, UfsError> {
        let lock = self.admin_lock.lock().await;
        let slot = ADMIN_COMMAND_SLOT_NUMBER;
        let deadline = zx::MonotonicInstant::after(COMMAND_TIMEOUT);
        loop {
            {
                let mut list = self.request_list.lock();
                if list.stopped {
                    return Err(UfsError::IoError);
                }
                if list.slot(slot)?.state == SlotState::Free {
                    list.set_slot_state(slot, SlotState::Reserved);
                    break;
                }
            }
            self.process_slot_completion(slot);
            if zx::MonotonicInstant::get() >= deadline {
                return Err(UfsError::NoResources);
            }
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL))
                .await;
        }
        Ok(AdminSlotGuard { slot: SlotGuard { processor: self, slot, armed: true }, _lock: lock })
    }

    /// Validates and pins `source` (if a VMO transfer) and invokes `f` with the prepared PRDT
    /// slice and PMT. Any PMT still left in `PreparedTransfer::pmt` when `f` returns (because `f`
    /// failed before storing it in a slot) is unpinned after `f` returns.
    fn with_prepared_transfer<'a, R>(
        &self,
        source: TransferSource<'a>,
        f: impl FnOnce(&mut PreparedTransfer<'_, 'a>) -> Result<R, UfsError>,
    ) -> Result<R, UfsError> {
        let page_size = self.page_size;
        let data_transfer = match &source {
            TransferSource::Borrowed(data) => Some(*data),
            TransferSource::Owned(io) => io.as_data_transfer(),
            TransferSource::None => None,
        };
        let inline_data = match &source {
            TransferSource::Owned(io) => io.inline_bytes(),
            _ => None,
        };
        if let Some(bytes) = inline_data {
            if bytes.is_empty() || bytes.len() > INLINE_DATA_SIZE || bytes.len() % 4 != 0 {
                return Err(UfsError::InvalidParameter);
            }
        }
        let pin_range = data_transfer.map(|data| data.pin_range(page_size)).transpose()?;
        let page_count = pin_range.map_or(0, |(_, length)| (length / page_size) as usize);
        if page_count > MAX_PRDT_ENTRIES {
            return Err(UfsError::InvalidParameter);
        }

        // Only zero-initialize scratch PRDT arrays for the transfer size class actually used.
        let mut small_prdt: [PrdtEntry; SMALL_TRANSFER_PAGES];
        let mut large_prdt: [PrdtEntry; MAX_PRDT_ENTRIES];
        let prdt_table: &mut [PrdtEntry] = match page_count {
            0 => &mut [],
            1..=SMALL_TRANSFER_PAGES => {
                small_prdt = [PrdtEntry::default(); SMALL_TRANSFER_PAGES];
                &mut small_prdt[..page_count]
            }
            _ => {
                large_prdt = [PrdtEntry::default(); MAX_PRDT_ENTRIES];
                &mut large_prdt[..page_count]
            }
        };

        let mut pmt = None;
        let mut prdt_count = 0;
        let mut direction = DataDirection::NoData;
        if let (Some(data), Some((offset, length))) = (data_transfer, pin_range) {
            let mut small_phys: [usize; SMALL_TRANSFER_PAGES];
            let mut large_phys: [usize; MAX_PRDT_ENTRIES];
            let phys: &mut [usize] = match page_count {
                0 => &mut [],
                1..=SMALL_TRANSFER_PAGES => {
                    small_phys = [0usize; SMALL_TRANSFER_PAGES];
                    &mut small_phys[..page_count]
                }
                _ => {
                    large_phys = [0usize; MAX_PRDT_ENTRIES];
                    &mut large_phys[..page_count]
                }
            };
            let pinned =
                self.bti.pin(data.pin_options(), data.vmo, offset, length, phys).map_err(|s| {
                    error!(status:% = s; "Failed to pin transfer buffer");
                    UfsError::IoError
                })?;
            match transfer::fill_prdt(prdt_table, phys, data.offset, data.length) {
                Ok(count) => prdt_count = count,
                Err(e) => {
                    // SAFETY: The pinned addresses were never written into a descriptor.
                    unsafe { unpin_pmt(Some(pinned)) };
                    return Err(e);
                }
            }
            let cache_op = match data.direction {
                DataDirection::DeviceToHost => zx::VmoOp::CACHE_CLEAN_INVALIDATE,
                DataDirection::HostToDevice => zx::VmoOp::CACHE_CLEAN,
                DataDirection::NoData => unreachable!(),
            };
            if let Err(s) = data.vmo.op_range(cache_op, data.offset, data.length as u64) {
                error!(status:% = s; "Failed to clean/invalidate transfer buffer cache");
                // SAFETY: The pinned addresses were never written into a descriptor.
                unsafe { unpin_pmt(Some(pinned)) };
                return Err(UfsError::IoError);
            }
            pmt = Some(pinned);
            direction = data.direction;
        } else if inline_data.is_some() {
            direction = DataDirection::HostToDevice;
        }

        let mut prepared =
            PreparedTransfer { pmt, prdt: &prdt_table[..prdt_count], inline_data, direction };
        let result = f(&mut prepared);
        // SAFETY: Any PMT still in `prepared.pmt` was not stored in a scheduled slot.
        unsafe { unpin_pmt(prepared.pmt.take()) };
        result
    }

    /// Writes the UCD and UTRD for `Reserved` `slot` and rings its doorbell while holding
    /// `request_list`. Moves `prepared.pmt` and `completion` into the slot only after the
    /// descriptors are written and the doorbell is rung.
    fn schedule_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        layout: CommandDescriptorLayout,
        request: &[u8],
        prepared: &mut PreparedTransfer<'_, '_>,
        completion: &mut SlotCompletion,
    ) -> Result<(), UfsError> {
        if request.len() != layout.request_length() {
            return Err(UfsError::InvalidParameter);
        }
        let RequestList { io_buffer, request_slots, stopped, inflight_io, .. } = &mut *list;
        let request_slot = match request_slots.get_mut(slot as usize) {
            Some(s) if !*stopped && s.state == SlotState::Reserved => s,
            _ => {
                return Err(if *stopped {
                    UfsError::IoError
                } else {
                    UfsError::InvalidSlot(slot as u32)
                });
            }
        };

        let io_error = |status: zx::Status| {
            error!(slot, status:%; "Failed to write transfer descriptors");
            UfsError::IoError
        };
        let ucd = &mut request_slot.command_descriptor_io;
        ucd.write(0, request).map_err(io_error)?;
        // Zero the response UPIU header (padded to 8-byte alignment so `zero` uses 64-bit stores)
        // so a failed request cannot return a stale response.
        ucd.zero(layout.response_offset(), (std::mem::size_of::<UpiuHeader>() + 7) & !7)
            .map_err(io_error)?;
        let ucd_address = ucd.phys_address();
        let prdt_len = if let Some(inline_bytes) = prepared.inline_data {
            ucd.write(INLINE_DATA_OFFSET, inline_bytes).map_err(io_error)?;
            let mut inline_prdt = PrdtEntry::default();
            inline_prdt.set_dma_address(
                ucd_address + INLINE_DATA_OFFSET as u64,
                inline_bytes.len() as u32,
            )?;
            ucd.write_struct(layout.prdt_offset(), &inline_prdt).map_err(io_error)?;
            1u16
        } else {
            if !prepared.prdt.is_empty() {
                ucd.write(layout.prdt_offset(), prepared.prdt.as_bytes()).map_err(io_error)?;
            }
            prepared.prdt.len() as u16
        };

        let mut descriptor = TransferRequestDescriptor::default();
        descriptor.set_command_type(TransferRequestDescriptor::COMMAND_TYPE_UFS_STORAGE);
        descriptor.set_data_direction(prepared.direction);
        descriptor.set_interrupt(true);
        descriptor.set_overall_command_status(OverallCommandStatus::Invalid);
        descriptor.command_descriptor_base_addr = ucd_address as u32;
        descriptor.command_descriptor_base_addr_upper = (ucd_address >> 32) as u32;
        descriptor.set_response_upiu_offset_and_length(
            dwords(layout.response_offset()),
            dwords(layout.response_length()),
        );
        descriptor.set_prdt_offset_and_length(
            if prdt_len == 0 { 0 } else { dwords(layout.prdt_offset()) },
            prdt_len,
        );

        io_buffer
            .write_struct(
                slot as usize * std::mem::size_of::<TransferRequestDescriptor>(),
                &descriptor,
            )
            .map_err(io_error)?;
        self.controller.with_mmio(|mmio| {
            // Order the descriptor stores before the doorbell store.
            mmio.write_barrier();
            let mut doorbell = UtrListDoorBellReg::from_raw(0);
            doorbell.set_door_bell(1 << slot);
            doorbell.write(mmio);
        });

        // Completions are processed under `request_list`, which is still held.
        request_slot.pmt = prepared.pmt.take();
        request_slot.result = Ok(());
        request_slot.completion = std::mem::take(completion);
        request_slot.deadline = Some(zx::MonotonicInstant::after(COMMAND_TIMEOUT));
        if matches!(request_slot.completion, SlotCompletion::Io { .. }) {
            request_slot.sync_io = true;
            *inflight_io += 1;
            if *inflight_io == 1 {
                // Nobody waits on this request; arm the timeout task that bounds its lifetime.
                self.timeout_waker.wake();
            }
        }
        list.set_slot_state(slot, SlotState::Scheduled);
        Ok(())
    }

    /// Pins `data`, fills the UCD and UTRD for `slot` and rings its doorbell. `completion` is
    /// moved into the slot only once the doorbell has been rung, so on error the caller still
    /// owns it.
    fn submit(
        &self,
        slot: u8,
        layout: CommandDescriptorLayout,
        request: &[u8],
        data: Option<&DataTransfer<'_>>,
        completion: &mut SlotCompletion,
    ) -> Result<(), UfsError> {
        if request.len() != layout.request_length() {
            return Err(UfsError::InvalidParameter);
        }
        let source = data.copied().map_or(TransferSource::None, TransferSource::Borrowed);
        self.with_prepared_transfer(source, |prepared| {
            let mut list = self.request_list.lock();
            self.schedule_slot_locked(&mut list, slot, layout, request, prepared, completion)
        })
    }

    /// Submits a SCSI command to `lun` synchronously and returns as soon as its doorbell has been
    /// rung. Blocks the calling thread while all I/O slots are busy (pushback). The outcome is
    /// delivered through `completion` from the completion path once the controller finishes,
    /// aborts (`TIMED_OUT`) or is quiesced (`IO`); transient SCSI statuses are reported, not
    /// retried.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` for a misaligned or oversized buffer, or `IoError` on a pin
    /// failure or after [`Self::quiesce`]. On error `completion` has already been delivered with
    /// the matching `zx::Status`.
    pub fn submit_io(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<IoTransfer<'_>>,
        completion: IoCompletion,
    ) -> Result<(), UfsError> {
        let data = data.filter(|data| data.length != 0);
        let expected_transfer_len = data.as_ref().map_or(0, |data| data.length);
        let direction = data.as_ref().map_or(DataDirection::NoData, |data| data.direction);
        let mut read_back = match &data {
            Some(IoTransfer {
                buffer: IoBuffer::Vmo(vmo),
                offset,
                length,
                direction: DataDirection::DeviceToHost,
            }) => Some(IoTransfer::read(Arc::clone(vmo), *offset, *length)),
            _ => None,
        };
        let source = data.map_or(TransferSource::None, TransferSource::Owned);
        let mut completion = Some(completion);

        let result = self.with_prepared_transfer(source, |prepared| {
            let mut list = self.request_list.lock();
            let slot = self.reserve_io_slot_locked(&mut list)?;
            let mut command = CommandUpiu::new(cdb, expected_transfer_len, direction);
            command.header.lun = lun;
            command.header.task_tag = slot;
            let mut slot_completion = SlotCompletion::Io {
                lun,
                completion: completion.take().unwrap(),
                read_back: read_back.take(),
            };
            if let Err(e) = self.schedule_slot_locked(
                &mut list,
                slot,
                CommandDescriptorLayout::SCSI_COMMAND,
                command.as_bytes(),
                prepared,
                &mut slot_completion,
            ) {
                if let SlotCompletion::Io { completion: c, .. } = slot_completion {
                    completion = Some(c);
                }
                let pmt = self.take_and_release_slot(&mut list, slot);
                drop(list);
                // SAFETY: The slot's doorbell was never rung.
                unsafe { unpin_pmt(pmt) };
                return Err(e);
            }
            Ok(())
        });
        if let Err(e) = result {
            if let Some(completion) = completion {
                self.complete_io(lun, completion, Err(zx::Status::from(e)));
            }
        }
        result
    }

    /// Delivers the outcome of a synchronous I/O request submitted for `lun`.
    pub fn complete_io(&self, lun: u8, completion: IoCompletion, status: Result<(), zx::Status>) {
        match completion {
            IoCompletion::Request(request_id) => match self.io_completer.get() {
                Some(completer) => completer.complete(lun, request_id, status),
                None => error!(lun, request_id:?; "No I/O completer installed; dropping request"),
            },
            #[cfg(test)]
            IoCompletion::Callback(callback) => callback(status),
        }
    }

    /// Invalidates the read buffer of a `DeviceToHost` outcome (even on error, in case of partial
    /// DMA or speculative CPU prefetches) so the client sees main memory, failing a successful
    /// outcome if cache invalidation fails.
    fn invalidate_read_back(&self, mut outcome: IoOutcome) -> IoOutcome {
        if let Some(read_back) = &outcome.read_back {
            if let Some(transfer) = read_back.as_data_transfer() {
                if let Err(err) = transfer.invalidate_read_cache() {
                    warn!(err:%; "Failed to invalidate transfer buffer cache after read");
                    if outcome.status.is_ok() {
                        outcome.status = Err(err);
                    }
                }
            }
        }
        outcome
    }

    /// Delivers a synchronous I/O outcome without unpinning anything: invalidates its read
    /// buffer (if any) and reports its status.
    fn deliver_io_outcome(&self, outcome: IoOutcome) {
        let outcome = self.invalidate_read_back(outcome);
        self.complete_io(outcome.lun, outcome.completion, outcome.status);
    }

    /// Runs deferred slot work: invalidates read buffers, unpins client buffers and delivers
    /// synchronous I/O outcomes, in that order (as `cqhci::PendingTask::complete` does). Must run
    /// without `request_list` held. This is the only path that unpins completed slots.
    ///
    /// # Safety
    ///
    /// As for [`unpin_pmt`]: the controller must no longer access any buffer whose PMT is being
    /// unpinned.
    unsafe fn run_deferred(&self, mut deferred: Deferred<'_, M>) {
        for DeferredSlot { pmt, outcome } in deferred.drain() {
            // 1. Invalidate CPU caches so the transferred data is visible to the client.
            let outcome = outcome.map(|outcome| self.invalidate_read_back(outcome));
            // 2. Unpin the pages.
            // SAFETY: Guaranteed by the caller.
            unsafe { unpin_pmt(pmt) };
            // 3. Report the outcome, which may hand the buffer back to the client.
            if let Some(outcome) = outcome {
                self.complete_io(outcome.lun, outcome.completion, outcome.status);
            }
        }
    }

    /// Returns whether any I/O slot is currently in [`SlotState::Timeout`].
    fn has_timed_out_io_slots(&self) -> bool {
        let list = self.request_list.lock();
        if list.stopped {
            return false;
        }
        let mut remaining = list.scheduled_mask & IO_SLOT_MASK;
        while remaining != 0 {
            let slot = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1;
            if list.slot(slot).is_ok_and(|s| s.state == SlotState::Timeout) {
                return true;
            }
        }
        false
    }

    /// Polls timed-out I/O slots every [`TIMEOUT_RECLAIM_POLL_INTERVAL`] for up to
    /// [`TIMEOUT_RECLAIM_WINDOW`] so their doorbell bits can clear after `UTRLCLR`. If any slot
    /// remains in [`SlotState::Timeout`] after the abort window, requests full controller and link
    /// recovery ([`Self::request_link_recovery`]).
    async fn reclaim_timed_out_io_slots(&self) {
        if !self.has_timed_out_io_slots() {
            return;
        }
        let deadline = zx::MonotonicInstant::after(TIMEOUT_RECLAIM_WINDOW);
        while zx::MonotonicInstant::get() < deadline {
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL))
                .await;
            self.process_completion_of_io_requests();
            if !self.has_timed_out_io_slots() {
                return;
            }
        }
        if self.has_timed_out_io_slots() {
            warn!("Timed-out I/O slot did not clear after abort; requesting link recovery");
            self.request_link_recovery();
        }
    }

    /// Command-timeout watchdog for synchronous I/O requests, whose submitters do not wait
    /// (mirroring the C++ driver's `timeout_task_` and Linux `ufshcd` command timeouts).
    ///
    /// Hardware link/controller/protocol errors raise an interrupt (`IS.ULLS`, `IS.DFES`,
    /// `IS.HCFES`, `IS.SBFES`, `IS.CEFES`, `IS.UE`) and immediately trigger
    /// [`Self::request_link_recovery`]. UFSHCI has no hardware per-command completion timer,
    /// however, so if device firmware wedges on a command without dropping the link or raising an
    /// error interrupt, no interrupt fires. This task bounds synchronous I/O with a one-shot timer
    /// at [`COMMAND_TIMEOUT`], aborts expired slots with `UTRLCLR`, and escalates to
    /// [`Self::request_link_recovery`] via [`Self::reclaim_timed_out_io_slots`] if the aborted slot
    /// does not clear within [`TIMEOUT_RECLAIM_WINDOW`]. Returns once [`Self::quiesce`] stops the
    /// processor; run it on the driver's scope.
    ///
    /// Idle with no timer while no synchronous request is in flight (woken by
    /// [`Self::schedule_slot_locked`] on the 0 -> 1 transition). Otherwise sleeps on a single
    /// one-shot timer at the earliest deadline reported by [`RequestList::next_io_deadline`].
    /// Because all requests share one [`COMMAND_TIMEOUT`], a slot scheduled while the timer is
    /// armed never has an earlier deadline than the one being waited on, so one timer suffices.
    pub async fn run_io_timeout_task(&self) {
        loop {
            let Some(wake_at) = std::future::poll_fn(|cx| {
                // Register first so a submission or quiesce racing with the check is not missed.
                self.timeout_waker.register(cx.waker());
                let list = self.request_list.lock();
                if list.stopped {
                    return Poll::Ready(None);
                }
                if list.inflight_io == 0 {
                    return Poll::Pending;
                }
                // Every in-flight synchronous slot is `Scheduled` with a deadline or `Timeout`, so
                // a deadline is always reported while `inflight_io > 0`.
                Poll::Ready(Some(
                    list.next_io_deadline(IO_SLOT_MASK)
                        .expect("synchronous I/O in flight without a deadline"),
                ))
            })
            .await
            else {
                return;
            };
            fuchsia_async::Timer::new(wake_at).await;
            if self.is_stopped() {
                return;
            }
            self.process_completion_of_io_requests();
            self.reclaim_timed_out_io_slots().await;
        }
    }

    /// Waits until `slot` leaves `Scheduled` and returns its new state.
    ///
    /// On the interrupt-driven path (`rx` resolves), returns the state sent by the completion
    /// handler directly without locking `request_list` or issuing any MMIO reads. The only other
    /// wakeup is a single one-shot timer at [`COMMAND_TIMEOUT`]: the slot's deadline was set
    /// before the timer was armed, so when it fires [`Self::poll_slot`] either completes the slot
    /// (its doorbell bit has cleared) or times it out with `UTRLCLR`.
    async fn wait_for_slot(&self, slot: u8, rx: oneshot::Receiver<SlotState>) -> SlotState {
        let mut rx = rx.fuse();
        let mut timeout = std::pin::pin!(
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(COMMAND_TIMEOUT)).fuse()
        );
        loop {
            futures::select_biased! {
                res = rx => {
                    if let Ok(state) = res {
                        return state;
                    }
                    let state =
                        self.request_list.lock().slot(slot).map_or(SlotState::Free, |s| s.state);
                    if state != SlotState::Scheduled {
                        return state;
                    }
                }
                _ = timeout => {
                    let state = self.poll_slot(slot);
                    if state != SlotState::Scheduled {
                        return state;
                    }
                    // Not expected: the deadline predates the timer. Re-arm briefly rather
                    // than wait on `rx` alone.
                    timeout.set(
                        fuchsia_async::Timer::new(zx::MonotonicInstant::after(
                            TIMEOUT_RECLAIM_POLL_INTERVAL,
                        ))
                        .fuse(),
                    );
                }
            }
        }
    }

    /// Polls a timed-out slot's doorbell bit every [`TIMEOUT_RECLAIM_POLL_INTERVAL`] for up to
    /// [`TIMEOUT_RECLAIM_WINDOW`] so it can be reclaimed promptly after `UTRLCLR`, which raises
    /// no interrupt (like Linux `ufshcd_clear_cmd`). If the bit does not clear in time, escalates
    /// to full controller and link recovery ([`Self::request_link_recovery`]).
    async fn reclaim_timed_out_slot(&self, slot: u8) {
        let deadline = zx::MonotonicInstant::after(TIMEOUT_RECLAIM_WINDOW);
        while !self.is_stopped()
            && self.poll_slot(slot) == SlotState::Timeout
            && zx::MonotonicInstant::get() < deadline
        {
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL))
                .await;
        }
        if !self.is_stopped() && self.poll_slot(slot) == SlotState::Timeout {
            warn!(slot; "Timed-out slot did not clear after abort; requesting link recovery");
            self.request_link_recovery();
        }
    }

    /// Submits a request on `guard`'s slot, waits for it, reads the response via `read_response`
    /// while releasing the slot in the same `request_list` lock acquisition, and validates the
    /// response header.
    ///
    /// # Cancellation Safety
    ///
    /// Dropping this future at any `.await` point is DMA-safe; see [`SlotGuard`] for how
    /// `Reserved`, `Scheduled`, and `Completed` slots and their pinned buffers are reclaimed.
    async fn execute_with_reader<R, F>(
        &self,
        guard: &mut SlotGuard<'_, M>,
        layout: CommandDescriptorLayout,
        request: &[u8],
        expected_response: UpiuTransactionCode,
        data: Option<DataTransfer<'_>>,
        read_response: F,
    ) -> Result<R, UfsError>
    where
        F: FnOnce(&ContiguousDmaBuffer, usize) -> Result<(R, UpiuHeader), UfsError>,
    {
        let slot = guard.slot;
        let (tx, rx) = oneshot::channel();
        let mut completion = SlotCompletion::Async(tx);
        self.submit(slot, layout, request, data.as_ref(), &mut completion)?;
        match self.wait_for_slot(slot, rx).await {
            SlotState::Completed => {}
            SlotState::Timeout => {
                // The completion path now owns the slot.
                guard.armed = false;
                self.reclaim_timed_out_slot(slot).await;
                error!(slot; "Slot timed out");
                return Err(UfsError::Timeout);
            }
            state => {
                error!(slot, state:?; "Slot in unexpected state");
                return Err(UfsError::IoError);
            }
        }

        let (result, pmt) = {
            let mut list = self.request_list.lock();
            let res = list.slot(slot).and_then(|request_slot| {
                request_slot.result?;
                read_response(&request_slot.command_descriptor_io, layout.response_offset())
            });
            let pmt = self.take_and_release_slot(&mut list, slot);
            guard.armed = false;
            (res, pmt)
        };
        // Explicit cleanup in the same order as `cqhci::PendingTask::complete`:
        // 1. Invalidate CPU caches so the transferred data is visible to the caller.
        let invalidated =
            match data.filter(|d| d.direction == DataDirection::DeviceToHost && d.length > 0) {
                Some(data) => data.invalidate_read_cache().map_err(|s| {
                    error!(status:% = s; "Failed to invalidate transfer buffer cache after read");
                    UfsError::IoError
                }),
                None => Ok(()),
            };
        // 2. Unpin the pages.
        // SAFETY: The slot's doorbell bit was observed clear.
        unsafe { unpin_pmt(pmt) };
        // 3. Return the response.
        let (response, header) = result?;
        check_response_header(header.as_bytes(), expected_response, slot)?;
        invalidated?;
        Ok(response)
    }

    /// Submits a request on `guard`'s slot, waits for it and returns the validated response
    /// UPIU.
    async fn execute<R: FromBytes + IntoBytes + Immutable>(
        &self,
        guard: &mut SlotGuard<'_, M>,
        layout: CommandDescriptorLayout,
        request: &[u8],
        expected_response: UpiuTransactionCode,
        data: Option<DataTransfer<'_>>,
    ) -> Result<R, UfsError> {
        debug_assert_eq!(std::mem::size_of::<R>(), layout.response_length());
        self.execute_with_reader(guard, layout, request, expected_response, data, |ucd, offset| {
            let response: R = ucd.read_struct(offset).map_err(|_| UfsError::IoError)?;
            let (header, _) =
                UpiuHeader::read_from_prefix(response.as_bytes()).map_err(|_| UfsError::IoError)?;
            Ok((response, header))
        })
        .await
    }

    /// Reads `UTRLDBR`.
    fn read_doorbell(&self) -> u32 {
        self.controller.with_mmio(|mmio| UtrListDoorBellReg::read(mmio).door_bell())
    }

    /// Reads a slot's `OCS`, re-reading while it is still `Invalid` in case the controller's
    /// descriptor write is not yet visible.
    fn read_ocs(&self, list: &RequestList, slot: u8) -> u8 {
        let offset = slot as usize * std::mem::size_of::<TransferRequestDescriptor>()
            + TransferRequestDescriptor::OCS_OFFSET;
        let read = || match list.io_buffer.read_struct::<u32>(offset) {
            Ok(dword2) => dword2 as u8,
            Err(_) => OverallCommandStatus::Invalid as u8,
        };
        let mut ocs = read();
        for _ in 1..OCS_RETRIES {
            if ocs != OverallCommandStatus::Invalid as u8 {
                break;
            }
            std::hint::spin_loop();
            ocs = read();
        }
        ocs
    }

    /// Decodes the RESPONSE UPIU of a finished synchronous SCSI command in `slot`'s UCD.
    fn decode_io_response(list: &RequestList, slot: u8, lun: u8) -> Result<(), UfsError> {
        let request_slot = list.slot(slot)?;
        let (response, header) = read_scsi_response(
            &request_slot.command_descriptor_io,
            CommandDescriptorLayout::SCSI_COMMAND.response_offset(),
        )?;
        check_response_header(header.as_bytes(), UpiuTransactionCode::Response, slot)?;
        match check_scsi_status(lun, &response) {
            ScsiOutcome::Good => Ok(()),
            ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => Err(error),
        }
    }

    /// Records `result` for a finished `slot` and wakes its async submitter; otherwise releases the
    /// slot and defers unpinning its PMT and (for synchronous I/O) delivering its outcome until
    /// `request_list` is released.
    fn finish_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        result: Result<(), UfsError>,
        deferred: &mut Deferred<'_, M>,
    ) {
        let Ok(request_slot) = list.slot_mut(slot) else {
            return;
        };
        request_slot.result = result;
        request_slot.deadline = None;
        match std::mem::take(&mut request_slot.completion) {
            SlotCompletion::Async(sender) => {
                list.set_slot_state(slot, SlotState::Completed);
                let _ = sender.send(SlotState::Completed);
            }
            SlotCompletion::Io { lun, completion, read_back } => {
                let status = result
                    .and_then(|()| Self::decode_io_response(list, slot, lun))
                    .map_err(zx::Status::from);
                let pmt = self.release_slot_locked(list, slot);
                deferred.push(pmt, Some(IoOutcome { lun, completion, read_back, status }));
            }
            // A dropped async submitter means its guard is releasing the `Completed` slot.
            SlotCompletion::None => deferred.push(self.release_slot_locked(list, slot), None),
        }
    }

    /// Marks a `Scheduled` slot `Timeout` and reports the timeout to its submitter. The slot's
    /// buffers stay pinned and the completion path frees it once the controller is done with it.
    fn time_out_slot_locked(list: &mut RequestList, slot: u8, deferred: &mut Deferred<'_, M>) {
        let Ok(request_slot) = list.slot_mut(slot) else {
            return;
        };
        request_slot.result = Err(UfsError::Timeout);
        request_slot.deadline = None;
        let completion = std::mem::take(&mut request_slot.completion);
        list.set_slot_state(slot, SlotState::Timeout);
        match completion {
            SlotCompletion::Async(sender) => {
                let _ = sender.send(SlotState::Timeout);
            }
            SlotCompletion::Io { lun, completion, .. } => {
                let status = Err(zx::Status::TIMED_OUT);
                deferred.push(None, Some(IoOutcome { lun, completion, read_back: None, status }));
            }
            SlotCompletion::None => {}
        }
    }

    /// Returns whether `slot` has reached its deadline (inclusive, so a one-shot timer armed at
    /// the deadline always finds it expired). A slot without a deadline (never scheduled, or
    /// already finished or timed out) counts as past it, so callers that only look at
    /// `Scheduled` slots never wait on a deadline that was cleared underneath them.
    fn is_past_deadline(list: &RequestList, slot: u8, now: zx::MonotonicInstant) -> bool {
        list.slot(slot).is_ok_and(|s| s.deadline.is_none_or(|deadline| now >= deadline))
    }

    /// Advances `slot`'s state machine given the current `doorbell` value. Returns whether the
    /// slot changed state; work that must run outside `request_list.lock()` goes to `deferred`.
    fn process_sdb_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        doorbell: u32,
        now: zx::MonotonicInstant,
        deferred: &mut Deferred<'_, M>,
    ) -> bool {
        if slot as usize >= SDB_SLOTS {
            return false;
        }
        let Ok(request_slot) = list.slot(slot) else {
            return false;
        };
        let outstanding = doorbell & (1 << slot) != 0;
        let is_past_deadline = Self::is_past_deadline(list, slot, now);
        match (request_slot.state, outstanding) {
            (SlotState::Scheduled, false) => {
                let ocs = self.read_ocs(list, slot);
                let result = match OverallCommandStatus::from_raw(ocs) {
                    Some(OverallCommandStatus::Success) => Ok(()),
                    Some(OverallCommandStatus::Invalid) if !is_past_deadline => return false,
                    Some(OverallCommandStatus::Invalid) => {
                        error!(slot; "Slot doorbell cleared but OCS never became valid");
                        Err(UfsError::Timeout)
                    }
                    status => {
                        error!(slot, ocs, status:?; "Slot failed with OCS error");
                        Err(UfsError::IoError)
                    }
                };
                self.finish_slot_locked(list, slot, result, deferred);
                true
            }
            (SlotState::Scheduled, true) if is_past_deadline => {
                self.controller.with_mmio(|mmio| UtrListClearReg(!(1u32 << slot)).write(mmio));
                Self::time_out_slot_locked(list, slot, deferred);
                true
            }
            // Covers both normal post-`UTRLCLR` doorbell clearing and the race where hardware
            // completed the request just before `UTRLCLR` took effect (raising a completion
            // interrupt after the slot entered `Timeout`): `time_out_slot_locked` already took
            // `request_slot.completion` to notify the submitter once, so here we unpin the PMT via
            // `Deferred` and return the slot to `Free` with `outcome: None`.
            (SlotState::Timeout, false) => {
                deferred.push(self.release_slot_locked(list, slot), None);
                true
            }
            _ => false,
        }
    }

    /// Processes completions and timeouts of `slot_mask`, then unpins orphaned buffers and
    /// delivers synchronous I/O outcomes after releasing `request_list`. Returns the number of
    /// slots that changed state. This is the single lock-once wrapper used by every completion
    /// entry point.
    fn process_slots(&self, slot_mask: u64) -> u32 {
        let mut deferred = Deferred::new(self);
        let completed = {
            let mut list = self.request_list.lock();
            self.process_slots_locked(&mut list, slot_mask, &mut deferred)
        };
        // SAFETY: Each unpinned slot's doorbell bit was observed clear; timed-out slots keep
        // their PMT.
        unsafe { self.run_deferred(deferred) };
        completed
    }

    /// Processes completions and timeouts of `slot_mask` while holding `request_list`.
    fn process_slots_locked(
        &self,
        list: &mut RequestList,
        slot_mask: u64,
        deferred: &mut Deferred<'_, M>,
    ) -> u32 {
        if list.stopped {
            return 0;
        }
        let active = (list.scheduled_mask & slot_mask) as u32;
        if active == 0 {
            return 0;
        }
        let free_before = list.free_mask;
        let doorbell = self.read_doorbell();
        let now = zx::MonotonicInstant::get();
        let mut completed = 0;
        let mut remaining = active;
        while remaining != 0 {
            let slot = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1;
            if self.process_sdb_slot_locked(list, slot, doorbell, now, deferred) {
                completed += 1;
            }
        }
        let released_io = ((list.free_mask & !free_before) & IO_SLOT_MASK).count_ones();
        if released_io > 0 {
            self.wake_all_slot_waiters(list);
        }
        completed
    }

    /// Processes `slot` and returns its resulting state, acquiring `request_list` once.
    fn poll_slot(&self, slot: u8) -> SlotState {
        let mut deferred = Deferred::new(self);
        let state = {
            let mut list = self.request_list.lock();
            // Returns early (without touching the controller) unless the slot is `Scheduled` or
            // `Timeout`.
            self.process_slots_locked(&mut list, 1u64 << slot, &mut deferred);
            list.slot(slot).map_or(SlotState::Free, |s| s.state)
        };
        // SAFETY: Each unpinned slot's doorbell bit was observed clear; timed-out slots keep
        // their PMT.
        unsafe { self.run_deferred(deferred) };
        state
    }

    /// Processes a possible completion or timeout of `slot`. Returns whether it changed state.
    pub fn process_slot_completion(&self, slot: u8) -> bool {
        (slot as usize) < SDB_SLOTS && self.process_slots(1u64 << slot) > 0
    }

    /// Processes all I/O slots. Returns the number of slots that changed state.
    pub fn process_completion_of_io_requests(&self) -> u32 {
        self.process_slots(IO_SLOT_MASK)
    }

    /// Services an interrupt: clears and logs the handled `IS` bits, and processes SDB
    /// completions. Status is cleared before the state it reports is read, so concurrent calls
    /// lose no event. Returns `IS` as read.
    pub fn service_interrupt(&self) -> InterruptStatusReg {
        if self.is_stopped() {
            return InterruptStatusReg::default();
        }
        let status = self.controller.fetch_and_clear_interrupts();
        if self.controller.is_recovery_needed() {
            self.recovery_waker.wake();
        }
        if status.utp_transfer_request_completion_status() || status.cq_event_status() {
            self.process_slots(ALL_SLOTS_MASK);
        }
        status
    }

    /// Sends a QUERY REQUEST UPIU on the admin slot and returns the QUERY RESPONSE UPIU.
    ///
    /// The response's `Query Response` code is not checked; callers interpret it.
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `IoError` on a transport error or malformed response.
    pub async fn send_query_request_upiu(
        &self,
        query_request: &QueryUpiu,
    ) -> Result<QueryUpiu, UfsError> {
        let mut admin = self.reserve_admin_slot().await?;
        let mut request = *query_request;
        request.header.task_tag = admin.slot.slot();
        self.execute(
            &mut admin.slot,
            CommandDescriptorLayout::QUERY,
            request.as_bytes(),
            UpiuTransactionCode::QueryResponse,
            None,
        )
        .await
    }

    /// Sends a NOP OUT UPIU on the admin slot and checks the NOP IN response.
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `IoError` on a transport error or a failed NOP IN.
    pub async fn send_nop_out_upiu(&self) -> Result<(), UfsError> {
        let mut admin = self.reserve_admin_slot().await?;
        let mut nop = NopOutUpiu::new();
        nop.header.task_tag = admin.slot.slot();
        let response: NopInUpiu = self
            .execute(
                &mut admin.slot,
                CommandDescriptorLayout::NOP,
                nop.as_bytes(),
                UpiuTransactionCode::NopIn,
                None,
            )
            .await?;
        if response.header.response != 0 {
            error!(response = response.header.response; "NOP IN reported non-zero response");
            return Err(UfsError::IoError);
        }
        Ok(())
    }

    /// Sends a SCSI command to `lun` and awaits its completion, transferring `data` (if any) to or
    /// from the device. Retries up to [`SCSI_RETRIES`] times on UNIT ATTENTION, NOT READY, BUSY,
    /// and TASK SET FULL. For block I/O use [`Self::submit_io`] instead.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` for a misaligned or oversized buffer, `Timeout`,
    /// `CheckCondition` for a CHECK CONDITION status, or `IoError`.
    pub async fn send_scsi_command(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
    ) -> Result<ResponseUpiu, UfsError> {
        let data = data.filter(|data| data.length != 0);
        let expected_transfer_len = data.map_or(0, |data| data.length);
        let direction = data.map_or(DataDirection::NoData, |data| data.direction);
        let mut retries = 0;
        loop {
            let mut guard = self.reserve_io_slot().await?;
            let mut command = CommandUpiu::new(cdb, expected_transfer_len, direction);
            command.header.lun = lun;
            command.header.task_tag = guard.slot();
            let response: ResponseUpiu = self
                .execute_with_reader(
                    &mut guard,
                    CommandDescriptorLayout::SCSI_COMMAND,
                    command.as_bytes(),
                    UpiuTransactionCode::Response,
                    data,
                    read_scsi_response,
                )
                .await?;
            drop(guard);
            match check_scsi_status(lun, &response) {
                ScsiOutcome::Good => return Ok(response),
                ScsiOutcome::Retry { error: _, delay } if retries < SCSI_RETRIES => {
                    retries += 1;
                    if delay {
                        fuchsia_async::Timer::new(zx::MonotonicInstant::after(SCSI_RETRY_DELAY))
                            .await;
                    }
                }
                ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => return Err(error),
            }
        }
    }

    /// Stops the controller so it can no longer access any buffer, fails outstanding requests
    /// (synchronous I/O completes with `IO`), wakes blocked submitters and unpins all DMA memory.
    /// Later submissions fail with `IoError`.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCE` does not clear; DMA memory then stays pinned.
    pub async fn quiesce(&self) -> Result<(), UfsError> {
        {
            let mut list = self.request_list.lock();
            list.stopped = true;
            let outstanding = (list.scheduled_mask & ALL_SLOTS_MASK) as u32;
            self.controller.with_mmio(|mmio| {
                if outstanding != 0 {
                    UtrListClearReg(!outstanding).write(mmio);
                }
                UtrListRunStopReg::from_raw(0).write(mmio);
            });
            // Blocked and async reservers observe `stopped` and fail.
            self.wake_all_slot_waiters(&mut list);
            self.timeout_waker.wake();
            self.recovery_waker.wake();
        }
        self.controller.disable_host_controller().await.inspect_err(|e| {
            error!(err:? = e; "Failed to disable host controller, leaving DMA memory pinned");
        })?;

        let mut deferred = Deferred::new(self);
        {
            let mut list = self.request_list.lock();
            let mut remaining = list.scheduled_mask;
            while remaining != 0 {
                let slot = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                self.finish_slot_locked(&mut list, slot, Err(UfsError::IoError), &mut deferred);
            }
            let RequestList { io_buffer, request_slots, .. } = &mut *list;
            for buffer in std::iter::once(io_buffer)
                .chain(request_slots.iter_mut().map(|s| &mut s.command_descriptor_io))
            {
                // SAFETY: `HCE` reads 0, so the controller performs no DMA.
                if let Err(status) = unsafe { buffer.unpin() } {
                    error!(status:%; "Failed to unpin descriptor buffer");
                }
            }
        }
        // SAFETY: `HCE` reads 0, so the controller performs no DMA.
        unsafe { self.run_deferred(deferred) };
        Ok(())
    }
}

/// Reads the RESPONSE UPIU of a SCSI command at `offset` in `ucd`, returning it with its header.
fn read_scsi_response(
    ucd: &ContiguousDmaBuffer,
    offset: usize,
) -> Result<(ResponseUpiu, UpiuHeader), UfsError> {
    // Read 16 bytes (two 64-bit loads) to inspect the 12-byte UPIU header without copying the
    // remaining 40 bytes (reserved fields, sense data and padding) on the fast path.
    let words: [u64; 2] = ucd.read_struct(offset).map_err(|_| UfsError::IoError)?;
    let (header, _) =
        UpiuHeader::read_from_prefix(words.as_bytes()).map_err(|_| UfsError::IoError)?;
    if header.status == ScsiStatus::Good.as_u8() && header.response == 0 && header.flags == 0 {
        let mut response = ResponseUpiu::new_zeroed();
        response.header = header;
        Ok((response, header))
    } else {
        let full: ResponseUpiu = ucd.read_struct(offset).map_err(|_| UfsError::IoError)?;
        Ok((full, header))
    }
}

/// Checks the transaction code and task tag of a response UPIU.
fn check_response_header(
    response: &[u8],
    expected: UpiuTransactionCode,
    task_tag: u8,
) -> Result<(), UfsError> {
    let (header, _) = UpiuHeader::read_from_prefix(response).map_err(|_| UfsError::IoError)?;
    if header.trans_code() != expected as u8 {
        error!(
            trans_code = header.trans_code(),
            expected = expected as u8;
            "Unexpected response transaction code"
        );
        return Err(UfsError::IoError);
    }
    if header.task_tag != task_tag {
        error!(
            task_tag = header.task_tag, slot = task_tag;
            "Response task tag does not match slot"
        );
        return Err(UfsError::IoError);
    }
    Ok(())
}

/// Returns `(sense key, ASC, ASCQ)` from descriptor-format (SPC-4 section 4.5.2: bytes 1..=3) or
/// fixed-format (SPC-4 section 4.5.3: bytes 2, 12, 13) sense data.
fn parse_sense(response: &ResponseUpiu) -> (u8, u8, u8) {
    let length = (response.sense_data_length.get() as usize).min(response.sense_data.len());
    let sense = &response.sense_data[..length];
    let byte = |index: usize| sense.get(index).copied().unwrap_or(0);
    match byte(0) & SENSE_RESPONSE_CODE_MASK {
        SENSE_DESCRIPTOR_FORMAT_CURRENT | SENSE_DESCRIPTOR_FORMAT_DEFERRED => {
            (byte(1) & 0xF, byte(2), byte(3))
        }
        _ => (byte(2) & 0xF, byte(12), byte(13)),
    }
}

/// Classification of a RESPONSE UPIU by [`check_scsi_status`].
enum ScsiOutcome {
    /// GOOD status and no transport-level response error.
    Good,
    /// A transient failure (UNIT ATTENTION, NOT READY, BUSY, TASK SET FULL) worth retrying.
    Retry { error: UfsError, delay: bool },
    /// A failure that retrying will not fix.
    Fail(UfsError),
}

/// Classifies a RESPONSE UPIU and logs failures.
fn check_scsi_status(lun: u8, response: &ResponseUpiu) -> ScsiOutcome {
    let raw_status = response.header.status;
    let Some(status) = ScsiStatus::from_raw(raw_status) else {
        error!(lun, status = raw_status; "SCSI error status");
        return ScsiOutcome::Fail(UfsError::IoError);
    };
    match status {
        ScsiStatus::Good if response.header.response == 0 => ScsiOutcome::Good,
        ScsiStatus::Good => {
            warn!(lun, response = response.header.response; "Target failure");
            ScsiOutcome::Fail(UfsError::IoError)
        }
        ScsiStatus::CheckCondition => {
            let (raw_sense_key, asc, ascq) = parse_sense(response);
            match SenseKey::from_raw(raw_sense_key) {
                Some(SenseKey::UnitAttention) => {
                    info!(lun, asc, ascq; "UNIT ATTENTION");
                    ScsiOutcome::Retry { error: UfsError::CheckCondition, delay: false }
                }
                Some(SenseKey::NotReady) => {
                    info!(lun, asc, ascq; "NOT READY");
                    ScsiOutcome::Retry { error: UfsError::CheckCondition, delay: true }
                }
                Some(_) | None => {
                    warn!(lun, sense_key = raw_sense_key, asc, ascq; "CHECK CONDITION");
                    ScsiOutcome::Fail(UfsError::CheckCondition)
                }
            }
        }
        ScsiStatus::Busy | ScsiStatus::TaskSetFull => {
            warn!(lun, status = status.as_u8(); "Transient SCSI status");
            ScsiOutcome::Retry { error: UfsError::IoError, delay: true }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryOpcode;
    use crate::registers::HostControllerEnableReg;
    use crate::scsi::{InquiryData, ScsiOpcode};
    use crate::testing::fake_ufs::{FakeMmio, FakeUfs, wait_until};
    use crate::testing::{
        ADMIN_SLOT, IoOutcomeReceiver, OCS_SUCCESS, PAGE_SIZE, check_condition,
        create_test_processor, delivered, expire_deadline, fake_utp, io_callback, nop_in,
        scsi_response, slot_pinned, slot_state, wait_for_any_doorbell,
    };
    use futures::poll;
    use std::sync::atomic::{AtomicBool, Ordering};
    use zerocopy::byteorder::big_endian::U32;

    /// UCD byte offset of a NOP IN response.
    const NOP_RESPONSE_OFFSET: usize = CommandDescriptorLayout::NOP.response_offset();

    #[fuchsia::test]
    async fn test_nop_out_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            let utrd = utp.utrd(ADMIN_SLOT);
            let request: NopOutUpiu = utp.request(ADMIN_SLOT);
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
            (utrd, request)
        });

        assert_eq!(processor.send_nop_out_upiu().await, Ok(()));
        let (utrd, request) = hardware.join().unwrap();
        assert_eq!(utrd.command_type(), TransferRequestDescriptor::COMMAND_TYPE_UFS_STORAGE);
        assert_eq!(utrd.data_direction(), DataDirection::NoData as u32);
        assert!(utrd.interrupt());
        assert_eq!((utrd.response_upiu_offset(), utrd.response_upiu_length()), (8, 8));
        assert_eq!(utrd.prdt_length(), 0);
        assert_eq!(request.header.trans_code(), UpiuTransactionCode::NopOut as u8);
        assert_eq!(request.header.task_tag, ADMIN_SLOT);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_admin_ocs_and_response_validation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            // 1: success, leaving a valid NOP IN in the UCD.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
            // 2: communication failure without a new response.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.set_ocs(ADMIN_SLOT, OverallCommandStatus::CommunicationFailure as u8);
            utp.clear_doorbell(ADMIN_SLOT);
            utp.raise_completion_interrupt();
            // 3: OCS success but no response written.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.set_ocs(ADMIN_SLOT, OCS_SUCCESS);
            utp.clear_doorbell(ADMIN_SLOT);
            utp.raise_completion_interrupt();
            // 4: response with the wrong task tag.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(3).as_bytes(), OCS_SUCCESS);
        });

        assert_eq!(processor.send_nop_out_upiu().await, Ok(()));
        assert_eq!(processor.send_nop_out_upiu().await, Err(UfsError::IoError));
        assert_eq!(processor.send_nop_out_upiu().await, Err(UfsError::IoError));
        assert_eq!(processor.send_nop_out_upiu().await, Err(UfsError::IoError));
        hardware.join().unwrap();
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_scsi_ocs_and_response_validation() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            // 1: GOOD response but invalid PRDT attributes.
            let slot = wait_for_any_doorbell(&utp);
            let response = scsi_response(slot, 0);
            utp.complete(
                slot,
                response.as_bytes(),
                OverallCommandStatus::InvalidPrdtAttributes as u8,
            );
            // 2: REJECT UPIU instead of RESPONSE.
            let slot = wait_for_any_doorbell(&utp);
            let mut response = scsi_response(slot, 0);
            response.header.trans_type = UpiuTransactionCode::RejectUpiu as u8;
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            // 3: wrong task tag.
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot + 1, 0).as_bytes(), OCS_SUCCESS);
            // 4: target failure.
            let slot = wait_for_any_doorbell(&utp);
            let mut response = scsi_response(slot, 0);
            response.header.response = 1;
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            // 5: success.
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        for _ in 0..4 {
            assert_eq!(
                processor.send_scsi_command(0, cdb, None).await.err(),
                Some(UfsError::IoError)
            );
        }
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_concurrent_queries_serialize() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            for _ in 0..4 {
                assert!(utp.wait_for_doorbell(ADMIN_SLOT));
                let request: QueryUpiu = utp.request(ADMIN_SLOT);
                let mut response = request;
                response.header.trans_type = UpiuTransactionCode::QueryResponse as u8;
                response.value = U32::new(request.idn as u32 * 10);
                utp.complete(ADMIN_SLOT, response.as_bytes(), OCS_SUCCESS);
            }
        });

        let requests: Vec<_> =
            (1..=4).map(|idn| QueryUpiu::new(QueryOpcode::ReadAttribute, idn, 0, 0)).collect();
        let results = futures::future::join_all(
            requests.iter().map(|r| processor.send_query_request_upiu(r)),
        )
        .await;
        for (idn, result) in (1..=4).zip(results) {
            assert_eq!(result.unwrap().value(), idn * 10);
        }
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_dropped_request_frees_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Dropped after the completion interrupt was delivered but before it resumed.
        let mut request = Box::pin(processor.send_nop_out_upiu());
        assert!(poll!(request.as_mut()).is_pending());
        utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Completed);
        drop(request);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);

        // Dropped while the controller still owns the slot: the completion interrupt frees it and
        // unpins the orphaned buffer.
        let data_vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(&data_vmo, 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());
        drop(request);
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));

        // The admin slot is still usable.
        let hardware = std::thread::spawn(move || {
            assert!(utp.wait_for_doorbell(ADMIN_SLOT));
            utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        });
        assert_eq!(processor.send_nop_out_upiu().await, Ok(()));
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_timeout_keeps_buffers_pinned_until_doorbell_clears() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(&data_vmo, 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());
        assert_ne!(utp.doorbell() & 1, 0);

        // The interrupt path aborts the expired request.
        expire_deadline(&processor, 0);
        assert_eq!(processor.process_completion_of_io_requests(), 1);
        assert_eq!(utp.regs.load32(UtrListClearReg::OFFSET), !1u32);
        {
            let list = processor.request_list.lock();
            let slot = list.slot(0).unwrap();
            assert_eq!(slot.state(), SlotState::Timeout);
            assert!(slot.deadline.is_none());
            assert!(matches!(slot.completion, SlotCompletion::None));
        }

        // The waiter reports the timeout, requests link recovery since the doorbell bit did not
        // clear within the abort window, and must not unpin while the doorbell bit is set.
        assert_eq!(request.await.err(), Some(UfsError::Timeout));
        assert!(processor.controller().is_recovery_needed());
        assert_eq!(slot_state(&processor, 0), SlotState::Timeout);
        assert!(slot_pinned(&processor, 0));
        assert!(!processor.process_slot_completion(0));

        utp.clear_doorbell(0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
    }

    #[fuchsia::test]
    async fn test_timed_out_slot_reclaimed_by_waiter() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();

        let mut request = Box::pin(processor.send_scsi_command(0, cdb, None));
        assert!(poll!(request.as_mut()).is_pending());
        // The command timeout aborts the request with UTRLCLR and notifies the waiter.
        expire_deadline(&processor, 0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Timeout);

        // The controller finishes the abort shortly after UTRLCLR without raising an interrupt;
        // the waiter polls the doorbell bit and reclaims the slot without needing link recovery.
        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| utp.regs.load32(UtrListClearReg::OFFSET) == !1u32));
            std::thread::sleep(std::time::Duration::from_millis(10));
            utp.clear_doorbell(0);
        });
        assert_eq!(request.await.err(), Some(UfsError::Timeout));
        hardware.join().unwrap();
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!processor.controller().is_recovery_needed());
    }

    #[fuchsia::test]
    async fn test_ocs_invalid_after_doorbell_clear() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // A late OCS write before the deadline is picked up once visible.
        let mut request = Box::pin(processor.send_nop_out_upiu());
        assert!(poll!(request.as_mut()).is_pending());
        utp.write_ucd(ADMIN_SLOT, NOP_RESPONSE_OFFSET, nop_in(ADMIN_SLOT).as_bytes());
        utp.clear_doorbell(ADMIN_SLOT);
        assert!(!processor.process_slot_completion(ADMIN_SLOT));
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Scheduled);
        utp.set_ocs(ADMIN_SLOT, OCS_SUCCESS);
        assert!(processor.process_slot_completion(ADMIN_SLOT));
        assert_eq!(request.await, Ok(()));

        // OCS still invalid past the deadline is a timeout, not a success.
        let mut request = Box::pin(processor.send_nop_out_upiu());
        assert!(poll!(request.as_mut()).is_pending());
        utp.write_ucd(ADMIN_SLOT, NOP_RESPONSE_OFFSET, nop_in(ADMIN_SLOT).as_bytes());
        utp.clear_doorbell(ADMIN_SLOT);
        expire_deadline(&processor, ADMIN_SLOT);
        assert!(processor.process_slot_completion(ADMIN_SLOT));
        assert_eq!(request.await, Err(UfsError::Timeout));
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_scsi_command_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let data_vmo_clone = data_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let utrd = utp.utrd(slot);
            let command: crate::upiu::CommandUpiu = utp.request(slot);
            let prdt_offset = CommandDescriptorLayout::SCSI_COMMAND.prdt_offset();
            let prdt_bytes = utp.read_ucd(slot, prdt_offset, size_of::<PrdtEntry>());
            let prdt = PrdtEntry::read_from_bytes(&prdt_bytes).unwrap();

            // The fake BTI's addresses are not real, so place the data directly in the VMO.
            let data =
                FakeUfs::new().execute_scsi_cmd(&command.cdb, DataDirection::DeviceToHost).unwrap();
            data_vmo_clone.write(&data, 0).unwrap();
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
            (slot, utrd, command, prdt)
        });

        let cdb = ScsiOpcode::build_inquiry_cdb(36);
        let response = processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(&data_vmo, 0, 36)))
            .await
            .unwrap();
        assert_eq!(response.header.trans_code(), UpiuTransactionCode::Response as u8);
        assert_eq!(response.header.status, 0x00);

        let (slot, utrd, command, prdt) = hardware.join().unwrap();
        assert_eq!(utrd.data_direction(), DataDirection::DeviceToHost as u32);
        assert_eq!((utrd.response_upiu_offset(), utrd.response_upiu_length()), (8, 14));
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        assert_eq!(prdt.byte_count(), 36);
        assert_eq!(command.header.task_tag, slot);
        assert_eq!(command.header.flags, crate::upiu::CommandUpiu::FLAG_READ);
        assert_eq!(command.expected_data_transfer_length.get(), 36);
        assert_eq!(command.cdb[0], ScsiOpcode::Inquiry as u8);
        assert!(!slot_pinned(&processor, slot));

        let mut inquiry_bytes = [0u8; 36];
        data_vmo.read(&mut inquiry_bytes, 0).unwrap();
        let inquiry = InquiryData::read_from_bytes(&inquiry_bytes[..]).unwrap();
        assert_eq!(inquiry.vendor_id, *b"FUCHSIA ");
        assert_eq!(inquiry.product_id, *b"UFS MOCK DEVICE ");
    }

    #[fuchsia::test]
    async fn test_scsi_write_descriptors() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = zx::Vmo::create(3 * PAGE_SIZE).unwrap();

        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let utrd = utp.utrd(slot);
            let command: crate::upiu::CommandUpiu = utp.request(slot);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
            (utrd, command)
        });

        let cdb = ScsiOpcode::build_write_10_cdb(0, 2);
        processor
            .send_scsi_command(0, cdb, Some(DataTransfer::write(&data_vmo, 4096, 8192)))
            .await
            .unwrap();
        let (utrd, command) = hardware.join().unwrap();
        assert_eq!(utrd.data_direction(), DataDirection::HostToDevice as u32);
        assert_eq!(utrd.prdt_length(), 2);
        assert_eq!(command.header.flags, crate::upiu::CommandUpiu::FLAG_WRITE);
    }

    #[fuchsia::test]
    async fn test_scsi_rejects_invalid_buffers() {
        let (_mmio_vmo, processor) = create_test_processor();
        let data_vmo = zx::Vmo::create(258 * PAGE_SIZE).unwrap();
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let err = Some(UfsError::InvalidParameter);

        let unaligned =
            processor.send_scsi_command(0, cdb, Some(DataTransfer::read(&data_vmo, 2, 4096))).await;
        assert_eq!(unaligned.err(), err);
        let too_large = processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(&data_vmo, 0, 257 * 4096)))
            .await;
        assert_eq!(too_large.err(), err);
        let overflow = processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(&data_vmo, u64::MAX - 8, 4096)))
            .await;
        assert_eq!(overflow.err(), err);

        for slot in 0..SDB_SLOTS as u8 {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
            assert!(!slot_pinned(&processor, slot));
        }
        assert!(!processor.process_slot_completion(40));
    }

    #[fuchsia::test]
    async fn test_scsi_transient_status_retry() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(
                slot,
                scsi_response(slot, ScsiStatus::Busy.as_u8()).as_bytes(),
                OCS_SUCCESS,
            );
            let slot = wait_for_any_doorbell(&utp);
            let response = check_condition(slot, SenseKey::UnitAttention.as_u8());
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_send_scsi_command_retries_not_ready_check_condition() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            for _ in 0..2 {
                let slot = wait_for_any_doorbell(&utp);
                let response = check_condition(slot, SenseKey::NotReady.as_u8());
                utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            }
            let slot = wait_for_any_doorbell(&utp);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);

            for _ in 0..(SCSI_RETRIES + 1) {
                let slot = wait_for_any_doorbell(&utp);
                let response = check_condition(slot, SenseKey::NotReady.as_u8());
                utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            }
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let start = zx::MonotonicInstant::get();
        assert!(processor.send_scsi_command(0, cdb, None).await.is_ok());
        assert!(zx::MonotonicInstant::get() - start >= SCSI_RETRY_DELAY * 2);

        assert_eq!(
            processor.send_scsi_command(0, cdb, None).await.unwrap_err(),
            UfsError::CheckCondition
        );
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_scsi_check_condition_not_retried() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let response = check_condition(slot, SenseKey::IllegalRequest.as_u8());
            utp.complete(slot, response.as_bytes(), OCS_SUCCESS);
            utp
        });

        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        let result = processor.send_scsi_command(0, cdb, None).await;
        assert_eq!(result.err(), Some(UfsError::CheckCondition));
        let utp = hardware.join().unwrap();
        assert_eq!(utp.doorbell(), 0);
    }

    #[fuchsia::test]
    async fn test_slot_waiter_woken_on_release() {
        let (_mmio_vmo, processor) = create_test_processor();

        let mut guards = Vec::new();
        for _ in 0..ADMIN_SLOT {
            guards.push(processor.reserve_io_slot().await.unwrap());
        }
        assert!(guards.iter().all(|guard| guard.slot() < ADMIN_SLOT));

        let mut waiter = Box::pin(processor.reserve_io_slot());
        assert!(poll!(waiter.as_mut()).is_pending());
        let released = guards.remove(5).slot();
        match poll!(waiter.as_mut()) {
            std::task::Poll::Ready(Ok(guard)) => assert_eq!(guard.slot(), released),
            other => panic!("Waiter not woken: {:?}", other.map(|r| r.map(|g| g.slot()))),
        }
    }

    #[fuchsia::test]
    async fn test_quiesce_stops_controller_and_fails_requests() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = zx::Vmo::create(PAGE_SIZE).unwrap();
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(&data_vmo, 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());

        assert_eq!(processor.quiesce().await, Ok(()));
        assert_eq!(utp.regs.load32(UtrListClearReg::OFFSET), !1u32);
        assert!(!UtrListRunStopReg::read(&utp.regs).value());
        assert!(!HostControllerEnableReg::read(&utp.regs).host_controller_enable());

        assert_eq!(request.await.err(), Some(UfsError::IoError));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert!(!processor.request_list.lock().io_buffer.is_pinned());

        assert_eq!(processor.send_nop_out_upiu().await, Err(UfsError::IoError));
        assert_eq!(processor.init(), Err(UfsError::IoError));
    }

    /// Submits a synchronous TEST UNIT READY on `lun` and returns its outcome receiver.
    fn submit_tur(processor: &TransferRequestProcessor<FakeMmio>, lun: u8) -> IoOutcomeReceiver {
        let (completion, rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        processor.submit_io(lun, cdb, None, completion).unwrap();
        rx
    }

    #[fuchsia::test]
    async fn test_submit_io_completes_through_callback() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // The submitter returns as soon as the doorbell is rung and does not wait.
        let (completion, mut rx) = io_callback();
        let transfer = IoTransfer::read(Arc::clone(&data_vmo), 0, 4096);
        assert_eq!(processor.submit_io(3, cdb, Some(transfer), completion), Ok(()));
        assert_eq!(utp.doorbell(), 1);
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().inflight_io, 1);
        assert!(rx.try_recv().unwrap().is_none());

        let utrd = utp.utrd(0);
        assert_eq!(utrd.data_direction(), DataDirection::DeviceToHost as u32);
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        let command: CommandUpiu = utp.request(0);
        assert_eq!(command.header.lun, 3);
        assert_eq!(command.header.task_tag, 0);
        assert_eq!(command.header.flags, CommandUpiu::FLAG_READ);
        assert_eq!(command.expected_data_transfer_length.get(), 4096);

        // The completion interrupt decodes the response, frees the slot and delivers the outcome.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().inflight_io, 0);
    }

    #[fuchsia::test]
    async fn test_submit_io_reports_failures_without_retrying() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Transient statuses, CHECK CONDITION, OCS and header failures are reported as-is.
        let mut busy = submit_tur(&processor, 0);
        let mut unit_attention = submit_tur(&processor, 0);
        let mut illegal = submit_tur(&processor, 0);
        let mut ocs = submit_tur(&processor, 0);
        let mut wrong_tag = submit_tur(&processor, 0);
        let mut good = submit_tur(&processor, 0);
        assert_eq!(utp.doorbell(), 0b11_1111);
        utp.complete(0, scsi_response(0, ScsiStatus::Busy.as_u8()).as_bytes(), OCS_SUCCESS);
        utp.complete(
            1,
            check_condition(1, SenseKey::UnitAttention.as_u8()).as_bytes(),
            OCS_SUCCESS,
        );
        utp.complete(
            2,
            check_condition(2, SenseKey::IllegalRequest.as_u8()).as_bytes(),
            OCS_SUCCESS,
        );
        utp.complete(3, scsi_response(3, 0).as_bytes(), OverallCommandStatus::Aborted as u8);
        utp.complete(4, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);

        // Each completion interrupt delivered its outcome.
        assert_eq!(delivered(&mut busy), Err(zx::Status::IO));
        assert_eq!(delivered(&mut unit_attention), Err(zx::Status::IO_REFUSED));
        assert_eq!(delivered(&mut illegal), Err(zx::Status::IO_REFUSED));
        assert_eq!(delivered(&mut ocs), Err(zx::Status::IO));
        assert_eq!(delivered(&mut wrong_tag), Err(zx::Status::IO));
        assert_eq!(delivered(&mut good), Ok(()));
        // Nothing was resubmitted.
        assert_eq!(utp.doorbell(), 0);
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
    }

    #[fuchsia::test]
    async fn test_submit_io_rejects_invalid_buffer_and_delivers_error() {
        let (_mmio_vmo, processor) = create_test_processor();
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let (completion, mut rx) = io_callback();
        let unaligned = IoTransfer::read(Arc::clone(&data_vmo), 2, 4096);
        assert_eq!(
            processor.submit_io(0, cdb, Some(unaligned), completion),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert_eq!(processor.request_list.lock().inflight_io, 0);
    }

    #[fuchsia::test]
    async fn test_submit_io_blocks_until_slot_frees() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Fill every I/O slot.
        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();
        assert_eq!(utp.doorbell(), (1u32 << ADMIN_SLOT) - 1);

        // The next submission blocks on its thread until a slot is released.
        let submitted = Arc::new(AtomicBool::new(false));
        let blocked = {
            let processor = Arc::clone(&processor);
            let submitted = Arc::clone(&submitted);
            std::thread::spawn(move || {
                let rx = submit_tur(&processor, 0);
                submitted.store(true, Ordering::SeqCst);
                rx
            })
        };
        assert!(wait_until(|| processor.request_list.lock().blocked_reservers == 1));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!submitted.load(Ordering::SeqCst));

        // Completing slot 5 hands it to the blocked submitter, which rings it again.
        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut inflight[5]), Ok(()));
        let mut rx = blocked.join().unwrap();
        assert!(submitted.load(Ordering::SeqCst));
        assert_eq!(processor.request_list.lock().blocked_reservers, 0);
        assert_eq!(slot_state(&processor, 5), SlotState::Scheduled);
        assert_eq!(utp.doorbell(), (1u32 << ADMIN_SLOT) - 1);

        utp.complete(5, scsi_response(5, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_submit_io_timeout_keeps_buffers_pinned_until_doorbell_clears() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let (completion, mut rx) = io_callback();
        let transfer = IoTransfer::read(Arc::clone(&data_vmo), 0, 4096);
        processor.submit_io(0, cdb, Some(transfer), completion).unwrap();
        assert_ne!(utp.doorbell() & 1, 0);

        // The command timeout aborts the expired request and reports the timeout right away, but
        // must not unpin while the doorbell bit is set.
        expire_deadline(&processor, 0);
        assert_eq!(processor.process_completion_of_io_requests(), 1);
        assert_eq!(utp.regs.load32(UtrListClearReg::OFFSET), !1u32);
        assert_eq!(delivered(&mut rx), Err(zx::Status::TIMED_OUT));
        assert_eq!(slot_state(&processor, 0), SlotState::Timeout);
        assert!(slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().inflight_io, 1);
        assert!(!processor.process_slot_completion(0));

        utp.clear_doorbell(0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().inflight_io, 0);
    }

    #[fuchsia::test]
    async fn test_quiesce_fails_blocked_and_inflight_io() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);

        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();
        let blocked = {
            let processor = Arc::clone(&processor);
            std::thread::spawn(move || {
                let (completion, rx) = io_callback();
                let cdb = ScsiOpcode::build_test_unit_ready_cdb();
                (processor.submit_io(0, cdb, None, completion), rx)
            })
        };
        assert!(wait_until(|| processor.request_list.lock().blocked_reservers == 1));

        assert_eq!(processor.quiesce().await, Ok(()));
        assert_eq!(utp.regs.load32(UtrListClearReg::OFFSET), !((1u32 << ADMIN_SLOT) - 1));
        let (result, mut rx) = blocked.join().unwrap();
        assert_eq!(result, Err(UfsError::IoError));
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        for rx in &mut inflight {
            assert_eq!(delivered(rx), Err(zx::Status::IO));
        }
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
        assert_eq!(processor.request_list.lock().inflight_io, 0);

        // Later synchronous submissions fail immediately.
        let (completion, mut rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert_eq!(processor.submit_io(0, cdb, None, completion), Err(UfsError::IoError));
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_io_timeout_task_aborts_expired_request() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let timeout_task = {
            let processor = Arc::clone(&processor);
            fuchsia_async::Task::local(async move { processor.run_io_timeout_task().await })
        };

        // With nothing in flight the task parks on its waker without arming a timer.
        fuchsia_async::Timer::new(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL)).await;
        let parked = processor.timeout_waker.take().expect("timeout task not parked on waker");
        // Hand the waker back by re-polling the task, which re-registers it.
        parked.wake();

        // The deadline is read when the task is woken by the submission (it shares this test's
        // executor, so it runs only once the test yields); expire it first so the one-shot timer
        // fires immediately and the task aborts the request with UTRLCLR, delivering `TIMED_OUT`
        // without any explicit processing.
        let rx = submit_tur(&processor, 0);
        expire_deadline(&processor, 0);
        assert_eq!(rx.await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert_eq!(utp.regs.load32(UtrListClearReg::OFFSET), !1u32);
        assert_eq!(slot_state(&processor, 0), SlotState::Timeout);

        // `UTRLCLR` raises no interrupt: the task polls the doorbell bit and reclaims the slot
        // once the controller clears it within the abort window, without requesting link recovery.
        utp.clear_doorbell(0);
        while slot_state(&processor, 0) != SlotState::Free {
            fuchsia_async::Timer::new(zx::MonotonicInstant::after(TIMEOUT_RECLAIM_POLL_INTERVAL))
                .await;
        }
        assert_eq!(processor.request_list.lock().inflight_io, 0);
        assert!(!processor.controller().is_recovery_needed());

        // If the controller never clears the doorbell bit after `UTRLCLR`, the timeout task
        // escalates to full link recovery once `TIMEOUT_RECLAIM_WINDOW` elapses.
        let rx = submit_tur(&processor, 0);
        expire_deadline(&processor, 0);
        assert_eq!(rx.await.unwrap(), Err(zx::Status::TIMED_OUT));
        assert!(processor.wait_for_link_recovery().await);
        utp.clear_doorbell(0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        drop(timeout_task);
    }

    #[fuchsia::test]
    async fn test_submit_io_inline_write() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let payload = [0xAAu8; 24];
        let cdb = ScsiOpcode::build_unmap_cdb(payload.len() as u16);

        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(0, cdb, Some(IoTransfer::inline_write(&payload)), completion),
            Ok(())
        );
        assert_eq!(utp.doorbell(), 1);
        assert!(!slot_pinned(&processor, 0));

        let utrd = utp.utrd(0);
        assert_eq!(utrd.data_direction(), DataDirection::HostToDevice as u32);
        assert_eq!((utrd.prdt_offset(), utrd.prdt_length()), (22, 1));
        let ucd_phys = u64::from(utrd.command_descriptor_base_addr)
            | (u64::from(utrd.command_descriptor_base_addr_upper) << 32);
        let prdt_offset = CommandDescriptorLayout::SCSI_COMMAND.prdt_offset();
        let prdt =
            PrdtEntry::read_from_bytes(&utp.read_ucd(0, prdt_offset, size_of::<PrdtEntry>()))
                .unwrap();
        assert_eq!(prdt.dma_address(), ucd_phys + INLINE_DATA_OFFSET as u64);
        assert_eq!(prdt.byte_count(), payload.len() as u32);
        assert_eq!(utp.read_ucd(0, INLINE_DATA_OFFSET, payload.len()), payload);

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx), Ok(()));

        // Non-dword-aligned or oversized inline slices fail before reserving a slot.
        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(0, cdb, Some(IoTransfer::inline_write(&[1u8; 6])), completion),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));

        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(
                0,
                cdb,
                Some(IoTransfer::inline_write(&[1u8; INLINE_DATA_SIZE + 4])),
                completion
            ),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
    }
}
