// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UTP transfer request submission and completion (UFSHCI 3.0/4.0 section 7.2) on the
//! single-doorbell UTRL (`UTRLDBR`).
//!
//! Two submission models share the slot pool:
//!
//! - Synchronous I/O ([`TransferRequestProcessor::submit_io`]) for block requests: the calling
//!   thread validates the transfer, reserves a slot (blocking for pushback when none is free),
//!   pins the buffer and builds the PRDT, writes the UCD/UTRD (DMA descriptors), rings the
//!   doorbell and returns. The completion path is interrupt-driven: the interrupt handler decodes
//!   the response, frees the slot and delivers the outcome through an [`IoCompletion`].
//! - Async UPIUs (`send_scsi_command`, `send_query_request_upiu`, `send_nop_out_upiu`) for
//!   administrative and driver-internal commands, which await the completion interrupt through a
//!   oneshot channel fed by the unified completion path.
//!
//! Slot ownership (`Free -> Reserved -> Scheduled -> Free`): a submitter owns a slot through a
//! [`ReservedSlot`] while `Reserved`. Once scheduled, the controller owns the slot until its
//! doorbell bit clears or `HCE` reads `0`. Client pages are unpinned only after DMA stops (and
//! read caches are invalidated), or quarantined without `unpin()` if `HCE = 0` times out.
//!
//! Lock order: `request_list` -> `controller`.

use crate::constants::{
    ADMIN_COMMAND_SLOT_NUMBER, COMMAND_TIMEOUT, DWORD_ALIGN_MASK, INLINE_DATA_OFFSET,
    INLINE_DATA_SIZE, MAX_PRDT_ENTRIES, MAX_REQUEST_LIST_SIZE, OCS_RETRIES, REGISTER_POLL_INTERVAL,
    SCSI_RETRIES, SCSI_RETRY_DELAY, SDB_SLOTS, SENSE_DESCRIPTOR_FORMAT_CURRENT,
    SENSE_DESCRIPTOR_FORMAT_DEFERRED, SENSE_RESPONSE_CODE_MASK, SMALL_TRANSFER_PAGES,
};
use crate::controller::UfsController;
use crate::dma_buffer::ContiguousDmaBuffer;
use crate::query::QueryUpiu;
use crate::registers::{
    InterruptStatusReg, UtrListBaseAddressReg, UtrListBaseAddressUpperReg, UtrListDoorBellReg,
    UtrListRunStopReg,
};
use crate::request_list::{
    self, DataDirection, OverallCommandStatus, RequestList, SlotCompletion, SlotState,
    TransferRequestDescriptor,
};
use crate::scsi::{ScsiOpcode, ScsiStatus, SenseKey};
use crate::transfer::{self, CommandDescriptorLayout, PrdtEntry};
use crate::upiu::{
    CommandUpiu, NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode,
};
use crate::{UfsError, uic};
use block_server::RequestId;
use fuchsia_sync::{Condvar, Mutex, MutexGuard};
use futures::FutureExt;
use futures::channel::oneshot;
use futures::task::AtomicWaker;
use log::{error, info, warn};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::Poll;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes};

/// Block operation kind and logical byte count recorded in [`BlockIoStats`] on `Ok(())` completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockIoOp {
    Read(u64),
    Write(u64),
    Flush,
    Trim(u64),
}

/// Lock-free block I/O operation and byte counters exported on `root/ufs` via lazy Inspect.
#[derive(Debug, Default)]
pub(crate) struct BlockIoStats {
    read_ops: AtomicU64,
    write_ops: AtomicU64,
    flush_ops: AtomicU64,
    trim_ops: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    trim_bytes: AtomicU64,
}

impl BlockIoStats {
    pub(crate) fn record(&self, op: Option<BlockIoOp>) {
        match op {
            Some(BlockIoOp::Read(bytes)) => {
                self.read_ops.fetch_add(1, Ordering::Relaxed);
                self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Some(BlockIoOp::Write(bytes)) => {
                self.write_ops.fetch_add(1, Ordering::Relaxed);
                self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Some(BlockIoOp::Flush) => {
                self.flush_ops.fetch_add(1, Ordering::Relaxed);
            }
            Some(BlockIoOp::Trim(bytes)) => {
                self.trim_ops.fetch_add(1, Ordering::Relaxed);
                self.trim_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            None => {}
        }
    }
}

/// Bitmask of all slots in the request list (`0..SDB_SLOTS`).
const ALL_SLOTS_MASK: u64 = request_list::slot_mask(SDB_SLOTS as u8);
/// Bitmask of usable I/O slots (`0..ADMIN_COMMAND_SLOT_NUMBER`).
const IO_SLOT_MASK: u64 = request_list::slot_mask(ADMIN_COMMAND_SLOT_NUMBER);

/// Direction of a non-empty [`DataTransfer`] (`NoData` is unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Read,
    Write,
}

impl TransferDirection {
    pub(crate) const fn to_data_direction(self) -> DataDirection {
        match self {
            Self::Read => DataDirection::DeviceToHost,
            Self::Write => DataDirection::HostToDevice,
        }
    }

    fn pin_options(self) -> zx::BtiOptions {
        match self {
            Self::Write => zx::BtiOptions::PERM_READ,
            Self::Read => zx::BtiOptions::PERM_READ | zx::BtiOptions::PERM_WRITE,
        }
    }
}

/// Backing storage for a [`DataTransfer`].
#[derive(Clone, Debug)]
enum TransferBuffer<'a> {
    Vmo(Arc<zx::Vmo>),
    Inline(&'a [u8]),
}

/// Non-empty data phase of a UTP command (`Arc<zx::Vmo>` DMA or inline UCD parameter bytes).
#[derive(Clone, Debug)]
pub struct DataTransfer<'a> {
    buffer: TransferBuffer<'a>,
    offset: u64,
    length: u32,
    logical_bytes: u64,
    direction: TransferDirection,
}

impl DataTransfer<'static> {
    pub fn read(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self {
            buffer: TransferBuffer::Vmo(vmo),
            offset,
            length,
            logical_bytes: u64::from(length),
            direction: TransferDirection::Read,
        }
    }

    pub fn write(vmo: Arc<zx::Vmo>, offset: u64, length: u32) -> Self {
        Self {
            buffer: TransferBuffer::Vmo(vmo),
            offset,
            length,
            logical_bytes: u64::from(length),
            direction: TransferDirection::Write,
        }
    }
}

impl<'a> DataTransfer<'a> {
    pub fn inline_write(bytes: &'a [u8]) -> Self {
        Self {
            buffer: TransferBuffer::Inline(bytes),
            offset: 0,
            length: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            logical_bytes: bytes.len() as u64,
            direction: TransferDirection::Write,
        }
    }

    pub fn inline_unmap(bytes: &'a [u8], unmapped_bytes: u64) -> Self {
        Self { logical_bytes: unmapped_bytes, ..Self::inline_write(bytes) }
    }

    pub fn direction(&self) -> TransferDirection {
        self.direction
    }

    fn pin_range(&self, page_size: u64) -> Result<(u64, u64), UfsError> {
        let start = self.offset & !(page_size - 1);
        let end = self
            .offset
            .checked_add(u64::from(self.length))
            .and_then(|end| end.checked_next_multiple_of(page_size))
            .ok_or(UfsError::InvalidParameter)?;
        Ok((start, end - start))
    }
}

/// Read-buffer range whose CPU data cache must be invalidated after `DeviceToHost` DMA.
#[derive(Debug)]
pub(crate) struct ReadBack {
    vmo: Arc<zx::Vmo>,
    offset: u64,
    length: u32,
}

impl ReadBack {
    fn invalidate_read_cache(&self) -> Result<(), zx::Status> {
        if self.length == 0 {
            return Ok(());
        }
        self.vmo.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, self.offset, u64::from(self.length))
    }
}

/// How the outcome of a synchronous I/O request ([`TransferRequestProcessor::submit_io`]) is
/// delivered.
pub enum IoCompletion {
    /// A block server request, reported to the [`IoCompleter`] installed with
    /// [`TransferRequestProcessor::set_io_completer`]. Carries no per-request allocation.
    Request(RequestId),
    /// Reported through a oneshot channel without boxing a closure.
    Oneshot(oneshot::Sender<Result<(), zx::Status>>),
}

impl std::fmt::Debug for IoCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(id) => f.debug_tuple("Request").field(id).finish(),
            Self::Oneshot(_) => f.write_str("Oneshot"),
        }
    }
}

/// Receives the outcomes of [`IoCompletion::Request`] submissions.
pub trait IoCompleter: Send + Sync {
    /// Reports the outcome of `request_id`, which was submitted for `lun`.
    fn complete(&self, lun: u8, request_id: RequestId, status: Result<(), zx::Status>);
}

struct DeferredSlot {
    read_back: Option<ReadBack>,
    pmt: Option<zx::Pmt>,
    outcome: Option<(SlotCompletion, ScsiOutcome)>,
}

struct Deferred<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    inline: [Option<DeferredSlot>; MAX_REQUEST_LIST_SIZE as usize],
    len: usize,
}

impl<'a, M: Mmio> Deferred<'a, M> {
    fn new(processor: &'a TransferRequestProcessor<M>) -> Self {
        Self { processor, inline: [const { None }; MAX_REQUEST_LIST_SIZE as usize], len: 0 }
    }

    fn push(
        &mut self,
        read_back: Option<ReadBack>,
        pmt: Option<zx::Pmt>,
        outcome: Option<(SlotCompletion, ScsiOutcome)>,
    ) {
        if read_back.is_none() && pmt.is_none() && outcome.is_none() {
            return;
        }
        self.inline[self.len] = Some(DeferredSlot { read_back, pmt, outcome });
        self.len += 1;
    }

    fn drain(&mut self) -> impl Iterator<Item = DeferredSlot> + '_ {
        let len = self.len;
        self.len = 0;
        self.inline[..len].iter_mut().filter_map(Option::take)
    }

    /// Invalidates read caches, unpins PMTs, and delivers sync/async outcomes outside the lock.
    ///
    /// # Safety
    ///
    /// The controller must no longer access any buffer in `self` (doorbell bit 0 or `HCE == 0`).
    unsafe fn complete_after_dma(mut self) {
        let processor = self.processor;
        for DeferredSlot { read_back, pmt, outcome } in self.drain() {
            let cache_err = read_back.and_then(|rb| {
                rb.invalidate_read_cache()
                    .inspect_err(|err| {
                        warn!(err:%; "Failed to invalidate transfer buffer cache after read");
                    })
                    .err()
            });
            // SAFETY: Guaranteed by the caller.
            unsafe { unpin_pmt(pmt) };
            if let Some((completion, outcome)) = outcome {
                processor.deliver_slot_outcome(completion, outcome, cache_err);
            }
        }
    }

    /// Quarantines any pinned `zx::Pmt` handles (dropping without `unpin()`) and delivers `IO`
    /// errors without touching potentially live DMA read buffers.
    fn quarantine_and_deliver(mut self) {
        self.quarantine_remaining();
    }

    fn quarantine_remaining(&mut self) {
        let processor = self.processor;
        for DeferredSlot { read_back: _, pmt, outcome } in self.drain() {
            if let Some(pmt) = pmt {
                processor.quarantined_pmt_count.fetch_add(1, Ordering::Relaxed);
                drop(pmt);
            }
            if let Some((completion, _)) = outcome {
                processor.deliver_slot_outcome(
                    completion,
                    ScsiOutcome::Fail(UfsError::IoError),
                    None,
                );
            }
        }
    }
}

impl<M: Mmio> Drop for Deferred<'_, M> {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        warn!("Deferred completions dropped without DMA stop confirmation; quarantining PMTs");
        self.quarantine_remaining();
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

/// Owns a [`SlotState::Reserved`] slot until `schedule_slot_locked` transitions it to `Scheduled`.
#[must_use = "dropping the guard immediately releases the reserved slot"]
pub(crate) struct ReservedSlot<'a, M: Mmio> {
    processor: &'a TransferRequestProcessor<M>,
    slot: u8,
    armed: bool,
}

impl<M: Mmio> ReservedSlot<'_, M> {
    pub(crate) fn slot(&self) -> u8 {
        self.slot
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<M: Mmio> Drop for ReservedSlot<'_, M> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = self.slot;
        let mut list = self.processor.request_list.lock();
        if let Ok(request_slot) = list.slot(slot) {
            if request_slot.state == SlotState::Reserved {
                debug_assert!(request_slot.pmt.is_none() && request_slot.read_back.is_none());
                self.processor.release_reserved_slot_locked(&mut list, slot);
            }
        }
    }
}

#[must_use = "dropping the guard immediately releases the admin slot and lock"]
struct AdminSlotGuard<'a, M: Mmio> {
    slot: ReservedSlot<'a, M>,
    _lock: futures::lock::MutexGuard<'a, ()>,
}

struct PreparedTransfer<'p, 'a> {
    pmt: Option<zx::Pmt>,
    read_back: Option<ReadBack>,
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
    pub(crate) io_stats: BlockIoStats,
    /// Cumulative `zx::Pmt` handles quarantined without `unpin()` after an `HCE = 0` timeout.
    quarantined_pmt_count: AtomicU64,
    /// Wakes [`Self::run_io_timeout_task`] when a request is scheduled on an idle list or the
    /// processor is stopped.
    timeout_waker: AtomicWaker,
    /// Wakes [`Self::wait_for_link_recovery`] when link recovery is requested or the processor is
    /// stopped.
    recovery_waker: AtomicWaker,
}

impl<M: Mmio> std::fmt::Debug for TransferRequestProcessor<M> {
    /// Reports only the lock-free fields; `request_list` and the controller are not locked so
    /// `fmt` never blocks or deadlocks.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferRequestProcessor")
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
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
            io_stats: BlockIoStats::default(),
            quarantined_pmt_count: AtomicU64::new(0),
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
    ///
    /// Only peeks at the request: the caller consumes it with
    /// [`UfsController::take_recovery_needed`] once it holds the device manager, so a request
    /// raised in between is not lost. The waker is registered before the flag is loaded
    /// (`Acquire`) so a [`Self::request_link_recovery`] racing with the check is not missed.
    pub async fn wait_for_link_recovery(&self) -> bool {
        std::future::poll_fn(|cx| {
            self.recovery_waker.register(cx.waker());
            if self.is_stopped() {
                return Poll::Ready(false);
            }
            if self.controller.is_recovery_needed() {
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

    /// Cumulative `zx::Pmt` handles quarantined without `unpin()` after an `HCE = 0` timeout.
    pub fn quarantined_pmt_count(&self) -> u64 {
        self.quarantined_pmt_count.load(Ordering::Relaxed)
    }

    /// Rejects new slot reservations and wakes blocked/async reservers while leaving `stopped`
    /// clear so in-flight `Scheduled` slots can drain cleanly via completion interrupts.
    pub fn close_submissions(&self) {
        let mut list = self.request_list.lock();
        list.closing = true;
        self.wake_all_slot_waiters(&mut list);
    }

    /// Returns the number of transfer slots (including [`ADMIN_COMMAND_SLOT_NUMBER`]) not `Free`.
    pub(crate) fn busy_slot_count(&self) -> usize {
        let list = self.request_list.lock();
        ((!list.free_mask) & ALL_SLOTS_MASK).count_ones() as usize
    }

    /// Waits up to `timeout` until every transfer slot is [`SlotState::Free`].
    pub async fn drain_inflight(&self, timeout: zx::MonotonicDuration) -> Result<(), UfsError> {
        let res = uic::poll_until_with_interval(timeout, REGISTER_POLL_INTERVAL, || {
            let _ = self.process_slots(ALL_SLOTS_MASK);
            (self.busy_slot_count() == 0).then_some(())
        })
        .await;
        if res.is_err() {
            let _ = self.process_slots(ALL_SLOTS_MASK);
        }
        res
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

    /// Wakes every blocked and async slot reserver so it re-examines the slot pool (or
    /// `stopped` / `closing`).
    fn wake_all_slot_waiters(&self, list: &mut RequestList) {
        self.wake_slot_waiters_for_mask(list, ALL_SLOTS_MASK);
    }

    fn wake_slot_waiters_for_mask(&self, list: &mut RequestList, released_mask: u64) {
        if (released_mask & IO_SLOT_MASK) != 0 && list.blocked_reservers > 0 {
            self.slot_available.notify_all();
        }
        if (released_mask & ALL_SLOTS_MASK) != 0 {
            for waker in list.slot_wakers.drain(..) {
                waker.wake();
            }
        }
    }

    /// Returns `slot` to `Free` without waking waiters and returns its read-back descriptor and
    /// pinned [`zx::Pmt`] so the caller can invalidate caches and unpin outside
    /// `request_list.lock()`.
    fn release_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
    ) -> (Option<ReadBack>, Option<zx::Pmt>) {
        let Some(request_slot) = list.slot_mut(slot).ok() else {
            return (None, None);
        };
        let released = request_slot.release();
        list.set_slot_state(slot, SlotState::Free);
        released
    }

    fn release_reserved_slot_locked(&self, list: &mut RequestList, slot: u8) {
        let _ = self.release_slot_locked(list, slot);
        self.wake_slot_waiters_for_mask(list, 1u64 << slot);
    }

    fn reserved_slot(&self, slot: u8) -> ReservedSlot<'_, M> {
        ReservedSlot { processor: self, slot, armed: true }
    }

    /// Marks the lowest `Free` I/O slot `Reserved` and returns it, if any.
    fn take_free_io_slot_locked(&self, list: &mut RequestList) -> Option<u8> {
        let free_io = list.free_mask & IO_SLOT_MASK;
        if free_io == 0 {
            return None;
        }
        let slot = free_io.trailing_zeros() as u8;
        list.reserve_slot(slot);
        Some(slot)
    }

    /// Reserves an I/O slot for a synchronous request while holding `request_list`, blocking the
    /// calling thread on [`Self::slot_available`] while all are busy (pushback) until a slot is
    /// released by the completion path or the processor closes/stops.
    fn reserve_io_slot_locked(
        &self,
        list: &mut MutexGuard<'_, RequestList>,
    ) -> Result<ReservedSlot<'_, M>, UfsError> {
        loop {
            if list.stopped || list.closing {
                return Err(UfsError::IoError);
            }
            if let Some(slot) = self.take_free_io_slot_locked(list) {
                return Ok(self.reserved_slot(slot));
            }
            list.blocked_reservers += 1;
            self.slot_available.wait(list);
            list.blocked_reservers -= 1;
        }
    }

    /// Reserves an I/O slot for an async request, waiting until one is released by the completion
    /// path.
    async fn reserve_io_slot(&self) -> Result<ReservedSlot<'_, M>, UfsError> {
        std::future::poll_fn(|cx| {
            let mut list = self.request_list.lock();
            if list.stopped || list.closing {
                return Poll::Ready(Err(UfsError::IoError));
            }
            if let Some(slot) = self.take_free_io_slot_locked(&mut list) {
                return Poll::Ready(Ok(self.reserved_slot(slot)));
            }
            if !list.slot_wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                list.slot_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }

    /// Serializes on `admin_lock` and awaits `ADMIN_COMMAND_SLOT_NUMBER == Free` via `slot_wakers`
    /// in case an earlier cancelled admin future is still finishing on hardware.
    async fn reserve_admin_slot(&self) -> Result<AdminSlotGuard<'_, M>, UfsError> {
        let lock = self.admin_lock.lock().await;
        let slot = ADMIN_COMMAND_SLOT_NUMBER;
        let guard = std::future::poll_fn(|cx| {
            let mut list = self.request_list.lock();
            if list.stopped || list.closing {
                return Poll::Ready(Err(UfsError::IoError));
            }
            if list.slot(slot)?.state == SlotState::Free {
                list.reserve_slot(slot);
                return Poll::Ready(Ok(self.reserved_slot(slot)));
            }
            if !list.slot_wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                list.slot_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await?;
        Ok(AdminSlotGuard { slot: guard, _lock: lock })
    }

    /// Validates `transfer` before reserving a slot. Returns `Ok(Some((offset, len)))` for VMO
    /// transfers needing `bti.pin` and `Ok(None)` for inline UCD parameter transfers.
    fn validate_transfer(
        &self,
        transfer: &DataTransfer<'_>,
    ) -> Result<Option<(u64, u64)>, UfsError> {
        match &transfer.buffer {
            TransferBuffer::Vmo(_) => {
                if (transfer.offset & DWORD_ALIGN_MASK) != 0
                    || (u64::from(transfer.length) & DWORD_ALIGN_MASK) != 0
                {
                    return Err(UfsError::InvalidParameter);
                }
                let (offset, length) = transfer.pin_range(self.page_size)?;
                if (length / self.page_size) as usize > MAX_PRDT_ENTRIES {
                    return Err(UfsError::InvalidParameter);
                }
                Ok(Some((offset, length)))
            }
            TransferBuffer::Inline(bytes) => {
                if bytes.is_empty()
                    || bytes.len() > INLINE_DATA_SIZE
                    || !bytes.len().is_multiple_of(4)
                {
                    return Err(UfsError::InvalidParameter);
                }
                Ok(None)
            }
        }
    }

    /// Pins `transfer` outside `request_list.lock()`, cleans/invalidates the CPU cache before DMA,
    /// and invokes `f` with the prepared PRDT and PMT (unpinning if `f` fails before scheduling).
    fn with_prepared_transfer<'a, R>(
        &self,
        transfer: Option<DataTransfer<'a>>,
        f: impl FnOnce(&mut PreparedTransfer<'_, 'a>) -> Result<R, UfsError>,
    ) -> Result<R, UfsError> {
        let page_size = self.page_size;
        let pin_range = transfer.as_ref().map(|t| self.validate_transfer(t)).transpose()?.flatten();
        let inline_data = match &transfer {
            Some(DataTransfer { buffer: TransferBuffer::Inline(bytes), .. }) => Some(*bytes),
            _ => None,
        };
        let page_count = pin_range.map_or(0, |(_, length)| (length / page_size) as usize);

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
        let mut read_back = None;
        let mut prdt_count = 0;
        let mut direction = DataDirection::NoData;
        if let (Some(data), Some((offset, length))) = (transfer, pin_range) {
            let TransferBuffer::Vmo(vmo) = data.buffer else {
                unreachable!("validate_transfer returns Some only for Vmo transfers");
            };
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
            let pinned = self
                .bti
                .pin(data.direction.pin_options(), &vmo, offset, length, phys)
                .map_err(|s| {
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
                TransferDirection::Read => zx::VmoOp::CACHE_CLEAN_INVALIDATE,
                TransferDirection::Write => zx::VmoOp::CACHE_CLEAN,
            };
            if let Err(s) = vmo.op_range(cache_op, data.offset, u64::from(data.length)) {
                error!(status:% = s; "Failed to clean/invalidate transfer buffer cache");
                // SAFETY: The pinned addresses were never written into a descriptor.
                unsafe { unpin_pmt(Some(pinned)) };
                return Err(UfsError::IoError);
            }
            direction = data.direction.to_data_direction();
            if data.direction == TransferDirection::Read && data.length > 0 {
                read_back = Some(ReadBack { vmo, offset: data.offset, length: data.length });
            }
            pmt = Some(pinned);
        } else if inline_data.is_some() {
            direction = DataDirection::HostToDevice;
        }

        let mut prepared = PreparedTransfer {
            pmt,
            read_back,
            prdt: &prdt_table[..prdt_count],
            inline_data,
            direction,
        };
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
        let should_wake_watchdog = list.next_timeout_task_deadline().is_none();
        let RequestList { io_buffer, request_slots, closing, stopped, .. } = &mut *list;
        let request_slot = match request_slots.get_mut(slot as usize) {
            Some(s) if !*stopped && !*closing && s.state == SlotState::Reserved => s,
            Some(_) if *stopped || *closing => return Err(UfsError::IoError),
            Some(_) | None => return Err(UfsError::InvalidSlot(slot as u32)),
        };

        let io_error = |status: zx::Status| {
            error!(slot, status:%; "Failed to write transfer descriptors");
            UfsError::IoError
        };
        let ucd = &mut request_slot.command_descriptor_io;
        ucd.write(0, request).map_err(io_error)?;
        // Zero the response UPIU header (padded to 8-byte alignment so `zero` uses 64-bit stores)
        // so a failed request cannot return a stale response.
        ucd.zero(layout.response_offset(), std::mem::size_of::<UpiuHeader>().next_multiple_of(8))
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

        request_slot.pmt = prepared.pmt.take();
        request_slot.read_back = prepared.read_back.take();
        request_slot.completion = std::mem::take(completion);
        request_slot.deadline = Some(zx::MonotonicInstant::after(COMMAND_TIMEOUT));
        request_slot.stall_logged = false;
        list.set_slot_state(slot, SlotState::Scheduled);
        if should_wake_watchdog {
            self.timeout_waker.wake();
        }
        Ok(())
    }

    /// Pins `data`, fills the UCD and UTRD for `reserved`, rings its doorbell, and returns the
    /// oneshot receiver that yields its [`ScsiOutcome`].
    fn submit(
        &self,
        reserved: &mut ReservedSlot<'_, M>,
        layout: CommandDescriptorLayout,
        request: &[u8],
        data: Option<DataTransfer<'_>>,
        lun: u8,
    ) -> Result<oneshot::Receiver<ScsiOutcome>, UfsError> {
        if request.len() != layout.request_length() {
            return Err(UfsError::InvalidParameter);
        }
        let slot = reserved.slot();
        let (tx, rx) = oneshot::channel();
        let mut completion = SlotCompletion::Scsi { lun, sender: tx };
        self.with_prepared_transfer(data, |prepared| {
            let mut list = self.request_list.lock();
            let scheduled = self.schedule_slot_locked(
                &mut list,
                slot,
                layout,
                request,
                prepared,
                &mut completion,
            );
            if scheduled.is_ok() {
                reserved.disarm();
            }
            scheduled
        })?;
        Ok(rx)
    }

    /// Submits a SCSI command to `lun` synchronously and returns as soon as its doorbell has been
    /// rung. Blocks the calling thread while all I/O slots are busy (pushback). The outcome is
    /// delivered through `completion` from the completion path once the controller finishes or is
    /// quiesced (`IO`); transient SCSI statuses are reported, not retried.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` for a misaligned or oversized buffer (validated before reserving
    /// a slot), or `IoError` on a pin failure or after [`Self::close_submissions`] /
    /// [`Self::quiesce`]. On error `completion` has already been delivered with the matching
    /// `zx::Status`.
    pub fn submit_io(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
        completion: IoCompletion,
    ) -> Result<(), UfsError> {
        let data = data.filter(|d| !matches!(d.buffer, TransferBuffer::Vmo(_)) || d.length != 0);
        if let Some(ref transfer) = data {
            if let Err(e) = self.validate_transfer(transfer) {
                self.complete_io(lun, completion, Err(zx::Status::from(e)));
                return Err(e);
            }
        }
        let expected_transfer_len = data.as_ref().map_or(0, |data| data.length);
        let direction =
            data.as_ref().map_or(DataDirection::NoData, |data| data.direction.to_data_direction());
        let io_op = match cdb[0] {
            x if x == ScsiOpcode::Read10 as u8 || x == ScsiOpcode::Read16 as u8 => {
                Some(BlockIoOp::Read(u64::from(expected_transfer_len)))
            }
            x if x == ScsiOpcode::Write10 as u8 || x == ScsiOpcode::Write16 as u8 => {
                Some(BlockIoOp::Write(u64::from(expected_transfer_len)))
            }
            x if x == ScsiOpcode::SynchronizeCache10 as u8 => Some(BlockIoOp::Flush),
            x if x == ScsiOpcode::Unmap as u8 => {
                Some(BlockIoOp::Trim(data.as_ref().map_or(0, |d| d.logical_bytes)))
            }
            _ => None,
        };

        // Reserve (and possibly block for) the slot before pinning, so pushback never holds
        // client pages pinned while waiting.
        let mut reserved = {
            let mut list = self.request_list.lock();
            match self.reserve_io_slot_locked(&mut list) {
                Ok(reserved) => reserved,
                Err(e) => {
                    drop(list);
                    self.complete_io(lun, completion, Err(zx::Status::from(e)));
                    return Err(e);
                }
            }
        };
        let slot = reserved.slot();
        let mut command = CommandUpiu::new(cdb, expected_transfer_len, direction);
        command.header.lun = lun;
        command.header.task_tag = slot;
        let mut completion = Some(completion);

        let result = self.with_prepared_transfer(data, |prepared| {
            let mut list = self.request_list.lock();
            let mut slot_completion = SlotCompletion::Io {
                lun,
                completion: completion
                    .take()
                    .expect("with_prepared_transfer invokes its closure at most once"),
                io_op,
            };
            let scheduled = self.schedule_slot_locked(
                &mut list,
                slot,
                CommandDescriptorLayout::SCSI_COMMAND,
                command.as_bytes(),
                prepared,
                &mut slot_completion,
            );
            match scheduled {
                Ok(()) => reserved.disarm(),
                Err(_) => {
                    if let SlotCompletion::Io { completion: c, .. } = slot_completion {
                        completion = Some(c);
                    }
                }
            }
            scheduled
        });
        drop(reserved);
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
            IoCompletion::Oneshot(sender) => {
                let _ = sender.send(status);
            }
        }
    }

    fn deliver_slot_outcome(
        &self,
        completion: SlotCompletion,
        outcome: ScsiOutcome,
        cache_err: Option<zx::Status>,
    ) {
        match completion {
            SlotCompletion::None => {}
            SlotCompletion::Io { lun, completion, io_op } => {
                let mut status = outcome.into_io_status();
                if let Some(err) = cache_err
                    && status.is_ok()
                {
                    status = Err(err);
                }
                if status.is_ok() {
                    self.io_stats.record(io_op);
                }
                self.complete_io(lun, completion, status);
            }
            SlotCompletion::Scsi { sender, .. } => {
                let outcome = match outcome {
                    ScsiOutcome::Good if cache_err.is_some() => {
                        ScsiOutcome::Fail(UfsError::IoError)
                    }
                    other => other,
                };
                let _ = sender.send(outcome);
            }
        }
    }

    /// Processes any already-clear doorbells, then logs each `Scheduled` slot past
    /// [`COMMAND_TIMEOUT`] at most once while leaving its slot state and `zx::Pmt` intact.
    fn log_stalled_slots(&self) {
        let _ = self.process_slots(ALL_SLOTS_MASK);
        let now = zx::MonotonicInstant::get();
        let mut list = self.request_list.lock();
        if list.stopped {
            return;
        }
        let mut remaining = list.scheduled_mask;
        while remaining != 0 {
            let slot = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1;
            let Ok(request_slot) = list.slot_mut(slot) else {
                continue;
            };
            if request_slot.state == SlotState::Scheduled
                && !request_slot.stall_logged
                && request_slot.deadline.is_some_and(|deadline| now >= deadline)
            {
                request_slot.stall_logged = true;
                error!(
                    slot;
                    "UTP transfer slot still scheduled past COMMAND_TIMEOUT; leaving DMA pinned"
                );
            }
        }
    }

    /// Diagnostic stall watchdog for in-flight `Scheduled` slots; exits promptly on `quiesce()`.
    pub async fn run_io_timeout_task(&self) {
        loop {
            let Some(wake_at) = std::future::poll_fn(|cx| {
                self.timeout_waker.register(cx.waker());
                let list = self.request_list.lock();
                if list.stopped {
                    return Poll::Ready(None);
                }
                match list.next_timeout_task_deadline() {
                    Some(wake_at) => Poll::Ready(Some(wake_at)),
                    None => Poll::Pending,
                }
            })
            .await
            else {
                return;
            };
            let mut timer = std::pin::pin!(fuchsia_async::Timer::new(wake_at).fuse());
            let mut interrupted = std::pin::pin!(
                std::future::poll_fn(|cx| {
                    self.timeout_waker.register(cx.waker());
                    let list = self.request_list.lock();
                    if list.stopped {
                        return Poll::Ready(true);
                    }
                    match list.next_timeout_task_deadline() {
                        Some(earlier) if earlier < wake_at => Poll::Ready(false),
                        Some(_) | None => Poll::Pending,
                    }
                })
                .fuse()
            );
            let stopped = futures::select_biased! {
                stopped = interrupted => stopped,
                () = timer => false,
            };
            if stopped {
                return;
            }
            self.log_stalled_slots();
        }
    }

    /// Submits an administrative UPIU on `admin`'s slot, awaits completion while holding
    /// `AdminSlotGuard`, and reads the validated response UPIU directly from the admin UCD.
    async fn execute_admin<R: FromBytes + IntoBytes + Immutable>(
        &self,
        admin: &mut AdminSlotGuard<'_, M>,
        layout: CommandDescriptorLayout,
        request: &[u8],
        expected_response: UpiuTransactionCode,
    ) -> Result<R, UfsError> {
        debug_assert_eq!(std::mem::size_of::<R>(), layout.response_length());
        let slot = admin.slot.slot();
        let rx = self.submit(&mut admin.slot, layout, request, None, 0)?;
        match rx.await.map_err(|_| UfsError::IoError)? {
            ScsiOutcome::Good => {}
            ScsiOutcome::Retry { error, .. } | ScsiOutcome::Fail(error) => return Err(error),
        }
        let response: R = {
            let list = self.request_list.lock();
            if list.stopped {
                return Err(UfsError::IoError);
            }
            list.slot(slot)?
                .command_descriptor_io
                .read_struct(layout.response_offset())
                .map_err(|_| UfsError::IoError)?
        };
        check_response_header(response.as_bytes(), expected_response, slot)?;
        Ok(response)
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

    /// Decodes the RESPONSE UPIU of a finished SCSI command in `slot`'s UCD (`slots 0..30`).
    fn decode_io_response(list: &RequestList, slot: u8, lun: u8) -> ScsiOutcome {
        let Ok(request_slot) = list.slot(slot) else {
            return ScsiOutcome::Fail(UfsError::InvalidSlot(u32::from(slot)));
        };
        let Ok((response, header)) = read_scsi_response(
            &request_slot.command_descriptor_io,
            CommandDescriptorLayout::SCSI_COMMAND.response_offset(),
        ) else {
            return ScsiOutcome::Fail(UfsError::IoError);
        };
        if let Err(err) =
            check_response_header(header.as_bytes(), UpiuTransactionCode::Response, slot)
        {
            return ScsiOutcome::Fail(err);
        }
        check_scsi_status(lun, &response)
    }

    /// Decodes the slot's [`ScsiOutcome`], releases `slot` to `Free`, and pushes read-back cache
    /// invalidation, `zx::Pmt` unpinning, and outcome delivery to `deferred` for execution outside
    /// `request_list.lock()`.
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
        let completion = std::mem::take(&mut request_slot.completion);
        let outcome = match completion {
            SlotCompletion::None => None,
            SlotCompletion::Io { lun, .. } | SlotCompletion::Scsi { lun, .. } => {
                let scsi_outcome = match result {
                    Ok(()) if slot == ADMIN_COMMAND_SLOT_NUMBER => ScsiOutcome::Good,
                    Ok(()) => Self::decode_io_response(list, slot, lun),
                    Err(err) => ScsiOutcome::Fail(err),
                };
                Some((completion, scsi_outcome))
            }
        };
        let (read_back, pmt) = self.release_slot_locked(list, slot);
        deferred.push(read_back, pmt, outcome);
    }

    /// Completes `slot` if it is `Scheduled` and its `doorbell` bit has cleared.
    fn process_sdb_slot_locked(
        &self,
        list: &mut RequestList,
        slot: u8,
        doorbell: u32,
        deferred: &mut Deferred<'_, M>,
    ) -> bool {
        if slot as usize >= SDB_SLOTS {
            return false;
        }
        let Ok(request_slot) = list.slot(slot) else {
            return false;
        };
        let outstanding = doorbell & (1 << slot) != 0;
        match (request_slot.state, outstanding) {
            (SlotState::Scheduled, false) => {
                let ocs = self.read_ocs(list, slot);
                let result = match OverallCommandStatus::from_raw(ocs) {
                    Some(OverallCommandStatus::Success) => Ok(()),
                    Some(OverallCommandStatus::Invalid) => {
                        error!(slot; "Slot doorbell cleared with OCS still Invalid (0x0F)");
                        Err(UfsError::IoError)
                    }
                    status => {
                        error!(slot, ocs, status:?; "Slot failed with OCS error");
                        Err(UfsError::IoError)
                    }
                };
                self.finish_slot_locked(list, slot, result, deferred);
                true
            }
            (SlotState::Scheduled, true) | (SlotState::Free | SlotState::Reserved, _) => false,
        }
    }

    /// Processes completions of `slot_mask`, then invalidates read caches, unpins PMTs, and
    /// delivers outcomes after releasing `request_list`.
    fn process_slots(&self, slot_mask: u64) -> u32 {
        let mut deferred = Deferred::new(self);
        let completed = {
            let mut list = self.request_list.lock();
            self.process_slots_locked(&mut list, slot_mask, &mut deferred)
        };
        // SAFETY: Every slot pushed into `deferred` had its doorbell bit observed clear (`0`).
        unsafe { deferred.complete_after_dma() };
        completed
    }

    /// Processes completions of `slot_mask` while holding `request_list`.
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
        let mut completed = 0;
        let mut remaining = active;
        while remaining != 0 {
            let slot = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1;
            if self.process_sdb_slot_locked(list, slot, doorbell, deferred) {
                completed += 1;
            }
        }
        let released_mask = (list.free_mask & !free_before) & ALL_SLOTS_MASK;
        if released_mask != 0 {
            self.wake_slot_waiters_for_mask(list, released_mask);
        }
        completed
    }

    /// Processes a possible completion of `slot`. Returns whether it completed.
    pub fn process_slot_completion(&self, slot: u8) -> bool {
        (slot as usize) < SDB_SLOTS && self.process_slots(1u64 << slot) > 0
    }

    /// Processes all I/O slots. Returns the number of slots that completed.
    pub fn process_completion_of_io_requests(&self) -> u32 {
        self.process_slots(IO_SLOT_MASK)
    }

    /// Services an interrupt: clears and logs the handled `IS` bits, and processes SDB
    /// completions. Status is cleared before the state it reports is read, so concurrent calls
    /// lose no event. Returns `IS` as read.
    ///
    /// The `stopped` check is needed here because [`UfsController::fetch_and_clear_interrupts`]
    /// accesses the register block unconditionally.
    pub fn service_interrupt(&self) -> InterruptStatusReg {
        if self.is_stopped() {
            return InterruptStatusReg::default();
        }
        let status = self.controller.fetch_and_clear_interrupts();
        if self.controller.is_recovery_needed() {
            self.recovery_waker.wake();
        }
        if status.utp_transfer_request_completion_status() {
            self.process_slots(ALL_SLOTS_MASK);
        }
        status
    }

    /// Sends a QUERY REQUEST UPIU on the admin slot and returns the QUERY RESPONSE UPIU.
    ///
    /// The response's `Query Response` code is not checked; callers interpret it.
    pub async fn send_query_request_upiu(
        &self,
        query_request: &QueryUpiu,
    ) -> Result<QueryUpiu, UfsError> {
        let mut admin = self.reserve_admin_slot().await?;
        let mut request = *query_request;
        request.header.task_tag = admin.slot.slot();
        self.execute_admin(
            &mut admin,
            CommandDescriptorLayout::QUERY,
            request.as_bytes(),
            UpiuTransactionCode::QueryResponse,
        )
        .await
    }

    /// Sends a NOP OUT UPIU on the admin slot and checks the NOP IN response.
    pub async fn send_nop_out_upiu(&self) -> Result<(), UfsError> {
        let mut admin = self.reserve_admin_slot().await?;
        let mut nop = NopOutUpiu::new();
        nop.header.task_tag = admin.slot.slot();
        let response: NopInUpiu = self
            .execute_admin(
                &mut admin,
                CommandDescriptorLayout::NOP,
                nop.as_bytes(),
                UpiuTransactionCode::NopIn,
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
    pub async fn send_scsi_command(
        &self,
        lun: u8,
        cdb: [u8; 16],
        data: Option<DataTransfer<'_>>,
    ) -> Result<(), UfsError> {
        let data = data.filter(|d| !matches!(d.buffer, TransferBuffer::Vmo(_)) || d.length != 0);
        if let Some(ref transfer) = data {
            self.validate_transfer(transfer)?;
        }
        let expected_transfer_len = data.as_ref().map_or(0, |data| data.length);
        let direction =
            data.as_ref().map_or(DataDirection::NoData, |data| data.direction.to_data_direction());
        let mut retries = 0;
        loop {
            let mut reserved = self.reserve_io_slot().await?;
            let mut command = CommandUpiu::new(cdb, expected_transfer_len, direction);
            command.header.lun = lun;
            command.header.task_tag = reserved.slot();
            let rx = self.submit(
                &mut reserved,
                CommandDescriptorLayout::SCSI_COMMAND,
                command.as_bytes(),
                data.clone(),
                lun,
            )?;
            drop(reserved);
            match rx.await.map_err(|_| UfsError::IoError)? {
                ScsiOutcome::Good => return Ok(()),
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
    /// (synchronous I/O completes with `zx::Status::IO`, async requests with `UfsError::IoError`),
    /// wakes blocked submitters, and unpins all DMA memory (or quarantines PMTs if `HCE` fails to
    /// clear). Later submissions fail with `IoError`.
    pub async fn quiesce(&self) -> Result<(), UfsError> {
        {
            let mut list = self.request_list.lock();
            list.closing = true;
            list.stopped = true;
            self.controller.with_mmio(|mmio| {
                UtrListRunStopReg::from_raw(0).write(mmio);
            });
            self.wake_all_slot_waiters(&mut list);
            self.timeout_waker.wake();
            self.recovery_waker.wake();
        }
        let disable_res = self.controller.disable_host_controller().await.inspect_err(|e| {
            error!(err:? = e; "Failed to disable host controller; quarantining DMA memory");
        });

        let mut deferred = Deferred::new(self);
        {
            let mut list = self.request_list.lock();
            let mut remaining = list.scheduled_mask();
            while remaining != 0 {
                let slot = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                self.finish_slot_locked(&mut list, slot, Err(UfsError::IoError), &mut deferred);
            }
            if disable_res.is_ok() {
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
        }
        if disable_res.is_ok() {
            // SAFETY: `HCE` reads 0, so the controller performs no DMA.
            unsafe { deferred.complete_after_dma() };
        } else {
            deferred.quarantine_and_deliver();
        }
        disable_res
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

/// Classification of a UTP command outcome (and SCSI RESPONSE UPIU on slots `0..30`).
#[derive(Debug)]
pub(crate) enum ScsiOutcome {
    /// GOOD status and no transport-level response error.
    Good,
    /// A transient failure (UNIT ATTENTION, NOT READY, BUSY, TASK SET FULL) worth retrying.
    Retry { error: UfsError, delay: bool },
    /// A failure that retrying will not fix.
    Fail(UfsError),
}

impl ScsiOutcome {
    fn into_io_status(self) -> Result<(), zx::Status> {
        match self {
            Self::Good => Ok(()),
            Self::Retry { error, .. } | Self::Fail(error) => Err(zx::Status::from(error)),
        }
    }
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
        scsi_response, slot_pinned, slot_state, wait_async, wait_for_any_doorbell,
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

        // Dropped after the completion interrupt was delivered: `finish_slot_locked` already
        // released the slot immediately upon completion.
        let mut request = Box::pin(processor.send_nop_out_upiu());
        assert!(poll!(request.as_mut()).is_pending());
        utp.complete(ADMIN_SLOT, nop_in(ADMIN_SLOT).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        drop(request);
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);

        // Dropped while the controller still owns the slot: the completion interrupt frees it,
        // invalidates read caches, and unpins the orphaned buffer.
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
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
    async fn test_ocs_invalid_after_doorbell_clear() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        // Async UPIU: doorbell bit cleared while `OCS` stays `0x0F` fails immediately with
        // `Err(UfsError::IoError)` and releases the slot.
        let mut request = Box::pin(processor.send_nop_out_upiu());
        assert!(poll!(request.as_mut()).is_pending());
        utp.write_ucd(ADMIN_SLOT, NOP_RESPONSE_OFFSET, nop_in(ADMIN_SLOT).as_bytes());
        utp.clear_doorbell(ADMIN_SLOT);
        assert!(processor.process_slot_completion(ADMIN_SLOT));
        assert_eq!(slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        assert_eq!(request.await, Err(UfsError::IoError));

        // Sync I/O: doorbell bit cleared while `OCS` stays `0x0F` delivers `Err(zx::Status::IO)`.
        let mut rx = submit_tur(&processor, 0);
        utp.clear_doorbell(0);
        assert!(processor.process_slot_completion(0));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_scsi_command_integration() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let data_vmo_clone = Arc::clone(&data_vmo);

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
        processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 36)))
            .await
            .unwrap();

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
        let data_vmo = Arc::new(zx::Vmo::create(3 * PAGE_SIZE).unwrap());

        let hardware = std::thread::spawn(move || {
            let slot = wait_for_any_doorbell(&utp);
            let utrd = utp.utrd(slot);
            let command: crate::upiu::CommandUpiu = utp.request(slot);
            utp.complete(slot, scsi_response(slot, 0).as_bytes(), OCS_SUCCESS);
            (utrd, command)
        });

        let cdb = ScsiOpcode::build_write_10_cdb(0, 2);
        processor
            .send_scsi_command(0, cdb, Some(DataTransfer::write(Arc::clone(&data_vmo), 4096, 8192)))
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
        let data_vmo = Arc::new(zx::Vmo::create(258 * PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);
        let err = Some(UfsError::InvalidParameter);

        let unaligned = processor
            .send_scsi_command(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 2, 4096)))
            .await;
        assert_eq!(unaligned.err(), err);
        let too_large = processor
            .send_scsi_command(
                0,
                cdb,
                Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 257 * 4096)),
            )
            .await;
        assert_eq!(too_large.err(), err);
        let overflow = processor
            .send_scsi_command(
                0,
                cdb,
                Some(DataTransfer::read(Arc::clone(&data_vmo), u64::MAX - 8, 4096)),
            )
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
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let mut request = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(request.as_mut()).is_pending());

        assert_eq!(processor.quiesce().await, Ok(()));
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
        let transfer = DataTransfer::read(Arc::clone(&data_vmo), 0, 4096);
        assert_eq!(processor.submit_io(3, cdb, Some(transfer), completion), Ok(()));
        assert_eq!(utp.doorbell(), 1);
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        assert_eq!(processor.request_list.lock().scheduled_mask.count_ones(), 1);
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
        assert_eq!(processor.request_list.lock().scheduled_mask.count_ones(), 0);
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
    async fn test_submit_io_rejects_invalid_transfer_without_reserving_slot() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // Fill every I/O slot (`0..ADMIN_SLOT`) so `reserve_io_slot_locked` would block on Condvar
        // if `submit_io` attempted reservation before validating `data`.
        let mut inflight: Vec<_> = (0..ADMIN_SLOT).map(|_| submit_tur(&processor, 0)).collect();

        let (completion, mut rx) = io_callback();
        let unaligned = DataTransfer::read(Arc::clone(&data_vmo), 2, 4096);
        assert_eq!(
            processor.submit_io(0, cdb, Some(unaligned), completion),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
        assert_eq!(processor.request_list.lock().blocked_reservers, 0);

        assert_eq!(processor.quiesce().await, Ok(()));
        for rx in &mut inflight {
            assert_eq!(delivered(rx), Err(zx::Status::IO));
        }
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
    async fn test_quiesce_fails_blocked_and_inflight_io() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);

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
        let (result, mut rx) = blocked.join().unwrap();
        assert_eq!(result, Err(UfsError::IoError));
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        for rx in &mut inflight {
            assert_eq!(delivered(rx), Err(zx::Status::IO));
        }
        for slot in 0..ADMIN_SLOT {
            assert_eq!(slot_state(&processor, slot), SlotState::Free);
        }
        assert_eq!(processor.request_list.lock().scheduled_mask.count_ones(), 0);

        // Later synchronous submissions fail immediately.
        let (completion, mut rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert_eq!(processor.submit_io(0, cdb, None, completion), Err(UfsError::IoError));
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    async fn test_submit_io_inline_write() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let payload = [0xAAu8; 24];
        let cdb = ScsiOpcode::build_unmap_cdb(payload.len() as u16);

        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(0, cdb, Some(DataTransfer::inline_write(&payload)), completion),
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

        // Non-dword-aligned or oversized inline slices fail and release the reserved slot.
        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(0, cdb, Some(DataTransfer::inline_write(&[1u8; 6])), completion),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));

        let (completion, mut rx) = io_callback();
        assert_eq!(
            processor.submit_io(
                0,
                cdb,
                Some(DataTransfer::inline_write(&[1u8; INLINE_DATA_SIZE + 4])),
                completion
            ),
            Err(UfsError::InvalidParameter)
        );
        assert_eq!(delivered(&mut rx), Err(zx::Status::INVALID_ARGS));
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
    }

    #[fuchsia::test]
    async fn test_io_timeout_task_wakes_promptly_on_quiesce() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let finished = Arc::new(AtomicBool::new(false));
        let timeout_task = {
            let processor = Arc::clone(&processor);
            let finished = Arc::clone(&finished);
            fuchsia_async::Task::local(async move {
                processor.run_io_timeout_task().await;
                finished.store(true, Ordering::SeqCst);
            })
        };

        // An in-flight request parks the task on a timer at its `COMMAND_TIMEOUT` deadline.
        let mut rx = submit_tur(&processor, 0);
        fuchsia_async::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(
            10,
        )))
        .await;
        assert!(!finished.load(Ordering::SeqCst));

        // Quiescing cuts that timer short instead of letting it run out.
        assert_eq!(processor.quiesce().await, Ok(()));
        assert!(wait_async(|| finished.load(Ordering::SeqCst)).await);
        assert_eq!(delivered(&mut rx), Err(zx::Status::IO));
        drop(timeout_task);
    }

    #[fuchsia::test]
    async fn test_async_and_sync_read_completion_unifies_cache_invalidation_and_pmt_unpin() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // 1. Async read: `finish_slot_locked` + `Deferred::complete_after_dma` frees the slot and
        //    unpins the `zx::Pmt` inside `utp.complete(...)` BEFORE the async future is polled.
        let mut async_req = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(async_req.as_mut()).is_pending());
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert!(async_req.await.is_ok());

        // 2. Sync read: same `finish_slot_locked` + `Deferred::complete_after_dma` path.
        let (completion, mut rx) = io_callback();
        processor
            .submit_io(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)), completion)
            .unwrap();
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(delivered(&mut rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_stalled_slot_logs_once_and_completes_cleanly_on_late_irq() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        let timeout_task = {
            let processor = Arc::clone(&processor);
            fuchsia_async::Task::local(async move { processor.run_io_timeout_task().await })
        };

        let (completion, mut rx) = io_callback();
        processor
            .submit_io(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)), completion)
            .unwrap();
        expire_deadline(&processor, 0);
        processor.timeout_waker.wake();

        assert!(wait_async(|| processor.request_list.lock().slot(0).unwrap().stall_logged).await);
        // Slot stays `Scheduled` with its `zx::Pmt` pinned while hardware still owns it.
        assert_eq!(slot_state(&processor, 0), SlotState::Scheduled);
        assert!(slot_pinned(&processor, 0));

        // Late completion interrupt frees the slot, unpins the PMT, and delivers `Ok(())`.
        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(slot_state(&processor, 0), SlotState::Free);
        assert!(!slot_pinned(&processor, 0));
        assert_eq!(delivered(&mut rx), Ok(()));

        assert_eq!(processor.quiesce().await, Ok(()));
        timeout_task.await;
    }

    #[fuchsia::test]
    fn test_stall_watchdog_monitors_new_slot_while_prior_slot_stalled() {
        let mut exec = fuchsia_async::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fuchsia_async::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));

        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let mut watchdog = std::pin::pin!(processor.run_io_timeout_task());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(fuchsia_async::TestExecutor::next_timer(), None);

        // 1. Schedule slot 0 and let its `COMMAND_TIMEOUT` deadline expire: the watchdog logs slot
        //    0 once (`stall_logged == true`), leaves it `Scheduled`, and parks with no timer armed.
        let mut rx0 = submit_tur(&processor, 0);
        let d0 = processor.request_list.lock().slot(0).unwrap().deadline.unwrap();
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(
            fuchsia_async::TestExecutor::next_timer(),
            Some(fuchsia_async::MonotonicInstant::from_zx(d0))
        );

        expire_deadline(&processor, 0);
        exec.set_fake_time(fuchsia_async::MonotonicInstant::from_zx(d0));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        {
            let list = processor.request_list.lock();
            assert!(list.slot(0).unwrap().stall_logged);
            assert_eq!(list.scheduled_mask, 0b01);
            assert_eq!(list.next_timeout_task_deadline(), None);
        }
        assert_eq!(fuchsia_async::TestExecutor::next_timer(), None);

        // 2. While slot 0 remains `Scheduled` with `stall_logged == true`, scheduling slot 1 must
        //    wake `timeout_waker` (`next_timeout_task_deadline().is_none()`) to monitor slot 1.
        let mut rx1 = submit_tur(&processor, 0);
        let d1 = processor.request_list.lock().slot(1).unwrap().deadline.unwrap();
        {
            let list = processor.request_list.lock();
            assert_eq!(list.scheduled_mask, 0b11);
            assert!(!list.slot(1).unwrap().stall_logged);
        }
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert_eq!(
            fuchsia_async::TestExecutor::next_timer(),
            Some(fuchsia_async::MonotonicInstant::from_zx(d1))
        );

        expire_deadline(&processor, 1);
        exec.set_fake_time(fuchsia_async::MonotonicInstant::from_zx(d1));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut watchdog).is_pending());
        assert!(processor.request_list.lock().slot(1).unwrap().stall_logged);

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        utp.complete(1, scsi_response(1, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(delivered(&mut rx0), Ok(()));
        assert_eq!(delivered(&mut rx1), Ok(()));

        let mut quiesce = std::pin::pin!(processor.quiesce());
        assert_eq!(exec.run_until_stalled(&mut quiesce), Poll::Ready(Ok(())));
        assert_eq!(exec.run_until_stalled(&mut watchdog), Poll::Ready(()));
    }

    #[fuchsia::test]
    async fn test_close_submissions_and_drain_inflight() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);

        let mut inflight_rx = submit_tur(&processor, 0);
        assert_eq!(processor.busy_slot_count(), 1);

        processor.close_submissions();

        // New sync and async submissions are rejected immediately while in-flight slots drain.
        let (completion, mut rejected_rx) = io_callback();
        let cdb = ScsiOpcode::build_test_unit_ready_cdb();
        assert_eq!(processor.submit_io(0, cdb, None, completion), Err(UfsError::IoError));
        assert_eq!(delivered(&mut rejected_rx), Err(zx::Status::IO));
        assert_eq!(processor.send_nop_out_upiu().await, Err(UfsError::IoError));

        let hardware = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        });
        assert_eq!(processor.drain_inflight(zx::MonotonicDuration::from_seconds(1)).await, Ok(()));
        hardware.join().unwrap();
        assert_eq!(processor.busy_slot_count(), 0);
        assert_eq!(delivered(&mut inflight_rx), Ok(()));
    }

    #[fuchsia::test]
    async fn test_quiesce_quarantines_pmt_and_delivers_io_error_when_disable_fails() {
        let (mmio_vmo, processor) = create_test_processor();
        let _utp = fake_utp(&mmio_vmo, &processor);
        let data_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        let cdb = ScsiOpcode::build_read_10_cdb(0, 1);

        // Set `HCE = 1` and freeze `HCE` so `disable_host_controller()` times out.
        processor.controller().with_mmio(|mmio| {
            HostControllerEnableReg::from_raw(1).write(mmio);
            mmio.set_hce_stuck(true);
        });
        processor
            .controller()
            .set_host_controller_timeout_for_test(zx::MonotonicDuration::from_millis(5));

        let mut async_req = Box::pin(processor.send_scsi_command(
            0,
            cdb,
            Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)),
        ));
        assert!(poll!(async_req.as_mut()).is_pending());

        let (completion, mut sync_rx) = io_callback();
        processor
            .submit_io(0, cdb, Some(DataTransfer::read(Arc::clone(&data_vmo), 0, 4096)), completion)
            .unwrap();
        assert!(slot_pinned(&processor, 0) && slot_pinned(&processor, 1));

        assert_eq!(processor.quiesce().await, Err(UfsError::Timeout));
        assert_eq!(async_req.await, Err(UfsError::IoError));
        assert_eq!(delivered(&mut sync_rx), Err(zx::Status::IO));
        assert_eq!(processor.quarantined_pmt_count(), 2);
        assert!(processor.request_list.lock().io_buffer.is_pinned());
        assert!(processor.request_list.lock().slot(0).unwrap().command_descriptor_io.is_pinned());
    }
}
